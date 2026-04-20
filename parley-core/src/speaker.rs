//! Speaker entities — who is talking on a given lane.
//!
//! Spec references:
//! - `docs/architecture.md` — *Speaker* entity in the core data model.
//! - `docs/conversation-mode-spec.md` §1.5.1 — lane → speaker binding is the
//!   missing piece needed before Conversation Mode v1.
//!
//! ## Design notes
//!
//! - `Speaker` is a pure data record. It does not know about lanes. The
//!   `lane → Speaker` mapping lives in the session that owns both the word
//!   graph and the speaker table; this struct is just the speaker side.
//! - `SpeakerKind::AiAgent` is the structural distinction the conversation
//!   orchestrator (proxy-side) will use to route turns. Until Conversation
//!   Mode v1 lands, only the data type exists; nothing produces AI speakers.
//! - `IdentificationMethod` is small and additive: new methods (e.g., a
//!   future voice fingerprint) get a new variant without breaking serialized
//!   sessions, because we use serde's default tag handling.

use serde::{Deserialize, Serialize};

/// Stable identifier for a speaker within a session. Plain string so it can
/// be a UUID, a stable name slug, or a synthetic `"speaker_0"` label —
/// callers decide. Sessions persist this to disk, so it must remain stable
/// across reloads.
pub type SpeakerId = String;

/// What kind of voice this speaker is.
///
/// The `AiAgent` and `Human` distinction is used by the conversation
/// orchestrator: human speakers feed STT into the LLM, AI speakers receive
/// LLM output and feed it to TTS. `Unknown` is the default state when STT
/// has detected a voice but identification has not yet completed.
/// `System` is for orchestrator-emitted meta-turns (compaction summaries,
/// failure announcements) — see Conversation Mode spec §9.4 and §10.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerKind {
    /// A human voice captured from a microphone.
    Human,
    /// An AI agent producing speech via TTS.
    AiAgent,
    /// A voice detected but not yet identified.
    Unknown,
    /// Orchestrator-generated meta-turn (compaction summaries, failure
    /// announcements). Not a real voice; carries structural conversation
    /// metadata.
    System,
}

/// How the system arrived at this speaker's identity.
///
/// Distinct from `SpeakerKind` because the *method* is independent of the
/// *kind*: a `Human` may be `Unidentified`, `SelfIntroduced`, `Manual`, or
/// (eventually) `VoiceFingerprint`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentificationMethod {
    /// No identification attempted or none yet succeeded. Default for fresh
    /// STT-detected speakers.
    Unidentified,
    /// Speaker said their name during the introductions phase
    /// (Conversation Mode spec §7.2).
    SelfIntroduced,
    /// User manually labeled this lane via a UI affordance or speaker
    /// button (spec §7.3).
    Manual,
    /// Matched against a stored voice embedding from a previous session.
    /// Reserved for future use; no production code path produces this yet.
    VoiceFingerprint,
    /// Speaker is a configured AI persona — identity comes from the
    /// persona definition, not from any acoustic signal.
    PersonaConfig,
}

/// A known or unknown voice identity participating in a session.
///
/// Created by:
/// - The STT pipeline when a new voice is detected (`kind = Unknown`,
///   `method = Unidentified`).
/// - The conversation orchestrator when an AI persona joins
///   (`kind = AiAgent`, `method = PersonaConfig`).
/// - The orchestrator when emitting a meta-turn (`kind = System`).
/// - Self-introduction or manual tagging (promotes `kind` from `Unknown`
///   to `Human` and updates `method`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Speaker {
    /// Stable identifier within the owning session.
    pub id: SpeakerId,
    /// Display label as the user has named this speaker — `"Gavin"`,
    /// `"Speaker_0"`, `"Theological Scholar"`, etc. May change over a
    /// session's lifetime as identification improves.
    pub label: String,
    /// What kind of voice this is (see `SpeakerKind`).
    pub kind: SpeakerKind,
    /// How the current identity was established.
    pub method: IdentificationMethod,
    /// 0.0–1.0 confidence in the current identification. `1.0` for
    /// `Manual` and `PersonaConfig`. STT-derived identifications carry
    /// the diarizer's reported confidence. Speakers with no
    /// identification attempt yet default to `0.0`.
    #[serde(default)]
    pub confidence: f32,
    /// Optional reference to a stored voice embedding (e.g., a row id in
    /// a future fingerprint store). `None` until voice fingerprinting is
    /// implemented; the field exists now so persisted sessions remain
    /// forward-compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_embedding_id: Option<String>,
}

impl Speaker {
    /// Build an `Unknown` / `Unidentified` placeholder for a freshly
    /// detected voice. The label uses a `"speaker_<n>"` convention that
    /// callers can override later as identification improves.
    pub fn unknown(id: impl Into<SpeakerId>, lane_index: u8) -> Self {
        let id = id.into();
        Self {
            id,
            label: format!("Speaker_{lane_index}"),
            kind: SpeakerKind::Unknown,
            method: IdentificationMethod::Unidentified,
            confidence: 0.0,
            voice_embedding_id: None,
        }
    }

    /// Build a manually-labeled human speaker. Used by the manual-tagging
    /// fallback (Conversation Mode spec §7.3) and by tests.
    pub fn manual_human(id: impl Into<SpeakerId>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind: SpeakerKind::Human,
            method: IdentificationMethod::Manual,
            confidence: 1.0,
            voice_embedding_id: None,
        }
    }

    /// Build an AI-agent speaker tied to a persona configuration.
    /// `id` is typically the persona id; `label` is its display name.
    pub fn ai_agent(id: impl Into<SpeakerId>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind: SpeakerKind::AiAgent,
            method: IdentificationMethod::PersonaConfig,
            confidence: 1.0,
            voice_embedding_id: None,
        }
    }

    /// Build a system speaker for orchestrator meta-turns.
    pub fn system() -> Self {
        Self {
            id: "system".into(),
            label: "System".into(),
            kind: SpeakerKind::System,
            method: IdentificationMethod::PersonaConfig,
            confidence: 1.0,
            voice_embedding_id: None,
        }
    }

    /// `true` if this speaker is a real voice (human or AI), `false` for
    /// system meta-turns. Used by the orchestrator to decide whether a
    /// turn is part of the conversational record or a structural marker.
    pub fn is_voice(&self) -> bool {
        matches!(self.kind, SpeakerKind::Human | SpeakerKind::AiAgent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_speaker_uses_lane_indexed_default_label() {
        let s = Speaker::unknown("s0", 3);
        assert_eq!(s.id, "s0");
        assert_eq!(s.label, "Speaker_3");
        assert_eq!(s.kind, SpeakerKind::Unknown);
        assert_eq!(s.method, IdentificationMethod::Unidentified);
        assert_eq!(s.confidence, 0.0);
        assert!(s.voice_embedding_id.is_none());
    }

    #[test]
    fn manual_human_has_full_confidence_and_human_kind() {
        let s = Speaker::manual_human("gavin", "Gavin");
        assert_eq!(s.kind, SpeakerKind::Human);
        assert_eq!(s.method, IdentificationMethod::Manual);
        assert_eq!(s.confidence, 1.0);
        assert!(s.is_voice());
    }

    #[test]
    fn ai_agent_speaker_marks_kind_aiagent_and_method_personaconfig() {
        let s = Speaker::ai_agent("theology-scholar", "Theological Scholar");
        assert_eq!(s.kind, SpeakerKind::AiAgent);
        assert_eq!(s.method, IdentificationMethod::PersonaConfig);
        assert!(s.is_voice());
    }

    #[test]
    fn system_speaker_is_not_a_voice() {
        let s = Speaker::system();
        assert_eq!(s.kind, SpeakerKind::System);
        assert!(!s.is_voice());
    }

    #[test]
    fn speaker_kind_variants_are_distinct() {
        let all = [
            SpeakerKind::Human,
            SpeakerKind::AiAgent,
            SpeakerKind::Unknown,
            SpeakerKind::System,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                assert_eq!(i == j, a == b);
            }
        }
    }

    #[test]
    fn identification_method_variants_are_distinct() {
        let all = [
            IdentificationMethod::Unidentified,
            IdentificationMethod::SelfIntroduced,
            IdentificationMethod::Manual,
            IdentificationMethod::VoiceFingerprint,
            IdentificationMethod::PersonaConfig,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                assert_eq!(i == j, a == b);
            }
        }
    }

    // ── Serialization round-trips ──────────────────────────────────

    #[test]
    fn serde_roundtrip_preserves_speaker_state() {
        let original = Speaker::manual_human("gavin", "Gavin");
        let json = serde_json::to_string(&original).expect("serialize");
        let back: Speaker = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, back);
    }

    #[test]
    fn serde_uses_snake_case_for_kind_and_method() {
        let s = Speaker::ai_agent("p1", "Persona One");
        let json = serde_json::to_string(&s).expect("serialize");
        // Tags must be snake_case so persisted sessions read identically
        // across renamings of the Rust identifiers.
        assert!(
            json.contains("\"ai_agent\""),
            "kind should serialize as ai_agent: {json}"
        );
        assert!(
            json.contains("\"persona_config\""),
            "method should serialize as persona_config: {json}"
        );
    }

    #[test]
    fn serde_default_for_confidence_when_missing() {
        // A persisted speaker missing the `confidence` field should
        // deserialize as 0.0 — this guards forward-compatibility for
        // sessions written before the field was added.
        let json = r#"{
            "id": "s0",
            "label": "Speaker_0",
            "kind": "unknown",
            "method": "unidentified"
        }"#;
        let s: Speaker = serde_json::from_str(json).expect("deserialize");
        assert_eq!(s.confidence, 0.0);
        assert!(s.voice_embedding_id.is_none());
    }

    #[test]
    fn serde_omits_voice_embedding_id_when_none() {
        let s = Speaker::manual_human("gavin", "Gavin");
        let json = serde_json::to_string(&s).expect("serialize");
        assert!(
            !json.contains("voice_embedding_id"),
            "None embedding should be skipped: {json}"
        );
    }

    #[test]
    fn serde_preserves_voice_embedding_id_when_some() {
        let s = Speaker {
            voice_embedding_id: Some("emb-42".into()),
            ..Speaker::manual_human("gavin", "Gavin")
        };
        let json = serde_json::to_string(&s).expect("serialize");
        let back: Speaker = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.voice_embedding_id.as_deref(), Some("emb-42"));
    }
}
