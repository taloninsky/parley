# Training & Privacy Transparency — Specification

**Status:** Draft
**Type:** Specification
**Audience:** Both
**Date:** 2026-04-22

---

## 1. Overview

Every external provider Parley sends data to (STT, LLM, TTS) has a privacy posture: whether it trains on user data, whether humans review it, how long it retains it, and whether it shares it with third parties. These postures vary by provider, by plan, by account configuration, and they change over time. The user has a right to know — at the moment of configuration *and* at the moment of use — exactly what they are opting into.

This spec defines:

1. The **data model** for per-credential privacy posture (four axes, plus a synthesized verdict).
2. The **configuration surface** for the user to attest their account's posture per credential.
3. The **attestation re-check** loop: a periodic LLM-driven web search that compares the user's attestation to the provider's published policy and flags drift.
4. The **resolution algorithm** that composes a session-wide posture from every provider/credential a session will touch.
5. The **provenance pinning**: each turn freezes the posture in effect at the moment of the call.
6. The **UI surfaces**: settings panel, model picker badges, status bar shield, per-turn indicator, pre-session modal.

Cost transparency is a parallel concern with a parallel UI surface but a different domain. It is covered separately (see [architecture.md §Cost](architecture.md#L633) and [conversation-mode-spec.md §11](conversation-mode-spec.md#L514)). Both meters live in the same status-bar region and both pin into [`TurnProvenance`](architecture.md#L630); cross-references throughout.

This spec depends on the credential model defined in [secrets-storage-spec.md](secrets-storage-spec.md). It does not redefine credentials; it adds posture metadata alongside them.

---

## 2. Goals

- The user knows, at the moment of credential configuration, what claims their provider/plan makes about training, human review, retention, and third‑party sharing — and explicitly attests to them.
- The user knows, at the moment of model selection, the posture consequences of choosing a particular model under a particular credential.
- The user knows, during an active session, whether any data flowing through the pipeline lands at a provider that retains or trains on it.
- Past sessions preserve the posture that was in effect when they were recorded, even if the user later changes credentials or plans.
- The system is honest about the limits of its knowledge: postures are *attestations*, not verifications. The UI never claims more than that.
- Provider policies change. The system periodically re‑checks published policy against the user's attestation and flags drift.

## 3. Non-Goals

- **Verifying provider behavior.** Parley cannot prove that a provider is honoring its own ToS. It can only repeat what the provider claims and what the user has configured.
- **Cryptographic attestation** (remote attestation, signed policy receipts). Out of scope; no provider currently offers this.
- **Enforcing posture.** This spec surfaces posture; it does not block sends. A user can knowingly use a training‑enabled provider after acknowledging the modal. (A future spec may add a "block on red" mode.)
- **Cost transparency.** Tracked separately. See cross‑references above.
- **Posture for local providers' behavior** beyond "data does not leave this device." Local providers get a single `Local` state; we do not enumerate filesystem retention policies.
- **Migrating posture metadata for existing credentials configured before this spec ships.** Existing credentials are reported as `Unknown` posture until the user re-attests.

---

## 4. Data Model

### 4.1 The Four Axes

Privacy posture is decomposed into four independent axes. Each axis is a closed enum. A credential's posture is the tuple of all four (plus, for STT, two sub‑axes — see §4.2).

| Axis | Variants | Meaning |
|---|---|---|
| **Training** | `None` / `OptOutRequired` / `OptOutApplied` / `TrainsByDefault` / `Unknown` | Does the provider train models on this data? |
| **HumanReview** | `None` / `AbuseOnly` / `QualityReview` / `Unknown` | Do humans at the provider read this data? |
| **Retention** | `Zero` / `Days(u32)` / `Indefinite` / `Unknown` | How long does the provider retain this data? |
| **ThirdPartySharing** | `None` / `SubprocessorsOnly` / `Yes` / `Unknown` | Does the provider share this data outside its own systems? |

#### Training axis — variant semantics

The training axis has four non‑Unknown states because the *user's action* matters, not just the provider's default:

- `None` — Provider contractually does not train on data sent under this credential. (Anthropic commercial API default, OpenAI API default as of 2024+, AssemblyAI ZDR contract.)
- `OptOutRequired` — Provider's default is to train, but offers a dashboard toggle to opt out, **and the user has not flipped it.** The credential is exposed to training.
- `OptOutApplied` — Same provider/plan as `OptOutRequired`, but the user has flipped the dashboard toggle. Credential is not exposed to training.
- `TrainsByDefault` — Provider trains on this data and offers no opt‑out under this plan. (Free‑tier consumer products typically; some enterprise plans for fine‑tuning credit.)
- `Unknown` — Posture has never been attested, or attestation was wiped pending re‑check.

The distinction between `OptOutRequired` and `OptOutApplied` exists because the credential itself is identical in both cases — only the user can know which is true. The system asks; the user attests.

#### Default for new credentials — worst case

When a credential is first added, all four axes default to the worst non‑`Unknown` state for that provider's known plans:

- Training: `TrainsByDefault`
- HumanReview: `QualityReview`
- Retention: `Indefinite`
- ThirdPartySharing: `Yes`

The user must explicitly downgrade these during the credential setup flow. This enforces the principle that **silence equals worst case**. A user who skips setup and starts using the credential anyway is treated as if they accepted training.

### 4.2 STT Sub-Axes — Audio vs. Transcript

For credentials in the `stt` category, the `Training` axis splits into two parallel sub‑axes:

- **`AudioTraining`** — Does the provider train acoustic models on the raw waveform? This is the bigger privacy concern: voice is a **biometric identifier** that can match the speaker across services.
- **`TranscriptTraining`** — Does the provider train language models on the recognized text? Lower (but real) privacy concern: same as any text‑based LLM training.

Same four variants as `Training`. Both must be attested separately. The composite posture (§4.4) treats either being non‑clean as a training violation, but the UI surfaces them independently so a user can see "voice not trained on, but transcripts are."

Non‑STT credentials use only the single `Training` axis.

### 4.3 Provider Plan & Account Type

Each credential carries a `plan` field — a free-form string scoped to its provider. The plan determines which posture options are available and what the per‑axis defaults are:

- Anthropic: `commercial_api` / `commercial_api_zdr` / `console_personal` / `bedrock_passthrough` / `vertex_passthrough`
- OpenAI: `api_default` / `api_zdr` / `chatgpt_team` / `chatgpt_enterprise`
- AssemblyAI: `pay_as_you_go` / `enterprise` / `enterprise_zdr`
- ElevenLabs: `creator` / `pro` / `enterprise` / `enterprise_zero_retention`
- Cartesia: `free` / `starter` / `startup` / `enterprise` / `enterprise_zdr` — training is off by default on every plan and is dashboard-toggleable; GDPR / SOC 2 / HIPAA controls (BAA, custom retention) only land on `enterprise` and above.
- Deepgram: `pay_as_you_go` / `enterprise` / `enterprise_zdr`

The set of valid plans per provider lives in the provider posture registry (§5). Adding a new plan is a registry change, not a code change.

Each `(provider, plan)` pair in the registry carries a *suggested* posture tuple — the posture the provider's published policy claims for that plan. This is the starting point for the user's attestation, not the final word; the user can override.

### 4.4 Composite Posture & The ZDR/ZDT Verdict

A composite posture is computed two ways:

**Per‑credential composite** — synthesizes the four (or five, for STT) axes into a single state for badging:

- **`Clean`** — Training is `None` or `OptOutApplied`, Retention is `Zero`, HumanReview is `None` or `AbuseOnly`, ThirdPartySharing is `None` or `SubprocessorsOnly`. Displayed as the **ZDR/ZDT** green badge.
- **`Degraded`** — At least one axis is non‑clean but Training is not `TrainsByDefault` and not `OptOutRequired`. (E.g., retained for 30 days but never trained on.) Yellow badge with the offending axes called out.
- **`Training`** — Training is `TrainsByDefault` or `OptOutRequired`, *or* AudioTraining/TranscriptTraining is the same. Red badge.
- **`Unknown`** — Any axis is `Unknown`. Gray badge with question mark.
- **`Local`** — Provider is local (Whisper, Ollama, Piper). Purple "Local" badge. Data does not leave the device.

**Per‑session composite** — synthesizes the composites of every credential the session will touch (or has touched). The session composite is the **worst** state across all participating credentials. One Training credential turns the whole session red, even if every other credential is Clean. Each axis is also independently aggregated so the UI can show "ZDR ✓ / ZDT ✗" granularly.

### 4.5 Rust Type Sketch — Specification Level

```rust
// proxy/src/posture.rs

pub enum TrainingPosture {
    None,
    OptOutRequired,
    OptOutApplied,
    TrainsByDefault,
    Unknown,
}

pub enum HumanReviewPosture { None, AbuseOnly, QualityReview, Unknown }

pub enum RetentionPosture {
    Zero,
    Days(u32),
    Indefinite,
    Unknown,
}

pub enum SharingPosture { None, SubprocessorsOnly, Yes, Unknown }

pub enum CompositeState { Clean, Degraded, Training, Unknown, Local }

pub struct CredentialPosture {
    // Identity (joins to secrets-storage-spec credential model)
    pub provider: ProviderId,
    pub credential_name: String,
    pub plan: String,

    // Per-axis attestation
    pub training: TrainingPosture,                  // non-STT only; STT uses the two below
    pub audio_training: Option<TrainingPosture>,    // STT only
    pub transcript_training: Option<TrainingPosture>, // STT only
    pub human_review: HumanReviewPosture,
    pub retention: RetentionPosture,
    pub third_party_sharing: SharingPosture,

    // Provenance of the attestation
    pub attested_at: DateTime<Utc>,
    pub attested_by: AttestationSource,             // User | RegistryDefault | LlmRecheck
    pub policy_url: Option<String>,                 // ToS link captured at attestation time
    pub last_recheck: Option<RecheckResult>,
}

pub enum AttestationSource { User, RegistryDefault, LlmRecheck }

pub struct RecheckResult {
    pub checked_at: DateTime<Utc>,
    pub model_used: String,
    pub findings: Vec<RecheckFinding>,
    pub overall: RecheckOutcome,                    // Confirmed | Drift | Inconclusive | Error
}

pub struct RecheckFinding {
    pub axis: PostureAxis,                          // enum: Training, AudioTraining, ...
    pub user_attestation: String,                   // serialized variant
    pub policy_says: String,                        // model's reading of the policy
    pub matches: bool,
    pub citation: Option<String>,                   // URL + section
}
```

Rough sketch; field names will firm up during implementation. The shape is the commitment.

### 4.6 Local Providers

Local providers (Whisper, Ollama, Piper) do not have credentials in the secrets-storage sense — they have endpoints (typically `http://localhost:...`) or model files. They get a `LocalProvider` record in the same registry. Their composite is always `Local`. The session composite treats `Local` as equivalent to `Clean` for the purpose of the green flag — but the per‑provider badge in the UI is purple, not green, to make the distinction visible.

---

## 5. Posture Registry

A registry — a versioned data file shipped with the binary — defines the canonical list of `(provider, plan)` pairs and their suggested postures. It is the source of truth for "what plans does AssemblyAI offer and what does each one claim."

### 5.1 Format

TOML, located at `proxy/data/posture-registry.toml`. Hot‑reloadable in development; embedded at build time in release. A user-overlay file at `%APPDATA%\parley\posture-registry.overlay.toml` (Windows) / `~/Library/Application Support/parley/posture-registry.overlay.toml` (macOS) is loaded after the embedded registry and replaces matching `(provider, plan)` entries. This lets a user add a custom enterprise plan without forking the binary.

### 5.2 Schema

```toml
[[provider]]
id = "anthropic"
display_name = "Anthropic"
category = "llm"
policy_index_url = "https://www.anthropic.com/legal"

[[provider.plan]]
id = "commercial_api"
display_name = "Commercial API (default)"
training = "None"
human_review = "AbuseOnly"
retention = { days = 30 }
third_party_sharing = "SubprocessorsOnly"
policy_url = "https://www.anthropic.com/legal/commercial-terms"
policy_reviewed = "2026-04-15"

[[provider.plan]]
id = "commercial_api_zdr"
display_name = "Commercial API — Zero Data Retention"
training = "None"
human_review = "None"
retention = "Zero"
third_party_sharing = "SubprocessorsOnly"
policy_url = "https://www.anthropic.com/legal/zero-data-retention"
policy_reviewed = "2026-04-15"
```

Every plan entry carries a `policy_url` and a `policy_reviewed` date. A plan whose `policy_reviewed` is older than 90 days is shown in the UI with a "stale" indicator until either the user re-attests or the LLM re-check confirms.

### 5.3 Adding a New Provider/Plan

A new provider is a new `[[provider]]` block. A new plan under an existing provider is a new `[[provider.plan]]` block. No code change required. The Settings UI reads the registry directly via a new `GET /api/posture/registry` endpoint and renders the dropdowns dynamically.

---

## 6. Attestation Flow

### 6.1 Initial Attestation at Credential Creation

When the user creates a credential (per [secrets-storage-spec §6.1](secrets-storage-spec.md#L195)), the Settings UI extends the existing flow with a posture attestation step. The flow becomes:

1. User picks provider (existing).
2. User picks plan from the provider's plan list (new). Default selection: the worst-case plan — typically the free/default tier — to nudge the user toward conscious selection.
3. UI loads the registry's suggested posture for that `(provider, plan)` and renders four (or five, for STT) toggle rows. Each row shows: axis name, suggested value, the policy quote/URL backing it, and a control to override.
4. For axes that depend on a user dashboard action (`OptOutRequired` ↔ `OptOutApplied`), the row includes an explicit checkbox: *"I have opted out in [Provider]'s dashboard."* Unchecked ⇒ `OptOutRequired`. Checked ⇒ `OptOutApplied`.
5. User enters the API key (existing).
6. UI submits the credential AND the attestation in a single transaction (§7.1).

The user cannot create the credential without going through the attestation step. Skipping = worst case (§4.1) explicitly recorded as `attested_by: User` with all axes at their worst-case variant. The UI shows the user the resulting red badge before they confirm.

### 6.2 Per-Provider UI Variants

Because each provider's plan structure is different, the attestation UI is rendered from the registry rather than hand‑coded per provider. The registry entry for each plan provides:

- The list of axes that apply (most providers: all four; STT: five).
- For each axis, the legal value range the user can attest. (E.g., a plan that contractually has `Zero` retention does not let the user attest `Indefinite` — that would be incoherent.)
- The policy URL and quote for each axis, so the attestation row shows *what* the user is being asked to confirm.

This means the UI is data‑driven and uniform in structure, but specific in content per provider/plan.

### 6.3 Re-Attestation Triggers

The user is prompted to re‑attest when:

- The plan's `policy_reviewed` date in the registry is updated (the registry shipped a new version → maybe the policy changed).
- The LLM re-check (§8) finds drift.
- The user manually invokes "Re-verify posture" from the credential row in Settings.
- 180 days have passed since the last user attestation.

Until re-attestation, the credential is marked with a "stale attestation" badge but continues to function. Sessions started under a stale attestation pin the stale posture into provenance; the staleness is part of the recorded provenance.

---

## 7. HTTP Surface

All endpoints live on the existing local proxy and are bound to localhost. They return JSON.

### 7.1 `PUT /api/secrets/{provider}/{credential}` — Extended

The existing endpoint from [secrets-storage-spec §5.2](secrets-storage-spec.md#L141) gains an additional required body field:

```json
{
  "key": "<raw-api-key>",
  "posture": {
    "plan": "commercial_api_zdr",
    "training": "None",
    "human_review": "None",
    "retention": "Zero",
    "third_party_sharing": "SubprocessorsOnly"
  }
}
```

For STT credentials, replace `training` with `audio_training` and `transcript_training`.

The proxy validates that:

- `plan` exists in the registry for this provider.
- Each axis value is in the legal range for this `(provider, plan)` per the registry.

A `PUT` without `posture` is rejected with HTTP 400 `{ "error": "posture_required" }`. There is no path to create a credential without attestation.

### 7.2 `PUT /api/secrets/{provider}/{credential}/posture`

Updates the posture of an existing credential without rotating the key. Body: same `posture` object as §7.1. Used for re‑attestation.

### 7.3 `GET /api/posture/registry`

Returns the merged (embedded + overlay) registry as JSON. Used by the Settings UI to populate plan dropdowns and per-axis options.

### 7.4 `GET /api/posture/session-preview`

Body / query: a list of `(category, provider, credential)` selections that the user is about to commit to for a session.

Response: the composite posture for that combination, plus the per-credential breakdowns. Used by the conversation init UI to render the pre‑session modal (§9.4) before the first turn is sent.

```json
{
  "composite": "Training",
  "axes": {
    "training": "Training",
    "audio_training": "Clean",
    "transcript_training": "Training",
    "human_review": "Degraded",
    "retention": "Degraded",
    "third_party_sharing": "Clean"
  },
  "credentials": [
    { "provider": "anthropic", "credential": "default", "composite": "Clean", "plan": "commercial_api_zdr" },
    { "provider": "assemblyai", "credential": "personal", "composite": "Training", "plan": "pay_as_you_go" }
  ],
  "stale": ["assemblyai/personal"]
}
```

### 7.5 `POST /api/posture/recheck`

Body: `{ "provider": "...", "credential": "..." }` or `{ "all": true }`.

Triggers an immediate LLM-driven re-check (§8). Returns the `RecheckResult` synchronously; this can take several seconds. Used by the "Re-verify now" button in Settings and by the scheduled background job.

---

## 8. Attestation Re-Check (LLM Web Search)

A periodic job uses an LLM with web search to verify each credential's attestation against the provider's currently published policy.

### 8.1 Frequency & Trigger

- Default: weekly per credential, staggered to avoid clustering.
- Manual: via `POST /api/posture/recheck`.
- On startup: any credential whose last re-check is older than the interval is re-checked within 5 minutes of proxy start.

The job is run **by the proxy**, not the UI. The proxy uses one of the user's configured LLM credentials (the user picks which one in Settings — defaults to the cheapest configured model that supports web search). The credential used for re-check is **its own posture risk**: the re-check sends the policy URL and the user's attestation to that LLM. The user is told this and the chosen credential's posture applies.

### 8.2 Prompt Structure

For each credential being checked, the proxy issues a single LLM call with:

- The `policy_url` from the registry entry.
- The user's current attestation across all axes.
- A structured instruction: "Fetch the policy at this URL. For each of the following claims, determine whether the published policy supports the claim. Cite the section. Output JSON conforming to this schema: ..."

The exact prompt is versioned alongside the registry; prompt changes are reviewed.

### 8.3 Outcome Handling

- **`Confirmed`** — All axes match. `last_recheck` is updated. No user action.
- **`Drift`** — One or more axes don't match. `last_recheck` is updated with findings. The credential is flagged in the UI; the user is prompted to re-attest. The credential is **not** automatically downgraded — the LLM might be wrong. The user adjudicates.
- **`Inconclusive`** — The model couldn't reach a confident verdict (policy ambiguous, page unreachable). Logged; no UI alarm; retried next cycle.
- **`Error`** — Network error, LLM error, etc. Logged; retried with exponential backoff.

### 8.4 Limits & Honesty

The UI never says "verified by Parley." It says **"Re-checked YYYY-MM-DD by [model]; the model says the policy still supports your attestation."** The user is shown the model's findings and citations. The re-check is a reminder system, not a guarantee.

Re-check failures do not block sessions. They surface in Settings as a yellow indicator on the credential row.

---

## 9. UI Surfaces

Five surfaces. All are data-driven from the data model in §4.

### 9.1 Settings — Privacy Posture Panel

A new panel at the top of Settings, above the existing per‑category provider cards from [secrets-storage-spec §6.1](secrets-storage-spec.md#L186). It shows:

- The **session composite** for the *currently selected default* configuration: a large badge — `ZDR / ZDT` (green), partial (yellow with axis breakdown like `ZDR ✓ ZDT ✗`), `Training In Loop` (red), or `Unknown` (gray).
- A table of every configured credential across every category, one row per credential, with: provider, credential name, plan, per-axis state, composite badge, last attested date, last re-checked date, "Re-verify" button.
- A "Posture audit log" disclosure that shows the history of attestation changes and re-check results for each credential.

### 9.2 Per-Credential Card — Posture Section

Each credential card in the existing Settings provider list (per [secrets-storage-spec §6.1](secrets-storage-spec.md#L186)) gains a posture section beneath the key controls:

- The four (or five, for STT) axis rows, editable inline.
- The composite badge for this credential.
- Plan selector dropdown.
- Last attested and last re-checked timestamps.
- "Re-attest" button (re-runs the §6.1 flow for this credential).
- "Re-verify now" button (runs §7.5).

### 9.3 Per-Model Picker — Posture Badge

Every model dropdown — in conversation setup, in formatting config, anywhere a model is chosen — renders an inline posture badge next to the model name. The badge reflects the composite of the credential currently bound to that model selection.

If the user changes the credential mid‑configuration, the badge updates live. The user sees the consequence of their choice at the moment of choice.

### 9.4 Pre-Session Modal

Before the first turn of any session that involves a non‑Clean composite, a modal appears:

- Headline: depends on composite. Red: *"This conversation will train AI models on your input."* Yellow: *"This conversation has degraded privacy."*
- Body: the per-credential breakdown — which provider contributes which violation. Plain language.
- Primary action: *"Continue this session"* (records the user's acknowledgment in session provenance).
- Secondary action: *"Cancel and reconfigure"* (returns to Settings).
- A *"Don't show again for this session"* checkbox. **Per‑session only**, never permanently. A new session always re-prompts if the composite is non‑Clean.

If the composite is `Clean` or `Local`, no modal — the green/purple shield in the status bar (§9.5) is the only signal.

### 9.5 Status Bar — Always-Visible Shield

In every session view, the status bar carries a shield indicator on the side opposite the cost meter (which already lives there per [architecture.md Phase 4f.7](architecture.md#L693)):

- **Green shield (ZDR / ZDT)** — session composite is `Clean`.
- **Purple shield (Local)** — every active provider is local.
- **Yellow shield (mixed)** — composite is `Degraded`. Hovering shows axis breakdown.
- **Red shield (Training)** — composite is `Training`. The shield is bolder and slowly pulses.
- **Gray ?-shield (Unknown)** — at least one credential has Unknown posture.

Click expands a popover with the per-credential breakdown and a "Manage in Settings" link. The shield is **always present**, even after the modal has been dismissed — the user never loses ambient awareness of what they're opted into.

### 9.6 Per-Turn Indicator

Each assistant turn bubble already shows its USD cost ([architecture.md Phase 4f.7](architecture.md#L693)). A small posture chip joins it: a single-letter chip per axis (`T` training, `H` human review, `R` retention, `S` sharing — colored green/yellow/red), so that scrolling back through history shows the posture pinned to each turn. Clicking expands the full posture record for that turn.

This is essential for historical sessions: even if today's posture is clean, you can see that a turn from three months ago was made under different conditions.

---

## 10. Provenance Pinning

Posture is recorded into [`TurnProvenance`](architecture.md#L630) at the moment each turn is materialized, alongside cost and model.

### 10.1 What Gets Pinned

For each turn, the proxy captures:

- The composite posture of every credential touched during this turn (typically the LLM credential; in conversation mode with live STT, also the STT credential).
- The full per-axis posture for each of those credentials (snapshotted at this moment, even if the user later changes attestation).
- The plan ID and the policy URL captured at attestation time.
- The `attested_at` timestamp and the `last_recheck` outcome at the moment of the call.
- A "stale attestation" flag if the attestation was past its 180-day refresh.

### 10.2 Field Sketch

```rust
// Added to TurnProvenance (parley-core)
pub struct PosturePin {
    pub credentials: Vec<CredentialPostureSnapshot>,
    pub session_composite: CompositeState,
}

pub struct CredentialPostureSnapshot {
    pub provider: ProviderId,
    pub credential_name: String,
    pub plan: String,
    pub training: TrainingPosture,
    pub audio_training: Option<TrainingPosture>,
    pub transcript_training: Option<TrainingPosture>,
    pub human_review: HumanReviewPosture,
    pub retention: RetentionPosture,
    pub third_party_sharing: SharingPosture,
    pub composite: CompositeState,
    pub attested_at: DateTime<Utc>,
    pub stale: bool,
    pub policy_url: Option<String>,
}
```

This data is serialized with the session and is the source of truth for the per‑turn indicator (§9.6) when historical sessions are reloaded.

### 10.3 Pre-Session Acknowledgment

If the user clicked through the pre-session modal (§9.4), the session also records:

- That the modal was shown.
- The composite at the time of acknowledgment.
- The timestamp of the click.

This is a session‑level field, not per‑turn. It documents informed consent.

---

## 11. Module / File Layout

- **New:** `proxy/src/posture.rs` — `TrainingPosture`, `HumanReviewPosture`, `RetentionPosture`, `SharingPosture`, `CompositeState`, `CredentialPosture`, composite synthesis, registry loading. Pure data + logic, no I/O beyond registry file read.
- **New:** `proxy/src/posture_api.rs` — handlers for §7.2, §7.3, §7.4, §7.5 (or folded into `proxy/src/main.rs` if small).
- **New:** `proxy/src/posture_recheck.rs` — the periodic re-check job, the LLM call, prompt template, JSON parsing of model output.
- **New:** `proxy/data/posture-registry.toml` — the embedded registry.
- **Modified:** `proxy/src/secrets.rs` — credential records gain a `posture` field; the credential index file format is extended; a migration that defaults existing entries to `Unknown` posture on first load.
- **Modified:** `proxy/src/secrets_api.rs` — `PUT /api/secrets/{provider}/{credential}` requires `posture` in body (§7.1).
- **Modified:** `parley-core/src/chat.rs` — `TurnProvenance` gains a `posture: PosturePin` field. `Cost` and `PosturePin` sit alongside each other.
- **Modified:** `proxy/src/orchestrator/mod.rs` — when materializing an `AiTurnAppended` event, pin the posture from every credential the orchestrator touched on this turn.
- **Modified:** `src/ui/conversation.rs` — render the status-bar shield, the per-turn posture chip, the pre-session modal.
- **Modified:** `src/ui/app.rs` — Settings panel gains the Privacy Posture panel (§9.1) and per-credential posture sections (§9.2). Model pickers gain inline posture badges (§9.3).
- **Modified:** `src/ui/server_fns.rs` — `WireTurn`/`WireTurnProvenance` mirror the new posture fields end‑to‑end.
- **Modified:** `docs/architecture.md` — extend the conversation-API and `TurnProvenance` sections to mention posture pinning; reference this spec.
- **Modified:** `docs/secrets-storage-spec.md` — add a forward reference to this spec from §5.2 (posture is now required on credential creation).
- **Modified:** `docs/conversation-mode-spec.md` — add a posture subsection cross-referencing this spec from §11 (which is currently cost-only).

---

## 12. Test Plan

### 12.1 Unit Tests — `proxy/src/posture.rs`

**Per-axis variants & composite synthesis:**

- Every credential whose `(training, human_review, retention, third_party_sharing)` is `(None|OptOutApplied, None|AbuseOnly, Zero, None|SubprocessorsOnly)` synthesizes to `Clean`.
- A credential with `Training = TrainsByDefault` synthesizes to `Training`, regardless of other axes.
- A credential with `Training = OptOutRequired` synthesizes to `Training` (the user did not opt out, so training applies).
- A credential with `Training = OptOutApplied` and any other axis non-clean synthesizes to `Degraded`, not `Training`.
- A credential with any axis = `Unknown` synthesizes to `Unknown`.
- A local provider record synthesizes to `Local`.

**STT sub-axes:**

- An STT credential where `audio_training = None` and `transcript_training = TrainsByDefault` synthesizes to `Training`.
- An STT credential where both are `None` and other axes are clean synthesizes to `Clean`.

**Session composite:**

- A session with one Clean credential and one Training credential synthesizes to `Training`.
- A session with one Clean and one Degraded synthesizes to `Degraded`.
- A session with one Local and one Clean synthesizes to `Clean` (Local is at least as good as Clean).
- A session with one Local and one Training synthesizes to `Training`.
- A session with all-Local credentials synthesizes to `Local`.
- A session with one Unknown credential and one Training credential synthesizes to `Training` (Training is worse than Unknown).
- A session with one Unknown and one Clean synthesizes to `Unknown`.

**Per-axis aggregation:**

- The session-level per-axis state is the worst across all participating credentials, axis by axis.
- An STT-less session reports `audio_training = N/A` rather than collapsing to `Clean`.

### 12.2 Registry Tests

- The embedded registry parses without error.
- Every plan in the registry has a `policy_url` and `policy_reviewed` date.
- Every category has at least one provider.
- Plans whose `policy_reviewed` is older than 90 days at the test build date trigger a warning (test annotation, not a hard fail).
- An overlay file with a duplicate `(provider, plan)` correctly replaces the embedded entry.
- An overlay file with a malformed entry produces a clear startup error and the embedded version is used.

### 12.3 HTTP-Layer Tests

- `PUT /api/secrets/anthropic/default` with a key but no posture returns 400 `{"error": "posture_required"}`.
- `PUT` with posture matching a known plan succeeds and the credential is queryable via `/api/secrets/status`.
- `PUT` with a plan not in the registry for that provider returns 400.
- `PUT` with an axis value outside the legal range for the plan returns 400.
- `PUT /api/secrets/anthropic/default/posture` updates only posture, leaves the key intact, and updates `attested_at`.
- `GET /api/posture/registry` returns the merged embedded+overlay registry.
- `GET /api/posture/session-preview` with a list of credential selections returns the correct composite and per-axis breakdown.
- `POST /api/posture/recheck` invokes the re-check job and returns its result.

### 12.4 Re-Check Tests

These use a mock LLM client.

- A re-check whose LLM response confirms every axis records `Confirmed`, updates `last_recheck`, leaves `attested_at` unchanged.
- A re-check whose LLM response disagrees on one axis records `Drift` with the disagreeing axis cited.
- A re-check whose LLM response is malformed JSON records `Error`.
- A re-check whose LLM response is incomplete (missing axes) records `Inconclusive`.
- A drifted credential still resolves and is usable in subsequent sessions; only the UI flag changes.

### 12.5 Provenance Pinning Tests (`parley-core`)

- An `AiTurnAppended` event includes a `PosturePin` with one entry per credential that contributed to the turn.
- A turn made with a stale attestation has `stale: true` in the snapshot.
- A turn's snapshot is unaffected by subsequent credential posture changes — the snapshot is a deep copy at write time.
- A turn made under a credential whose posture is `Unknown` correctly serializes and round-trips.

### 12.6 Manual Verification

- The pre-session modal appears on first turn of a session involving any non-Clean credential and is suppressed (this session only) after acknowledgment.
- The status-bar shield always reflects the current session composite and updates live when credentials change mid-session.
- The per-turn posture chip on assistant bubbles renders the correct color combination per axis.
- The Privacy Posture panel in Settings correctly aggregates across all configured credentials and shows the green ZDR/ZDT badge if and only if every credential is Clean.
- Local provider sessions show the purple Local shield, not green.
- A credential whose attestation passes 180 days surfaces a "stale" indicator in Settings.
- Manually triggering a re-check on a credential surfaces the LLM's findings and citations.

---

## 13. Open Questions

- **Which LLM credential drives re-check?** Default behavior is the cheapest configured model that supports web search. The user can override in Settings. Worth a one‑line confirmation that this default is acceptable.
- **What does the per-turn posture chip look like at small font sizes?** Visual design at developer discretion, subject to review (per §1 of the documentation skill).
- **Should the pre-session modal be triggered for the *first turn after a credential change* mid-session?** Currently the spec says modal is shown once per session at first turn. If the user swaps a Clean credential for a Training credential mid-session, should we re-prompt? **Strawman: yes** — the privacy posture changed materially. Confirm before implementation.
- **Provider plan changes upstream.** A provider may rename or remove a plan in their docs. The registry will lag. Do we want a "registry version" indicator visible in Settings so the user can see when the shipped registry was last updated? **Strawman: yes**, in the Privacy Posture panel header.
- **Cost spec parallel.** Cost is documented but lacks a single spec doc. This spec implicitly suggests one should exist. Out of scope here; logged as a follow-up.

---

## 14. Cross-References

- [docs/secrets-storage-spec.md](secrets-storage-spec.md) — credential model this spec extends.
- [docs/conversation-mode-spec.md](conversation-mode-spec.md) — session lifecycle that posture pins into.
- [docs/conversation-mode-spec.md §11](conversation-mode-spec.md#L514) — cost tracking, the parallel domain that shares UI surface.
- [docs/architecture.md](architecture.md) — `TurnProvenance` (line 630), Phase 4f.7 cost UI (line 693), provenance principles (line 30).
- [docs/philosophy.md](philosophy.md) — §6 "Models come and go" (provider abstraction is precondition for posture being meaningful) and §9 "Don't guess — ask" (Parley should surface uncertainty about its own privacy state).
