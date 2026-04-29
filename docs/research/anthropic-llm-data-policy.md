# Anthropic LLM Data Policy Notes

**Status:** Active
**Type:** Research
**Audience:** Both
**Date:** 2026-04-28

## Summary

This note summarizes the public Anthropic data-use, training, retention, and subprocessor policy surface relevant to using Anthropic LLMs in Parley through Anthropic API endpoints. It is written for people who want to verify that Parley is not accidentally using their words, ideas, confidential material, or other intellectual property in a way that gives away their IP or trains a third-party model.

This is an engineering policy note, not legal advice. It records the public policy surface Parley relies on as of 2026-04-28.

Parley's intended Anthropic usage relies on Anthropic's commercial API policy surface, not consumer Claude Free, Pro, or Max. Under Anthropic's public commercial terms, privacy-center articles, and data processing addendum:

- Anthropic says Customer Content from the Services may not be used to train models.
- Anthropic says it will not use inputs or outputs from commercial products, including the Anthropic API, to train models by default.
- Anthropic says API inputs and outputs are automatically deleted from backend systems within 30 days of receipt or generation, except for longer-retention services under customer control, zero-retention agreements, usage-policy enforcement, legal requirements, or other agreed terms.
- Anthropic says some enterprise API customers may receive zero data retention arrangements, subject to Anthropic approval, where Anthropic does not store inputs or outputs except where needed to comply with law or combat misuse.

Based on those public statements, Anthropic appears to satisfy Parley's current LLM privacy target: no model training by default and ordinary API input/output retention under 90 days.

Important caveats:

- Public policy pages can change. Re-verify before making contractual promises.
- Usage-policy, safety, legal, compliance, and abuse-investigation exceptions may allow review or longer retention.
- Anthropic says inputs and outputs flagged by trust and safety classifiers as violating the Usage Policy may be retained for up to 2 years, and trust and safety classification scores may be retained for up to 7 years.
- Feedback and bug reports are a separate data path. Anthropic says feedback data may be retained for up to 5 years and may be used for model training as permitted by applicable law.
- Customer-controlled storage features, such as the Files API or other products that save conversations or sessions, can have longer retention. Parley should avoid those features or document their deletion controls separately.
- Local Parley storage is separate from Anthropic's policy. If Parley stores prompts, model outputs, transcripts, or generated artifacts locally, that local data must be governed by Parley's own deletion and access controls.

## Subprocessor Disclosure

Parley may send user-provided text, transcript excerpts, conversation context, prompts, and related metadata to Anthropic as a third-party LLM API provider. Anthropic processes that data to provide model inference and related API services. Anthropic maintains a public subprocessor list through its Trust Center.

## Verification URLs

Verified 2026-04-28.

- Anthropic Commercial Terms of Service: https://www.anthropic.com/legal/commercial-terms
    - Verifies that Anthropic's commercial terms govern Anthropic API keys and related services.
    - Verifies that Anthropic says Customer Content includes Inputs and Outputs.
    - Verifies that Anthropic says it may not train models on Customer Content from Services.

- Anthropic Privacy Center, commercial model training: https://privacy.claude.com/en/articles/7996868-is-my-data-used-for-model-training
    - Verifies that Anthropic says it will not use inputs or outputs from commercial products, including the Anthropic API, to train models by default.
    - Verifies the feedback and explicit opt-in caveat.

- Anthropic Privacy Center, organization data retention: https://privacy.claude.com/en/articles/7996866-how-long-do-you-store-my-organization-s-data
    - Verifies that Anthropic says API inputs and outputs are automatically deleted from backend systems within 30 days of receipt or generation.
    - Verifies exceptions for longer-retention services, zero data retention agreements, Usage Policy enforcement, and legal compliance.
    - Verifies the public trust-and-safety retention statements for flagged inputs/outputs and classifier scores.

- Anthropic zero data retention article: https://privacy.claude.com/en/articles/8956058-i-have-a-zero-data-retention-agreement-with-anthropic-what-products-does-it-apply-to
    - Verifies that some enterprise API customers may have zero data retention arrangements, subject to Anthropic approval.
    - Verifies that those arrangements apply only to eligible Anthropic APIs and Anthropic products using the commercial organization API key.

- Anthropic Data Processing Addendum: https://www.anthropic.com/legal/data-processing-addendum
    - Verifies processor/service-provider style commitments for Customer Personal Data.
    - Verifies that Anthropic processes Customer Personal Data to provide or maintain the Services and follow documented instructions unless required by law.
    - Verifies deletion and subprocessor commitments.

- Anthropic Trust Center subprocessors: https://trust.anthropic.com/subprocessors
    - Verifies the public Anthropic subprocessor list used for commercial/API products.

- Anthropic API overview: https://platform.claude.com/docs/en/api/getting-started
    - Verifies that Anthropic exposes direct API endpoints for Claude model access and identifies the direct Claude API as a RESTful API.
