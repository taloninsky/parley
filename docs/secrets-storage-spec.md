# Secrets Storage — Specification

> Status: **Draft**
> Author: Gavin + BigDog
> Date: 2026-04-22

---

## 1. Overview

Provider API keys (Anthropic, AssemblyAI, and any future providers) are currently stored as plaintext browser cookies on `localhost`, read by the WASM UI, and forwarded to the local `parley-proxy` on every request. This is insecure and architecturally inverted: the proxy is the only component that actually needs to talk to the providers, but the browser is the one holding the secret.

This spec replaces that model. **The proxy owns provider credentials.** They are persisted in the OS-native secret store (Windows Credential Manager / macOS Keychain) via the [`keyring`](https://crates.io/crates/keyring) crate. The UI never sees, stores, or transmits provider keys after the user enters them; it only displays per-provider configuration status and submits new values to a local proxy endpoint.

This is the standard practice used by `gh`, `aws-vault`, `cargo login`, and `npm`.

---

## 2. Goals

- Provider keys at rest are protected by the OS user account (DPAPI on Windows, Keychain ACL on macOS) — not readable by other OS users without privilege escalation.
- Provider keys are not reachable from the WASM bundle. An XSS-class compromise of the UI cannot exfiltrate them.
- Provider keys are entered exactly once per device and survive across UI reloads, port changes, and proxy restarts.
- The UI presents an unambiguous "configured / not configured" status for each provider, plus a single affordance to set, replace, or remove a key.
- The change aligns with Philosophy §6 ("Models come and go") and §8 ("The core is the product"): credentials are a property of the proxy/core, not the rendering layer.

## 3. Non-Goals

- Defending against an attacker already executing code as the logged-in OS user. The proxy can read what the user can read; this is by design and matches every comparable tool.
- Linux desktop support is *not in scope to test* this pass, but the chosen library (`keyring`) handles Secret Service on Linux desktops with no API change. Headless Linux (no D-Bus, no GNOME Keyring / KWallet) is a separate problem and remains out of scope.
- An encrypted-file fallback for headless / no-keystore environments. Out of scope; revisit if a real use case appears.
- Per-key usage metering, expiry tracking, or rotation reminders.
- **Provider/model configuration** (which model slug to use per provider, default-model selection, enabling/disabling providers). That is plain non-secret config and belongs in a separate spec; see §10.2.
- Programmatic integration with external password managers (1Password CLI, Bitwarden CLI, LastPass). The user is encouraged to manually back up keys to whatever password manager they prefer; Parley does not read from one. The `KeyStore` trait introduced in §7 leaves room for a future backend if we decide we want it.
- Channel binding between the WASM UI and the proxy (session token / handshake). Tracked separately as a follow-up; see §10.1.

---

## 4. Storage Model

### 4.1 Provider Categories and Registry

Providers are organized by **category**, reflecting the role they play in the pipeline:

| Category | Examples (current + likely) |
|---|---|
| `stt` — Speech-to-text | AssemblyAI, Deepgram, local Whisper |
| `llm` — General-purpose language model (used by formatting *and* conversation) | Anthropic, OpenAI, local Ollama |
| `tts` — Text-to-speech | ElevenLabs, OpenAI TTS, Piper |

A new module `proxy/src/providers.rs` defines:

- `enum ProviderCategory { Stt, Llm, Tts }` — closed set; adding a category is a deliberate change.
- `enum ProviderId` — closed set of *known* provider identifiers (`Anthropic`, `AssemblyAi`, …). Each variant maps to a `ProviderCategory`, a stable lowercase string id, a human-readable display name, and an env-var name (e.g. `PARLEY_ANTHROPIC_API_KEY`).
- `fn registry() -> &'static [ProviderDescriptor]` — the canonical list, used by both the `/status` endpoint and the keystore module.

Adding a new provider is one entry in this registry. The HTTP API and UI auto-pick it up.

### 4.2 Credentials: One Provider, Many Accounts

A single provider may have **multiple named credentials** ("my personal Anthropic key," "my work Anthropic key"). Each credential has:

- `provider`: a `ProviderId`.
- `name`: a user-supplied label, lowercase ASCII, `[a-z0-9_-]{1,32}`. Must be unique within a provider.
- `key`: the secret value (never returned by the API).

Every provider has an implicit `default` credential name when the user sets a key without specifying one. This keeps the simple case simple while making multi-credential a first-class shape rather than a retrofit.

Which credential to use for a given outbound call (e.g. "the Anthropic call inside the conversation orchestrator") is **not** part of this spec — it lives in the provider/model configuration (§10.2). For v1 the proxy may default to `default` when no selection has been made; the orchestrator's request can carry an optional `credential: "<name>"` field to pick a non-default one.

### 4.3 Resolution Order

When the proxy needs a credential `(provider, credential_name)`, it resolves in this order and returns the first match:

1. **Environment variable** — only honored for the credential named `default`. Variable name comes from the provider registry (e.g. `PARLEY_ANTHROPIC_API_KEY`). This preserves the standard "env var beats config" dev/CI workflow without polluting the multi-credential namespace.
2. **OS keystore entry** — service `parley`, account `<provider_id>/<credential_name>` (e.g. `anthropic/default`, `anthropic/work`).
3. **Not configured.**

Resolution is performed per-request. There is no in-process secret cache beyond the lifetime of a single request handler.

### 4.4 Keystore Entry Shape

- Service: `"parley"` (constant).
- Account: `"<provider_id>/<credential_name>"` (e.g. `"anthropic/default"`, `"assemblyai/personal"`).
- Secret: the raw API key string. No JSON wrapping, no metadata. If metadata is ever needed, a sibling account `"<provider_id>/<credential_name>:meta"` is the convention; secrets and metadata never share a payload.

Enumerating credentials for a provider works by listing keystore entries with the prefix `<provider_id>/`. `keyring` does not provide cross-platform enumeration, so the proxy maintains a small **credential index file** (plain JSON, non-secret) at the OS user-config dir (`%APPDATA%\parley\credentials.json` on Windows, `~/Library/Application Support/parley/credentials.json` on macOS) listing `(provider, credential_name)` pairs. The index file contains **no secrets**, only labels. It is the source of truth for "what credentials exist"; the keystore is the source of truth for "what the secret value is." If the two disagree (e.g. user manually deleted a keystore entry), the index entry is reported as `configured: false` with a `"missing_in_keystore"` warning.

### 4.5 What the Proxy Never Does

- **Never** writes a provider key to a file on disk. (The credential index file contains only labels, never the secret.)
- **Never** logs a provider key, in full or in part. Log lines that mention a credential must refer to it by `provider/name` only.
- **Never** returns a provider key in an HTTP response body. The status endpoint reports presence/absence only.

---

## 5. Proxy HTTP Surface

All endpoints live on the existing local proxy and are bound to localhost. They return JSON. The actual key value is never returned by any endpoint.

### 5.1 `GET /api/secrets/status`

Returns the full categorized provider list with the credentials configured for each.

```json
{
  "categories": {
    "stt": [
      {
        "id": "assemblyai",
        "display_name": "AssemblyAI",
        "env_var": "PARLEY_ASSEMBLYAI_API_KEY",
        "credentials": [
          { "name": "default", "source": "keystore", "configured": true }
        ]
      }
    ],
    "llm": [
      {
        "id": "anthropic",
        "display_name": "Anthropic",
        "env_var": "PARLEY_ANTHROPIC_API_KEY",
        "credentials": [
          { "name": "default", "source": "env",      "configured": true },
          { "name": "work",    "source": "keystore", "configured": true },
          { "name": "old",     "source": null,       "configured": false, "warning": "missing_in_keystore" }
        ]
      }
    ],
    "tts": []
  },
  "errors": []
}
```

- `source` is `"env"` | `"keystore"` | `null` (when `configured: false`).
- The `default` credential is always listed for every provider, even if not configured, so the UI can render a uniform "Set key" affordance per provider.
- `errors` carries proxy-startup keystore errors (see §10.3 of original Q3 — degrade-and-report behavior).

### 5.2 `PUT /api/secrets/{provider}/{credential}`

Body: `{ "key": "<raw-api-key>" }`.

Stores the key in the OS keystore under account `"<provider>/<credential>"`. Replaces any existing keystore entry for that credential. Adds the `(provider, credential)` pair to the credential index file if not already present.

Returns the updated single-credential status: `{ "name": "<credential>", "configured": true, "source": "keystore" }`.

Validation:

- `provider` must be a known `ProviderId`; otherwise 404.
- `credential` must match `[a-z0-9_-]{1,32}`; otherwise 400.
- `key` must be a non-empty string of printable ASCII, length 1..=512; otherwise 400.
- A `PUT` to `default` when the env var is set is accepted (writes to keystore), but the response notes `source: "env"` because env still wins at resolution time.

No round-trip to the provider; we do not validate that the key actually works.

### 5.3 `DELETE /api/secrets/{provider}/{credential}`

Removes the keystore entry and the credential index entry. Returns the resulting state: `{ "name": "<credential>", "configured": false, "source": null }` (or `"source": "env"` if `default` and the env var is set).

Idempotent: deleting an absent credential is not an error. The implicit `default` slot for every provider is never removed from the listing — only its `configured` flag flips.

### 5.4 `POST /api/secrets/{provider}/{credential}/rename`

Body: `{ "new_name": "<credential>" }`.

Renames a credential. Implementation: read existing keystore entry, write it under the new account, delete the old, update the index. Cannot rename `default` (400). New name must satisfy the same validation as `PUT`.

Returns the updated provider's full credential list.

### 5.5 Existing Endpoints — Payload Changes

All existing proxy endpoints that currently accept `anthropic_key` or `assemblyai_key` (or equivalent) in their request bodies must drop those fields. The proxy resolves the credential internally per §4.3 at the moment of the outbound provider call.

For endpoints where the caller may want to pick a non-default credential (initially: conversation init), an optional `credential: "<name>"` field is added to the request body. Omitted ⇒ `default`.

If the proxy attempts a provider call and the credential is unresolved, it returns HTTP 412 Precondition Failed with body `{ "error": "provider_not_configured", "provider": "<id>", "credential": "<name>" }`. The UI translates this into a "configure your <provider> key in Settings" message.

### 5.6 Binding & Auth

For this pass, the secrets endpoints rely on the same localhost binding the rest of the proxy uses. **No additional channel binding.** A follow-up spec will introduce a per-launch session token shared between the launcher and the UI (§10.1).

---

## 6. UI Changes

### 6.1 Settings Surface

A single Settings panel renders the `/api/secrets/status` response directly: one section per category (`STT`, `LLM`, `TTS`), one card per provider within each section, one row per credential within each provider.

For each provider card:

- The provider's display name and category are shown.
- A list of credentials, each with one of three states:
  - **Configured (keystore)** — green check, "Replace" and "Remove" buttons.
  - **Configured (env var)** — green check with an `env` badge (only possible for the `default` credential), "Remove" disabled with tooltip "set by `PARLEY_<PROVIDER>_API_KEY`; unset the env var to manage from here". `Replace` writes to the keystore but env still wins until unset; the UI surfaces this clearly.
  - **Not configured** — neutral icon, "Set key" button.
- An "Add credential" button at the bottom of the card prompts for a name (validated client-side against `[a-z0-9_-]{1,32}`) and a key.
- Each credential row has a "Rename" affordance, except `default` which is fixed.

The key input field is **write-only**: once a key is set, the UI never reads it back. Replacing shows an empty input, not the existing value.

*Visual design (spacing, colors, exact layout) is at developer discretion — this is a §1 "design at developer discretion, subject to review" element per requirements practice.*

### 6.2 Credential Selection (Where Used)

For v1, the only place that needs to pick a non-default credential is **conversation init**. The conversation setup UI gains a credential dropdown next to the model selector, populated from the LLM provider's credential list. Defaults to `default`. Selection is sent to the proxy as the optional `credential` field on the init request (§5.5).

Other call sites (formatting, AssemblyAI token fetch) always use `default` in v1.

### 6.3 Removal of Cookie Path

All `document.cookie` reads and writes for provider keys are removed from the UI. Specifically:

- `parley_anthropic_key` cookie reads/writes in `src/ui/app.rs` and `src/ui/conversation.rs` are deleted.
- The `parley_api_key` cookie (AssemblyAI key under the legacy name in `app.rs`) is deleted.
- No migration is performed. Users re-enter their keys once via the new Settings panel. (Acceptable because the install base is the developer.)

### 6.4 Call Sites

Every UI call site that currently passes a provider key in a request body stops doing so. The relevant signals (`anthropic_key`, `api_key`) and their props are removed. The UI continues to gate user actions on configuration status by reading `GET /api/secrets/status` on mount and after any mutation.

---

## 7. Module / File Layout

- **New:** `proxy/src/providers.rs` — `ProviderId`, `ProviderCategory`, `ProviderDescriptor`, the static registry. Pure data; no I/O.
- **New:** `proxy/src/secrets.rs` — credential index, env+keystore resolution, set/delete/rename, status reporting. Defines a `KeyStore` trait (production: `keyring::Entry`; tests: in-memory). All keystore access funnels through this module; no other proxy module touches `keyring` directly. `KeyStore` is also the seam for any future external-password-manager backend.
- **New:** `proxy/src/secrets_api.rs` — the four HTTP handlers in §5 (or folded into `proxy/src/main.rs` if small).
- **Modified:** `proxy/src/main.rs` — register routes; remove `api_key` parameters from existing handlers; instantiate the `KeyStore` and credential index at startup.
- **Modified:** `proxy/src/conversation_api.rs`, `proxy/src/llm/anthropic.rs`, `proxy/src/orchestrator/mod.rs` — drop `anthropic_key` / `api_key` parameters; accept optional `credential` field; call `secrets::resolve(ProviderId::Anthropic, credential)` at the outbound HTTP request.
- **Modified:** `src/stt/assemblyai.rs`, `src/ui/server_fns.rs` — token fetch routes through the proxy, which supplies the AssemblyAI key from the keystore. The browser stops sending it.
- **Modified:** `src/ui/app.rs`, `src/ui/conversation.rs` — remove cookie helpers and key signals; introduce a status hook (`use_secrets_status`) and a Settings panel that renders the categorized response from §5.1.
- **Modified:** `docs/architecture.md` — update the proxy section and the conversation-API credential policy paragraph (line 677) to reflect categorized providers, multi-credential support, and keystore-backed resolution.

---

## 8. Test Plan

### 8.1 Unit Tests (`proxy/src/providers.rs`, `proxy/src/secrets.rs`)

A `KeyStore` trait abstracts the actual keystore call so tests can use an in-memory backend; production wires it to `keyring::Entry`.

**Registry / parsing:**

- Every `ProviderId` round-trips through its string form.
- Unknown provider strings are rejected.
- Every registered provider belongs to exactly one category.
- Credential-name validation: accepts `default`, `work`, `a`, `a_b-c1`, length 32; rejects empty, uppercase, spaces, length 33, leading hyphen if disallowed.

**Resolution:**

- `resolve(p, "default")` returns env value when env is set, regardless of keystore content.
- `resolve(p, "default")` falls back to keystore when env is unset.
- `resolve(p, "non_default")` ignores the env var even if set (env only applies to `default`).
- `resolve(p, name)` returns `None` when neither source has it.

**Mutation:**

- `set(p, name, key)` writes to keystore and is observable via `resolve`; updates the credential index.
- `set` on an existing credential overwrites the keystore entry and does not duplicate the index entry.
- `delete(p, name)` removes the keystore entry and the index entry; subsequent `resolve` returns `None` (or env value for `default`).
- `delete` is idempotent on a missing credential.
- `rename(p, old, new)` moves the keystore entry, updates the index, and `resolve(p, new)` returns the original value while `resolve(p, old)` returns `None`.
- `rename` rejects renaming `default` and rejects target names that already exist.
- Key validation rejects empty strings and strings exceeding the length bound; accepts a representative real-shape key.

**Index / keystore divergence:**

- An index entry whose keystore secret is missing reports `configured: false` with `warning: "missing_in_keystore"`.
- A keystore entry not in the index is *not* reported by `/status` (the index is the source of truth for what credentials exist).

### 8.2 HTTP-Layer Tests (`proxy`)

Using the existing proxy test harness with the in-memory `KeyStore` and a temp-dir credential index:

- `GET /api/secrets/status` initially shows all known providers in their correct categories with only the `default` credential row, all `configured: false`.
- `PUT /api/secrets/anthropic/default` with a valid body marks the credential `configured: true, source: "keystore"`.
- `PUT /api/secrets/anthropic/work` adds a second credential row to the anthropic provider.
- `PUT` with an empty key, oversized key, or non-printable key returns 400.
- `PUT` for an unknown provider returns 404.
- `PUT` with an invalid credential name returns 400.
- `DELETE /api/secrets/anthropic/work` removes that credential; subsequent `GET` no longer lists it.
- `DELETE /api/secrets/anthropic/default` flips `default` to `configured: false` but the row remains in the listing.
- `POST /api/secrets/anthropic/work/rename` to `personal` moves the credential; subsequent `resolve(Anthropic, "personal")` returns the original key.
- Renaming to or from `default` returns 400.
- An env-var-backed `default` reports `source: "env"` and survives a `DELETE` (the keystore entry is removed but env-var resolution still wins).
- A downstream endpoint call with no credential configured returns HTTP 412 with the documented body, including the `credential` field.
- A downstream endpoint that explicitly requests `credential: "work"` returns 412 referencing `work` (not `default`) when `work` is unset.
- No endpoint response body contains the secret string in any code path. Asserted by scanning response bodies for seeded test keys.

### 8.3 Manual Verification (Windows + macOS)

Documented checklist for the developer to run once per OS:

1. Start proxy with no env vars and an empty keystore. Settings panel renders the LLM and STT sections with anthropic and assemblyai each showing only a `default` row, both not configured. Conversation init returns a clear "configure your Anthropic key" message.
2. Set the Anthropic `default` key via Settings. Verify it appears in Windows Credential Manager (Generic Credentials → `parley` / `anthropic/default`) on Windows or in Keychain Access (login keychain → `parley`) on macOS.
3. Add a second credential `work` to anthropic. Verify a second keystore entry `anthropic/work` appears alongside `anthropic/default`.
4. Restart the proxy. Both credentials still listed and configured. A conversation init with `credential: "work"` succeeds against the second key.
5. Rename `work` → `personal`. Old keystore entry gone, new one present, conversation init with `credential: "personal"` works.
6. Replace the `default` key with a new value. Status still configured. Old value is gone from the keystore.
7. Remove the `default` credential. Row remains, marked not configured. Keystore entry is gone.
8. Remove the `personal` credential. Row disappears entirely from the listing.
9. Set `PARLEY_ANTHROPIC_API_KEY` in the environment. `default` reports `source: "env"`. UI shows the env badge. Removing `default` from the UI is disabled; the env still resolves.
10. After removing the cookie code, confirm with browser devtools that no `parley_*_key` cookies are written for any UI flow.
11. Manually delete the `anthropic/default` keystore entry via OS tooling without removing the index entry. `/status` reports `configured: false, warning: "missing_in_keystore"`.

---

## 9. Scope

### 9.1 In Scope

- `proxy/src/providers.rs` registry (categories, ids, display names, env-var names) covering anthropic and assemblyai.
- `proxy/src/secrets.rs` module with env+keystore resolution, `KeyStore` trait, credential index file, multi-credential support, and rename.
- Four HTTP endpoints (§5.1–5.4).
- Optional `credential` field on conversation init; `default` everywhere else.
- Removal of `api_key` / `anthropic_key` request-body parameters from all existing proxy endpoints.
- AssemblyAI temp-token fetch routed through the proxy so the browser does not handle the AssemblyAI key.
- UI Settings panel rendering categorized providers + multi-credential rows, including add/replace/remove/rename.
- Conversation-init UI: credential dropdown next to model selector.
- Removal of all cookie-based key handling.
- `docs/architecture.md` updates.

### 9.2 Out of Scope (Deferred, Tracked Separately)

- Channel-binding session token between UI and proxy (§10.1).
- Provider/model configuration spec (which model slug to use, default models, enabling/disabling providers) — separate spec (§10.2).
- Encrypted-file fallback for headless/Linux environments.
- Migration of existing cookie values into the keystore. Users re-enter keys once.
- Per-key usage metering, rotation reminders, expiry handling.
- Programmatic password-manager backends (1Password CLI, Bitwarden CLI). The `KeyStore` trait is the seam if we add this later.
- TTS providers — registry will list the `tts` category with no providers; first TTS provider will be added in its own change.

---

## 10. Follow-Ups (Separate Specs)

### 10.1 Channel Binding

The localhost trust boundary is broad: any process running as the same user can reach the proxy. A follow-up spec will introduce a per-launch session token that the launcher writes to a user-only file and the UI reads on startup, then sends with every secrets-API request. This raises the bar from "any localhost process" to "any process that can read your user-scoped files" — a meaningful improvement for shared dev machines, cheap to add once the storage migration is in.

### 10.2 Provider & Model Configuration

A separate spec will cover the *non-secret* configuration that lives alongside credentials: which model slug to use per provider (e.g. `claude-haiku-4-7` vs. `claude-opus-4-7`), which credential is the default for each use-case (formatting vs. conversation), and whether a provider is enabled. Per Philosophy §7, this is plain human-readable config (TOML in the user config dir), not a keystore concern. The Settings UI will eventually render both side-by-side, but the storage layers stay clean.

---

## 11. Resolved Decisions

- **Provider categorization** (`stt` / `llm` / `tts`) — yes, baked into the registry and the `/status` response. UI iterates categories.
- **Provider-test endpoint** (`POST /api/secrets/test/...`) — deferred. Nice to have, not required for v1.
- **Keystore errors at startup** (e.g. macOS Keychain access denied) — degrade to env-var-only and surface the error in `/status`'s `errors` array. Do not hard-fail.
- **Multi-credential per provider** — in v1, not deferred. `default` is implicit; additional credentials are opt-in.
- **External password manager integration** — out of v1. Users back up keys manually to whatever they prefer. The `KeyStore` trait is the future seam.
- **Linux** — not tested in v1. The `keyring` crate transparently supports desktop Linux via Secret Service, so the surface area required to enable it is nearly zero; headless Linux remains a separate problem.
