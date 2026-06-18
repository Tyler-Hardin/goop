//! Unified button state machine for the send/mic/cancel button.
//!
//! The button has exactly one state at all times.  All transitions go
//! through the methods on [`BtnState`] — DOM event handlers and
//! [`AppState::dispatch`] never write the button state directly.
//!
//! ## States
//!
//! ```text
//!                          press (empty, connected)
//!   ┌──────┐           ┌──────────────────────────────┐
//!   │ Idle │──────────▶│ Recording { start_y }        │
//!   │      │           └──────────┬───────────────────┘
//!   └──┬───┘                      │
//!      │                          │ slide > 120px
//!      │ send(text)               ▼
//!      │                 ┌──────────────────────────────┐
//!      │                 │ CancelSlide { start_y }      │
//!      │                 └──────────┬───────────────────┘
//!      │                            │
//!      │          release           │ release
//!      │          (not cancelled)   │ (always cancelled)
//!      ▼                   ▼        ▼
//!   ┌─────────┐       ┌─────────┐ ┌──────┐
//!   │ Running │◄──────│ (audio  │ │ Idle │
//!   │         │       │  sent)  │ │      │
//!   └────┬────┘       └─────────┘ └──────┘
//!        │
//!        │ FinalResponse / Error / Cancelled (server) / click-cancel
//!        ▼
//!   ┌──────┐   disconnect ┌──────────┐  connect
//!   │ Idle │◄────────────│ Disabled │──────────▶ Idle
//!   └──────┘             └──────────┘
//! ```
//!
//! The key invariant (from the original JS design): **server lifecycle
//! events (FinalResponse, Error, Cancelled) must never exit the
//! Recording or CancelSlide states.**  The user's finger owns those
//! states; only a release transitions out of them.  [`on_llm_done`]
//! enforces this by being a no-op for every variant except `Running`.

use crate::state::AppState;
use crate::stt;
use leptos::prelude::*;

/// How far the finger must slide up (in px) to enter cancel mode.
pub const CANCEL_THRESHOLD: f64 = 120.0;

// ── state enum ─────────────────────────────────────────────────────────

/// Unified button state.
///
/// Exactly one variant is active at all times.  The `Idle` sub-states
/// (mic icon vs send icon, disabled appearance) are derived from
/// `input_text` and `connected` signals at render time, not stored here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BtnState {
    /// Ready for input.  Icon depends on whether the textarea has text.
    Idle,
    /// Hold-to-talk recording active.  `start_y` is the clientY of the
    /// pointerdown/touchstart that began recording, used to detect
    /// slide-up-to-cancel.
    Recording { start_y: f64 },
    /// Slide-up past [`CANCEL_THRESHOLD`]; finger still down.  Releasing
    /// in this state discards the recording.
    CancelSlide { start_y: f64 },
    /// LLM is processing a prompt or transcribing audio.  The button
    /// shows a red X and acts as a cancel control.
    Running,
    /// No session connected, or WebSocket is closed.  Button is
    /// grayed out and non-interactive.
    Disabled,
}

// ── pure transitions ───────────────────────────────────────────────────

impl BtnState {
    /// CSS class for the send button element, given whether the input
    /// currently has text.
    pub fn css_class(self, has_text: bool) -> &'static str {
        match self {
            BtnState::Idle if has_text => "has-text",
            BtnState::Idle => "",
            BtnState::Recording { .. } => "recording",
            BtnState::CancelSlide { .. } => "cancel-slide",
            BtnState::Running => "cancel",
            BtnState::Disabled => "",
        }
    }

    /// Whether the button should have the `disabled` attribute.
    pub fn is_disabled(self) -> bool {
        self == BtnState::Disabled
    }

    // ── transitions ───────────────────────────────────────────────

    /// Attempt to start recording.  Returns `Some(Recording)` if all
    /// preconditions are met, or `None` if the current state or app
    /// context prevents recording.
    pub fn try_start_recording(
        current: BtnState,
        app: &AppState,
        start_y: f64,
    ) -> Option<BtnState> {
        if current != BtnState::Idle {
            return None;
        }
        if app.running.get_untracked() {
            return None;
        }
        if !app.input_text.get_untracked().trim().is_empty() {
            return None;
        }
        if !app.connection.get_untracked().is_connected() {
            return None;
        }
        Some(BtnState::Recording { start_y })
    }

    /// Update recording state based on finger movement.
    /// A no-op if not in a recording-related state.
    pub fn on_move(current: BtnState, client_y: f64) -> BtnState {
        match current {
            BtnState::Recording { start_y } if start_y - client_y > CANCEL_THRESHOLD => {
                BtnState::CancelSlide { start_y }
            }
            BtnState::CancelSlide { start_y } if start_y - client_y <= CANCEL_THRESHOLD => {
                BtnState::Recording { start_y }
            }
            other => other,
        }
    }

    /// End recording.  Returns `Some((new_state, cancelled))` if
    /// currently `Recording` or `CancelSlide`, or `None` if not in a
    /// recording state.
    pub fn end_recording(current: BtnState) -> Option<(BtnState, bool)> {
        match current {
            BtnState::Recording { .. } => Some((BtnState::Running, false)),
            BtnState::CancelSlide { .. } => Some((BtnState::Idle, true)),
            _ => None,
        }
    }

    /// Handle STT initialisation failure — revert to Idle.
    /// Only transitions from `Recording` / `CancelSlide`.
    pub fn on_stt_failed(current: BtnState) -> BtnState {
        match current {
            BtnState::Recording { .. } | BtnState::CancelSlide { .. } => BtnState::Idle,
            other => other,
        }
    }

    /// Server finished processing (FinalResponse, Error, Cancelled).
    /// Transitions `Running` → `Idle`, but **never** clobbers
    /// `Recording` or `CancelSlide` — the user's finger owns those
    /// states.
    pub fn on_llm_done(current: BtnState) -> BtnState {
        match current {
            BtnState::Running => BtnState::Idle,
            other => other,
        }
    }
}

// ── side-effect helpers (called from DOM event handlers) ───────────────

/// Begin the recording side-effect (spawns `stt::start()`).
/// On failure, resets the button state to `Idle`.
pub fn spawn_recording_start(btn: RwSignal<BtnState>) {
    leptos::task::spawn_local(async move {
        if let Err(e) = stt::start().await {
            log::warn!("STT start failed: {e}");
            btn.update(|s| *s = BtnState::on_stt_failed(*s));
        }
    });
}

/// Stop recording and, if not cancelled, send the captured audio.
/// On empty / failed recording, resets button state to `Idle`.
pub fn spawn_recording_stop(state: AppState, btn: RwSignal<BtnState>, cancelled: bool) {
    leptos::task::spawn_local(async move {
        let wav = stt::stop(cancelled).await;
        if let Some(data) = wav {
            state.send_audio(data);
        } else if !cancelled {
            // Recording ended but no audio captured (empty / decode error).
            btn.set(BtnState::Idle);
        }
        // If cancelled, we already set Idle via end_recording.
    });
}
