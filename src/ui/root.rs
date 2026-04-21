//! Top-level shell that lets the user pick between the existing
//! transcription UI (`app::App`) and the new conversation UI
//! (`conversation::ConversationView`).
//!
//! The toggle state is held in a single `use_signal` here. We don't
//! use a router — there's only one decision to make, and a router
//! would haul in surface area we don't otherwise need yet.

use dioxus::prelude::*;

use crate::ui::app::App;
use crate::ui::conversation::ConversationView;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Transcribe,
    Converse,
}

#[component]
pub fn Root() -> Element {
    let mut mode = use_signal(|| Mode::Transcribe);

    rsx! {
        div { class: "parley-shell",
            // Compact mode toggle pinned to the top of the page. Kept
            // above the existing transcription UI so it can't disrupt
            // any existing layout assumptions inside `App`.
            div { style: "display: flex; gap: 0.5rem; padding: 0.5rem 1rem; border-bottom: 1px solid #ddd; background: #f7f7f7;",
                button {
                    style: tab_style(*mode.read() == Mode::Transcribe),
                    onclick: move |_| mode.set(Mode::Transcribe),
                    "Transcribe"
                }
                button {
                    style: tab_style(*mode.read() == Mode::Converse),
                    onclick: move |_| mode.set(Mode::Converse),
                    "Conversation"
                }
            }
            // Render BOTH subtrees and toggle visibility with CSS.
            // A `match` here would unmount the inactive view and
            // discard every `use_signal` it owns — meaning a stray
            // tab click would silently throw away the user's
            // in-progress conversation or transcription. Keeping
            // both mounted preserves all local state at the cost of
            // a tiny amount of layout work for the hidden subtree.
            //
            // Caveat: any background work either subtree starts
            // (e.g. the transcription view's audio capture) keeps
            // running while hidden. That's fine today because both
            // views are user-initiated — capture only starts when
            // you click Record, and the conversation view is idle
            // until you send a turn.
            div { style: if *mode.read() == Mode::Transcribe { "display: block;" } else { "display: none;" },
                App {}
            }
            div { style: if *mode.read() == Mode::Converse { "display: block;" } else { "display: none;" },
                ConversationView {}
            }
        }
    }
}

fn tab_style(active: bool) -> String {
    let bg = if active { "#1976d2" } else { "#fff" };
    let fg = if active { "#fff" } else { "#333" };
    format!(
        "padding: 0.4rem 0.9rem; font-size: 0.95rem; cursor: pointer; \
         border: 1px solid #bbb; border-radius: 4px; background: {bg}; color: {fg};"
    )
}
