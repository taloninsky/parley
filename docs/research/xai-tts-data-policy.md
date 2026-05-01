# xAI TTS Data Policy Notes

**Status:** Active
**Type:** Research
**Audience:** Both
**Date:** 2026-04-28

## Summary

This note summarizes the public xAI data-use, training, and retention policies relevant to using xAI Text to Speech in Parley. It is written for people who want to verify that Parley is not accidentally using their words, ideas, confidential material, or other intellectual property in a way that gives away their IP or trains a third-party model.

This is an engineering policy note, not legal advice. It records the public policy surface Parley relies on as of 2026-04-28.

Parley's intended xAI TTS usage relies on the xAI API policy surface, not the consumer SuperGrok/Grok chat product. Under xAI's public API, business, and enterprise terms:

- xAI says it does not train on API inputs or outputs without explicit permission.
- xAI says business inputs and outputs are not used for model training.
- xAI says API requests and responses are retained for 30 days for abuse/misuse auditing and then automatically deleted.

Based on those public statements, xAI appears to satisfy Parley's current TTS privacy target: no model training by default and ordinary retention under 90 days.

Important caveats:

- Public policy pages can change. Re-verify before making contractual promises.
- Safety, moderation, legal, compliance, and abuse-investigation exceptions may allow review or longer retention.
- No separate TTS-only privacy policy was found; this note applies xAI's general API input/output policy to xAI TTS because TTS is documented as part of the xAI API.
- Local Parley storage is separate from xAI's policy. If Parley stores source text or generated audio locally, that local data must be governed by Parley's own deletion and access controls.

## Verification URLs

Verified 2026-04-28.

- xAI API Security FAQ: https://docs.x.ai/developers/faq/security
    - Verifies that xAI says it never trains on API inputs or outputs without explicit permission.
    - Verifies the public 30-day API request/response retention statement.
    - Verifies enterprise Zero Data Retention availability.

- xAI Data Processing Addendum: https://x.ai/legal/data-processing-addendum
    - Verifies the privacy terms that apply when xAI processes personal data for API/business customers.
    - Verifies processor/service-provider style commitments, deletion commitments, and security commitments.
    
- xAI Voice API documentation: https://docs.x.ai/developers/model-capabilities/audio/voice
    - Verifies that xAI Text to Speech is part of the xAI Voice/API documentation surface.

- xAI Voice REST API reference: https://docs.x.ai/developers/rest-api-reference/inference/voice
    - Verifies the current xAI Voice API reference, including Text to Speech and related voice endpoints.

- xAI Subprocessor List: https://x.ai/legal/subprocessor-list
    - Verifies the subprocessors xAI identifies for processing activities.
