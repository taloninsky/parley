//! Top-level shell that lets the user pick between the existing
//! transcription UI (`app::App`) and the new conversation UI
//! (`conversation::ConversationView`).
//!
//! The toggle state is held in a single `use_signal` here. We don't
//! use a router — there's only one decision to make, and a router
//! would haul in surface area we don't otherwise need yet.

use dioxus::prelude::*;

use crate::ui::app::App;
use crate::ui::app_state::AppSettings;
use crate::ui::conversation::ConversationView;
use crate::ui::settings_drawer::SettingsDrawer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Transcribe,
    Converse,
}

#[component]
pub fn Root() -> Element {
    let mut mode = use_signal(|| Mode::Transcribe);

    // Lifted settings state. Provided once here so both the gear
    // button below, the `App` (Transcribe) view, the
    // `ConversationView`, and the shared `SettingsDrawer` overlay
    // see the same handles. Spec: "full lift to Root" path.
    let settings = AppSettings::init();
    use_context_provider(|| settings);
    let mut show_settings = settings.show_settings;

    rsx! {
        div { class: "parley-shell",
            // Compact mode toggle pinned to the top of the page. Kept
            // above the existing transcription UI so it can't disrupt
            // any existing layout assumptions inside `App`. Styled to
            // match the dark palette used by both child views.
            div { style: "display: flex; gap: 0.5rem; padding: 0.5rem 1rem; border-bottom: 1px solid #2a3960; background: #16213e; align-items: center;",
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
                // Spacer pushes the gear to the far right of the bar.
                div { style: "flex: 1;" }
                button {
                    style: gear_style(show_settings()),
                    title: "Settings",
                    onclick: move |_| show_settings.set(!show_settings()),
                    "\u{2699}\u{fe0f}"
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
            // Drawer is mounted unconditionally and self-gates on the
            // shared `show_settings` signal — keeps it visible above
            // whichever view is active.
            SettingsDrawer {}
        }
    }
}

fn tab_style(active: bool) -> String {
    let bg = if active { "#0f3460" } else { "#1a1a2e" };
    let fg = if active { "#e0e0e0" } else { "#8888aa" };
    let border = if active { "#4ecca3" } else { "#2a3960" };
    format!(
        "padding: 0.4rem 0.9rem; font-size: 0.95rem; cursor: pointer; \
         border: 1px solid {border}; border-radius: 4px; background: {bg}; color: {fg};"
    )
}

fn gear_style(active: bool) -> String {
    let bg = if active { "#0f3460" } else { "#1a1a2e" };
    let border = if active { "#4ecca3" } else { "#2a3960" };
    format!(
        "padding: 0.4rem 0.7rem; font-size: 1rem; line-height: 1; cursor: pointer; \
         border: 1px solid {border}; border-radius: 4px; background: {bg}; color: #e0e0e0;"
    )
}
