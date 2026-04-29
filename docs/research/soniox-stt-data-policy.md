# Soniox STT Data Policy Notes

**Status:** Active
**Type:** Research
**Audience:** Both
**Date:** 2026-04-28

## Summary

This note summarizes the public Soniox data-use, training, retention, and subprocessor-adjacent policy surface relevant to using Soniox Speech-to-Text in Parley through Soniox API endpoints. It is written for people who want to verify that Parley is not accidentally using their words, audio, transcripts, confidential material, or other intellectual property in a way that gives away their IP or trains a third-party model.

This is an engineering policy note, not legal advice. It records the public policy surface Parley relies on as of 2026-04-28.

Parley's intended Soniox usage relies on Soniox Speech-to-Text APIs for real-time and/or async transcription. Under Soniox's public API documentation, security and privacy page, and data residency page:

- Soniox says content sent to the Soniox API, including audio, transcripts, and metadata, is never used to train or improve Soniox models.
- Soniox says audio and transcripts are never used to improve Soniox models or services.
- Soniox says it does not store audio or transcript data unless explicitly requested through a service that supports storage, such as the async API.
- Soniox says stored audio and transcripts can be deleted at any time through the Soniox Console or API.
- Soniox says minimal logging is performed for reliability, debugging, and billing, and that logs never contain raw audio or transcript content.
- Soniox says diagnostic metadata such as request IDs or error traces may be retained temporarily for operational purposes.

Based on those public statements, Soniox appears to satisfy Parley's current STT privacy target for ordinary real-time transcription: no model training and no default retention of raw audio or transcript content. For async transcription, Soniox's public docs indicate storage is customer-requested and customer-deletable, but do not provide a fixed automatic deletion window for stored async audio/transcripts.

Important caveats:

- Public policy pages can change. Re-verify before making contractual promises.
- Soniox's public docs found in this research do not state a specific retention period for diagnostic metadata beyond saying it may be retained temporarily.
- Soniox's public docs found in this research do not state a fixed automatic deletion period for customer-stored async audio or transcript objects. If Parley uses Soniox async storage, Parley should delete stored Soniox files/transcriptions after processing and document that application-level deletion behavior.
- Legal, compliance, support, security, billing, and abuse-investigation exceptions may exist in contractual terms or private DPA/MSA documents that were not publicly retrievable from the tested legal URLs.
- Soniox says compliance documentation, including legal/compliance documents, can be obtained through the Soniox Console security and compliance section.
- Local Parley storage is separate from Soniox's policy. If Parley stores source audio, transcripts, or derived metadata locally, that local data must be governed by Parley's own deletion and access controls.

## Subprocessor Disclosure

Parley may send user audio, streamed speech frames, uploaded audio files, transcript context, and related metadata to Soniox as a third-party Speech-to-Text API provider. Soniox processes that data to provide transcription, translation, diarization, and related speech-processing services. Public Soniox docs identify compliance documentation access through the Soniox Console, but this research did not find a public Soniox subprocessor list at the tested URLs.

## Verification URLs

Verified 2026-04-28.

- Soniox security and privacy documentation: https://soniox.com/docs/security-and-privacy
    - Verifies that Soniox says audio and transcripts are never used to improve Soniox models or services.
    - Verifies that Soniox says it does not store audio or transcript data unless explicitly requested through a storage-supporting service such as async API.
    - Verifies that Soniox says stored audio and transcripts can be deleted through the Console or API.
    - Verifies that Soniox says logs never contain raw audio or transcript content and that diagnostic metadata may be retained temporarily.
    - Verifies Soniox's public compliance claims and the Console location for compliance documentation.

- Soniox data residency documentation: https://soniox.com/docs/data-residency
    - Verifies that Soniox says content sent to the Soniox API, including audio, transcripts, or metadata, is never used to train or improve Soniox models.
    - Verifies regional processing and storage controls for content data.
    - Verifies that audio and transcript data remain in the selected region when data residency is enabled.

- Soniox Speech-to-Text getting started documentation: https://soniox.com/docs/stt/get-started
    - Verifies that Soniox Speech-to-Text supports real-time and file-based audio transcription through API usage.
    - Verifies that Soniox API keys are project-scoped and managed through the Soniox Console.

- Soniox API reference: https://soniox.com/docs/api-reference
    - Verifies the Soniox Speech-to-Text REST and WebSocket API surfaces.
    - Verifies that Speech-to-Text REST APIs include file and transcription management.
    - Verifies regional API domain guidance through the data residency guide.
