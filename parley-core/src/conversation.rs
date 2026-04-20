//! Conversation history and turn types.
//!
//! A [`ConversationSession`] is the persistable record of a turn-based
//! exchange between humans and AI agents. It lives in `parley-core`
//! (not the proxy) because it crosses the WASM frontend ↔ proxy
//! boundary and will eventually be serialized to disk in the session
//! file (spec §13).
//!
//! The runtime *behavior* — driving the state machine, dispatching to
//! providers, streaming tokens — is the
//! [`ConversationOrchestrator`](../../../parley_proxy/orchestrator/struct.ConversationOrchestrator.html)
//! and lives in the proxy.
//!
//! Spec references: `docs/conversation-mode-spec.md` §3.2, §6.3, §13.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::chat::{ChatMessage, ChatRole, Cost, TokenUsage};
use crate::model_config::ModelConfigId;
use crate::persona::PersonaId;
use crate::speaker::{Speaker, SpeakerId};

/// Stable identifier for a single turn within a session. The
/// orchestrator assigns these monotonically as turns are appended.
pub type TurnId = String;

/// Stable identifier for a conversation session. The orchestrator (or
/// the persistence layer) assigns it; the type is `String` for the
/// same reason `SpeakerId` is — it round-trips through the session
/// file unchanged.
pub type SessionId = String;

/// One turn in the conversation. A turn is a single contribution by
/// one speaker, derived from the annotated stream but recorded
/// directly here for the orchestrator's consumption (spec §3.2).
///
/// User and AI turns share this struct; the [`provenance`] field
/// distinguishes them — `None` for user/system turns, `Some` for AI
/// turns. The [`role`] field aligns with [`ChatRole`] so a turn
/// converts to a [`ChatMessage`] without re-mapping.
///
/// [`provenance`]: Turn::provenance
/// [`role`]: Turn::role
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// Monotonic id within the session. Assigned by the orchestrator.
    pub id: TurnId,
    /// Who spoke this turn, by speaker id. Resolves against the
    /// session's [`speakers`](ConversationSession::speakers) table.
    pub speaker_id: SpeakerId,
    /// Conversational role for LLM dispatch. User turns are
    /// `ChatRole::User`; AI turns are `ChatRole::Assistant`; summary
    /// turns produced by compaction are `ChatRole::System`.
    pub role: ChatRole,
    /// Pre-rendered text of the turn. For AI turns this is the
    /// model's completed response; for user turns it is the typed or
    /// transcribed input.
    pub content: String,
    /// Wall-clock timestamp when the turn was finalized, in
    /// milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// Provenance — populated only for AI turns. Carries the persona
    /// and model that produced the turn plus the resulting token usage
    /// and cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<TurnProvenance>,
}

/// Provenance metadata for an AI-generated turn. Lets the session
/// file answer "which persona+model produced this, and what did it
/// cost" without re-deriving from logs.
///
/// Spec references: §6.3 (mid-session switching captured in
/// provenance), §11 (cost tracking).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnProvenance {
    /// Persona that was active when this turn was dispatched.
    pub persona_id: PersonaId,
    /// Model config the persona resolved to for this turn. Recorded
    /// because a persona's model can be swapped mid-session.
    pub model_config_id: ModelConfigId,
    /// Final token accounting from the provider.
    pub usage: TokenUsage,
    /// Computed USD cost (usage × model rates).
    pub cost: Cost,
}

/// Records a single (persona, model) activation window — used to
/// reconstruct the sequence of personas a session moved through
/// (spec §6.3 + §13.3 frontmatter `personas_used`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaActivation {
    /// Persona id that became active at this point.
    pub persona_id: PersonaId,
    /// Model config the persona was paired with at activation.
    pub model_config_id: ModelConfigId,
    /// Index into [`ConversationSession::turns`] where this
    /// activation begins. The activation runs until the next entry in
    /// [`ConversationSession::persona_history`] or the end of the
    /// session.
    pub from_turn_index: usize,
}

/// A complete conversation. Holds the turn history, the speakers
/// participating, and the timeline of which (persona, model) pair was
/// active across the session.
///
/// **Mutation policy:** the orchestrator owns the session and is the
/// only place that calls [`Self::append_user_turn`] /
/// [`Self::append_ai_turn`] / [`Self::switch_persona`]. The session's
/// invariants — monotonic ids, persona-history alignment with the
/// turn vector — are enforced through these methods, not through
/// public field access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSession {
    /// Stable session id (filename stem, etc.).
    pub id: SessionId,
    /// All turns in the order they occurred.
    pub turns: Vec<Turn>,
    /// Speakers participating in this session, keyed by id. The
    /// orchestrator inserts new speakers as they're discovered; AI
    /// agents and the system speaker are pre-inserted at session
    /// construction.
    pub speakers: HashMap<SpeakerId, Speaker>,
    /// History of persona/model activations. Always non-empty: the
    /// constructor seeds index 0 with the initial persona.
    pub persona_history: Vec<PersonaActivation>,
    /// Counter for turn id generation. Not persisted as a field of
    /// truth — derive from `turns.len()` on load if needed; here it
    /// just keeps the constructor honest about where the next id
    /// comes from.
    #[serde(default)]
    next_turn_seq: u64,
}

impl ConversationSession {
    /// Construct a fresh session with an initial active persona +
    /// model and the AI speaker for that persona pre-registered.
    ///
    /// `ai_speaker` is the [`Speaker`] entry that will own AI turns
    /// produced under the initial persona; the orchestrator typically
    /// builds this via [`Speaker::ai_agent`]. Callers can register
    /// additional speakers (humans, other AI agents) through
    /// [`Self::register_speaker`].
    pub fn new(
        id: impl Into<SessionId>,
        ai_speaker: Speaker,
        active_persona: PersonaId,
        model_config_id: ModelConfigId,
    ) -> Self {
        let mut speakers = HashMap::new();
        speakers.insert(ai_speaker.id.clone(), ai_speaker);
        let persona_history = vec![PersonaActivation {
            persona_id: active_persona,
            model_config_id,
            from_turn_index: 0,
        }];
        Self {
            id: id.into(),
            turns: Vec::new(),
            speakers,
            persona_history,
            next_turn_seq: 0,
        }
    }

    /// Register (or replace) a speaker. The orchestrator calls this
    /// when a new human is identified or when an AI agent for a newly
    /// activated persona joins.
    pub fn register_speaker(&mut self, speaker: Speaker) {
        self.speakers.insert(speaker.id.clone(), speaker);
    }

    /// The (persona, model) pair currently in effect. Always present
    /// because [`Self::new`] seeds the history.
    pub fn active(&self) -> &PersonaActivation {
        self.persona_history
            .last()
            .expect("persona_history is non-empty by construction")
    }

    /// Switch the active (persona, model). Records the activation
    /// against the *next* turn index; the next turn appended will be
    /// attributed to this new persona.
    ///
    /// No-op when both ids match the current activation — avoids
    /// littering history with redundant entries.
    pub fn switch_persona(&mut self, persona_id: PersonaId, model_config_id: ModelConfigId) {
        let current = self.active();
        if current.persona_id == persona_id && current.model_config_id == model_config_id {
            return;
        }
        self.persona_history.push(PersonaActivation {
            persona_id,
            model_config_id,
            from_turn_index: self.turns.len(),
        });
    }

    /// Append a user-spoken (or user-typed) turn. Returns the new
    /// turn's id so the caller can correlate with downstream events.
    pub fn append_user_turn(
        &mut self,
        speaker_id: SpeakerId,
        content: String,
        timestamp_ms: u64,
    ) -> TurnId {
        let id = self.next_id();
        self.turns.push(Turn {
            id: id.clone(),
            speaker_id,
            role: ChatRole::User,
            content,
            timestamp_ms,
            provenance: None,
        });
        id
    }

    /// Append a completed AI turn. The orchestrator builds the
    /// provenance from the active (persona, model) plus the
    /// provider's reported usage and computed cost.
    pub fn append_ai_turn(
        &mut self,
        speaker_id: SpeakerId,
        content: String,
        timestamp_ms: u64,
        provenance: TurnProvenance,
    ) -> TurnId {
        let id = self.next_id();
        self.turns.push(Turn {
            id: id.clone(),
            speaker_id,
            role: ChatRole::Assistant,
            content,
            timestamp_ms,
            provenance: Some(provenance),
        });
        id
    }

    /// Append a system turn (used by compaction in a later slice;
    /// included now so the data shape is stable). Spec §9.4.
    pub fn append_system_turn(&mut self, content: String, timestamp_ms: u64) -> TurnId {
        // Reuse the system speaker if present; otherwise create one.
        let system_id = "system".to_string();
        if !self.speakers.contains_key(&system_id) {
            self.speakers.insert(system_id.clone(), Speaker::system());
        }
        let id = self.next_id();
        self.turns.push(Turn {
            id: id.clone(),
            speaker_id: system_id,
            role: ChatRole::System,
            content,
            timestamp_ms,
            provenance: None,
        });
        id
    }

    /// Render the session's turns as a `Vec<ChatMessage>` suitable
    /// for handing to an `LlmProvider`. The orchestrator prepends the
    /// resolved persona system prompt before sending; this method
    /// only emits the *history*.
    pub fn to_chat_messages(&self) -> Vec<ChatMessage> {
        self.turns
            .iter()
            .map(|t| ChatMessage {
                role: t.role,
                content: t.content.clone(),
            })
            .collect()
    }

    fn next_id(&mut self) -> TurnId {
        let id = format!("turn-{:04}", self.next_turn_seq);
        self.next_turn_seq += 1;
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speaker::Speaker;

    fn ai() -> Speaker {
        Speaker::ai_agent("ai-scholar", "Scholar")
    }

    fn fresh() -> ConversationSession {
        ConversationSession::new("sess-1", ai(), "scholar".into(), "claude-x".into())
    }

    #[test]
    fn new_seeds_active_persona_and_ai_speaker() {
        let s = fresh();
        assert_eq!(s.active().persona_id, "scholar");
        assert_eq!(s.active().model_config_id, "claude-x");
        assert_eq!(s.active().from_turn_index, 0);
        assert!(s.speakers.contains_key("ai-scholar"));
    }

    #[test]
    fn turn_ids_are_monotonic() {
        let mut s = fresh();
        let a = s.append_user_turn("gavin".into(), "hi".into(), 1);
        let b = s.append_user_turn("gavin".into(), "still here".into(), 2);
        assert_eq!(a, "turn-0000");
        assert_eq!(b, "turn-0001");
    }

    #[test]
    fn user_turn_has_no_provenance_ai_turn_does() {
        let mut s = fresh();
        s.append_user_turn("gavin".into(), "hi".into(), 1);
        let prov = TurnProvenance {
            persona_id: "scholar".into(),
            model_config_id: "claude-x".into(),
            usage: TokenUsage {
                input: 10,
                output: 5,
            },
            cost: Cost::from_usd(0.01),
        };
        s.append_ai_turn("ai-scholar".into(), "hello".into(), 2, prov.clone());
        assert!(s.turns[0].provenance.is_none());
        assert_eq!(s.turns[1].provenance.as_ref().unwrap(), &prov);
    }

    #[test]
    fn switch_persona_appends_activation_at_next_turn_index() {
        let mut s = fresh();
        s.append_user_turn("gavin".into(), "hi".into(), 1);
        s.switch_persona("editor".into(), "gpt-mini".into());
        assert_eq!(s.persona_history.len(), 2);
        assert_eq!(s.persona_history[1].persona_id, "editor");
        assert_eq!(s.persona_history[1].from_turn_index, 1);
    }

    #[test]
    fn switch_persona_is_noop_when_identical() {
        let mut s = fresh();
        s.switch_persona("scholar".into(), "claude-x".into());
        s.switch_persona("scholar".into(), "claude-x".into());
        assert_eq!(s.persona_history.len(), 1);
    }

    #[test]
    fn switch_persona_records_change_when_only_model_differs() {
        let mut s = fresh();
        s.switch_persona("scholar".into(), "claude-y".into());
        assert_eq!(s.persona_history.len(), 2);
        assert_eq!(s.persona_history[1].model_config_id, "claude-y");
    }

    #[test]
    fn to_chat_messages_emits_history_in_order() {
        let mut s = fresh();
        s.append_user_turn("gavin".into(), "hi".into(), 1);
        s.append_ai_turn(
            "ai-scholar".into(),
            "hello".into(),
            2,
            TurnProvenance {
                persona_id: "scholar".into(),
                model_config_id: "claude-x".into(),
                usage: TokenUsage::default(),
                cost: Cost::default(),
            },
        );
        let msgs = s.to_chat_messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert_eq!(msgs[0].content, "hi");
        assert_eq!(msgs[1].role, ChatRole::Assistant);
        assert_eq!(msgs[1].content, "hello");
    }

    #[test]
    fn append_system_turn_creates_system_speaker_if_missing() {
        let mut s = fresh();
        assert!(!s.speakers.contains_key("system"));
        s.append_system_turn("compaction summary".into(), 5);
        assert!(s.speakers.contains_key("system"));
        assert_eq!(s.turns[0].role, ChatRole::System);
    }

    #[test]
    fn session_round_trips_through_json() {
        let mut s = fresh();
        s.append_user_turn("gavin".into(), "hi".into(), 1);
        s.append_ai_turn(
            "ai-scholar".into(),
            "hello".into(),
            2,
            TurnProvenance {
                persona_id: "scholar".into(),
                model_config_id: "claude-x".into(),
                usage: TokenUsage {
                    input: 1,
                    output: 1,
                },
                cost: Cost::from_usd(0.001),
            },
        );
        let json = serde_json::to_string(&s).unwrap();
        let parsed: ConversationSession = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.turns.len(), 2);
        assert_eq!(parsed.persona_history.len(), 1);
        assert_eq!(parsed.id, "sess-1");
    }

    #[test]
    fn register_speaker_inserts_or_replaces() {
        let mut s = fresh();
        s.register_speaker(Speaker::manual_human("gavin", "Gavin"));
        assert_eq!(s.speakers.len(), 2);
        // Replacing same id should not grow the map.
        s.register_speaker(Speaker::manual_human("gavin", "Gavin H."));
        assert_eq!(s.speakers.len(), 2);
        assert_eq!(s.speakers["gavin"].label, "Gavin H.");
    }
}
