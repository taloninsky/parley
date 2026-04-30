# Google Gemini API Data Policy Notes

**Status:** Active
**Type:** Research
**Audience:** Both
**Date:** 2026-04-29

## Summary

This note summarizes the public Google data-use, training, retention, and subprocessor policy surface relevant to using paid frontier Gemini API access in Parley. It covers paid Gemini API and paid Google AI Studio access for Gemini-family frontier models. It is written for people who want to verify that Parley is not accidentally using their words, ideas, confidential material, or other intellectual property in a way that gives away their IP or trains a third-party model.

This is an engineering policy note, not legal advice. It records the public policy surface Parley relies on as of 2026-04-29.

Google's paid Gemini API policy posture is:

- **Gemini API paid services:** Google says it does not use prompts, associated system instructions, cached content, files, or responses to improve Google products. Google logs prompts, contextual information, and output for 55 days for abuse detection, service safety and security, and required legal or regulatory disclosures. Google says abuse-monitoring data is not used to train or fine-tune AI/ML models except models used specifically for policy enforcement.
- **Google AI Studio paid access:** Google AI Studio access is treated as paid when the account has access to a Cloud Project with active billing or is a Workspace enterprise account. Google AI Studio feedback features are separately opt-in/skip-able and may collect prompts, responses, uploaded content, selected alternatives, votes, and usage details for product and model improvement.
- **Frontier Gemini model families:** The Gemini API model catalog includes frontier Gemini text/reasoning models, Live API models, TTS models, and embedding models. The general paid Gemini API data-use policy applies unless feature-specific Gemini API terms impose additional storage or use rules.

Based on those public statements, Google's paid Gemini API appears to satisfy Parley's minimum LLM privacy target for ordinary stateless API inference when Parley uses paid Gemini API quota or an equivalent paid/enterprise AI Studio path: no model/product improvement use by default and ordinary abuse-log retention of 55 days.

Important caveats:

- Public policy pages can change. Re-verify before making contractual promises.
- Abuse, safety, legal, compliance, and service-protection exceptions may allow review or longer retention.
- Gemini API paid services do not appear to offer a public zero-data-retention control equivalent to OpenAI ZDR. The documented retention number for Gemini API abuse monitoring is 55 days.
- Gemini API Grounding with Google Search and Grounding with Google Maps also document 30-day storage of prompts, contextual information, and output for creating grounded results and debugging/testing supporting systems.
- Gemini API Files API stores uploaded files for 48 hours unless manually deleted earlier. Explicit context caching stores cached tokens for a developer-selected TTL, defaulting to 1 hour if not set, and can be deleted manually.
- Gemini API Live API resumption tokens are documented as valid for 2 hours after the last session termination.
- Tuning, custom models, uploaded files, cached content, batch jobs, stored datasets, feedback, grounding, agentic tools, and persistent application features may store data by design. Parley should avoid these features for privacy-sensitive LLM use unless separately disclosed.
- Local Parley storage is separate from Google's policy. If Parley stores prompts, model outputs, transcripts, audio, uploaded files, or generated artifacts locally, that local data must be governed by Parley's own deletion and access controls.

## Subprocessor Disclosure

Parley may send user-provided text, transcript excerpts, conversation context, prompts, uploaded files, audio, images, video, generated artifacts, and related metadata to Google as a third-party Gemini API provider. Google processes that data to provide Gemini model inference and related API services. For Gemini API paid services, Google identifies Gemini API Paid Services as a data processing service under Google's Data Processing Addendum for Products Where Google is a Data Processor.

## Verification URLs

Verified 2026-04-29.

- [Gemini API Additional Terms of Service](https://ai.google.dev/gemini-api/terms) verifies paid no-product-improvement language for prompts, system instructions, cached content, files, and responses; processor-DPA processing for paid services; and feature-specific 30-day storage terms for Grounding with Google Search and Grounding with Google Maps.
- [Gemini API abuse monitoring](https://ai.google.dev/gemini-api/docs/usage-policies) verifies 55-day retention of prompts, contextual information, and output for prohibited-use detection, service safety and security, and required legal or regulatory disclosures; authorized employee review of flagged data; and no training/fine-tuning use except for policy-enforcement models.
- [Gemini API billing](https://ai.google.dev/gemini-api/docs/billing) verifies that paid tiers are Google's documented path for enterprise-grade data privacy and for ensuring prompts and responses are not used to improve Google products, and verifies AI Studio paid-service treatment when Cloud billing is enabled in the relevant account/project context.
- [Gemini API model catalog](https://ai.google.dev/gemini-api/docs/models) verifies the API-accessible frontier Gemini model categories, including Gemini text/reasoning models, audio/live models, TTS models, and embedding models.
- [Gemini API Files API](https://ai.google.dev/gemini-api/docs/files) verifies that uploaded files are stored for 48 hours, can be manually deleted earlier, and have project/per-file storage limits.
- [Gemini API context caching](https://ai.google.dev/gemini-api/docs/caching) verifies implicit caching for Gemini 2.5 and newer models and explicit caching TTL behavior, including default 1-hour TTL and manual cache deletion support.
- [Gemini API Live API session management](https://ai.google.dev/gemini-api/docs/live-api/session-management) verifies Live API session lifetime, session resumption behavior, and the documented 2-hour validity window for resumption tokens after the last session termination.
- [Google Data Processing Addendum for Products Where Google is a Data Processor](https://business.safety.google/processorterms/) verifies Google processor commitments for applicable processor services, including processing under customer instructions, deletion provisions, security measures, incident notification, access controls, confidentiality, and subprocessor commitments.
- [Google Data Protection Terms service information](https://business.safety.google/services/) verifies that Gemini API Paid Services are listed as data processing services and identifies the relevant personal data as data processed in submitted prompts and responses.
- [Google Data Processing Terms subprocessor information](https://business.safety.google/subprocessors/) verifies the public Google subprocessor information URL referenced by the processor DPA.
- [Google Cloud compliance offerings](https://cloud.google.com/security/compliance/offerings) verifies Google's public security and compliance posture, including ISO and SOC compliance offerings.
