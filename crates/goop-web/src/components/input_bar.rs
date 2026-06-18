use leptos::{ev, prelude::*};

use crate::components::input_button::{self, BtnState};
use crate::state::AppState;

/// Footer bar with textarea + send/mic/cancel button.
///
/// Auto-resizes the textarea on input.  Enter sends, Shift+Enter newlines.
/// The button icon toggles between mic (empty), send (has text), and
/// cancel (running).  Long-press / hold the mic starts recording with
/// slide-to-cancel.
///
/// Button state is owned by [`AppState::btn_state`] — a unified
/// [`BtnState`] enum.  DOM event handlers call pure transition methods
/// on [`BtnState`] and spawn side-effects via [`input_button`] helpers.
#[component]
pub fn InputBar() -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState missing");
    let input_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
    let btn_state = state.btn_state;

    // Has text in the input?
    let has_text = Signal::derive(move || !state.input_text.get().trim().is_empty());

    // CSS class for the send button — pure function on BtnState.
    let btn_class = Signal::derive(move || btn_state.get().css_class(has_text.get()).to_string());

    // ── auto-resize on input ─────────────────────────────────────────

    let on_input = {
        let state = state.clone();
        move |_| {
            if let Some(el) = input_ref.get_untracked() {
                state.input_text.set(el.value());
                resize_textarea(&el);
            }
        }
    };

    // ── keyboard ────────────────────────────────────────────────────

    let on_keydown = {
        let state = state.clone();
        move |evt: ev::KeyboardEvent| {
            if evt.key() == "Enter" && !evt.shift_key() {
                evt.prevent_default();
                send_or_cancel(&state, &input_ref);
            }
        }
    };

    // ── send button: click ─────────────────────────────────────────

    let on_click = {
        let state = state.clone();
        move |_| {
            // Guard: if the user just released after recording, the
            // browser may fire a click on the same frame.  Only act if
            // we're not in a recording-related state.
            let current = btn_state.get_untracked();
            if matches!(
                current,
                BtnState::Recording { .. } | BtnState::CancelSlide { .. }
            ) {
                return;
            }
            send_or_cancel(&state, &input_ref);
        }
    };

    // ── send button: pointer events (desktop hold-to-talk) ───────────

    let on_pointerdown = {
        let state = state.clone();
        move |evt: ev::PointerEvent| {
            let client_y = evt.client_y() as f64;
            if let Some(next) =
                BtnState::try_start_recording(btn_state.get_untracked(), &state, client_y)
            {
                evt.prevent_default();
                btn_state.set(next);
                input_button::spawn_recording_start(btn_state);
            }
        }
    };

    let on_pointermove = move |evt: ev::PointerEvent| {
        let current = btn_state.get_untracked();
        if !matches!(
            current,
            BtnState::Recording { .. } | BtnState::CancelSlide { .. }
        ) {
            return;
        }
        let client_y = evt.client_y() as f64;
        btn_state.update(|s| *s = BtnState::on_move(*s, client_y));
    };

    let on_pointerup = {
        let state = state.clone();
        move |evt: ev::PointerEvent| {
            evt.prevent_default();
            if let Some((next, cancelled)) = BtnState::end_recording(btn_state.get_untracked()) {
                btn_state.set(next);
                input_button::spawn_recording_stop(state.clone(), btn_state, cancelled);
            }
        }
    };

    // ── send button: touch events (mobile hold-to-talk) ──────────────

    let on_touchstart = {
        let state = state.clone();
        move |evt: ev::TouchEvent| {
            if let Some(touch) = evt.touches().get(0) {
                let client_y = touch.client_y() as f64;
                if let Some(next) =
                    BtnState::try_start_recording(btn_state.get_untracked(), &state, client_y)
                {
                    evt.prevent_default();
                    btn_state.set(next);
                    input_button::spawn_recording_start(btn_state);
                }
            }
        }
    };

    let on_touchmove = move |evt: ev::TouchEvent| {
        if let Some(touch) = evt.touches().get(0) {
            let current = btn_state.get_untracked();
            if !matches!(
                current,
                BtnState::Recording { .. } | BtnState::CancelSlide { .. }
            ) {
                return;
            }
            evt.prevent_default();
            let client_y = touch.client_y() as f64;
            btn_state.update(|s| *s = BtnState::on_move(*s, client_y));
        }
    };

    let on_touchend = {
        let state = state.clone();
        move |evt: ev::TouchEvent| {
            if let Some((next, cancelled)) = BtnState::end_recording(btn_state.get_untracked()) {
                evt.prevent_default();
                btn_state.set(next);
                input_button::spawn_recording_stop(state.clone(), btn_state, cancelled);
            }
            // When not in a recording state, do nothing — let the browser
            // synthesize a click event so the on:click handler fires.
        }
    };

    // ── focus on mount and on session switch ─────────────────────

    Effect::new(move || {
        // Track the focus request counter — re-fires when a new session
        // is connected, even if InputBar was already mounted.
        let _ = state.input_focus_request.get();
        if let Some(el) = input_ref.get() {
            let _ = el.focus();
        }
    });

    // ── render ──────────────────────────────────────────────────────

    view! {
        <footer>
            <textarea
                id="input"
                node_ref=input_ref
                placeholder="Ask anything…"
                autocomplete="off"
                rows="1"
                on:input=on_input
                on:keydown=on_keydown
            ></textarea>
            <button
                id="send"
                class=btn_class
                disabled=move || btn_state.get().is_disabled()
                on:click=on_click
                on:pointerdown=on_pointerdown
                on:pointermove=on_pointermove
                on:pointerup=on_pointerup
                on:touchstart=on_touchstart
                on:touchmove=on_touchmove
                on:touchend=on_touchend
            >
                <svg
                    class="icon-mic"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                >
                    <path d="M12 1a3 3 0 0 0-3 3v8a3 3 0 0 0 6 0V4a3 3 0 0 0-3-3z" />
                    <path d="M19 10v2a7 7 0 0 1-14 0v-2" />
                    <line x1="12" y1="19" x2="12" y2="23" />
                    <line x1="8" y1="23" x2="16" y2="23" />
                </svg>
                <svg
                    class="icon-send"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                >
                    <line x1="22" y1="2" x2="11" y2="13" />
                    <polygon points="22 2 15 22 11 13 2 9 22 2" />
                </svg>
                <svg class="icon-rec" viewBox="0 0 24 24" fill="currentColor">
                    <rect x="6" y="6" width="12" height="12" rx="2" />
                </svg>
                <svg
                    class="icon-cancel"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2"
                    stroke-linecap="round"
                >
                    <line x1="18" y1="6" x2="6" y2="18" />
                    <line x1="6" y1="6" x2="18" y2="18" />
                </svg>
            </button>
        </footer>
    }
}

// ── helpers ──────────────────────────────────────────────────────────

fn resize_textarea(el: &web_sys::HtmlTextAreaElement) {
    let style = web_sys::HtmlElement::style(el);
    style.set_property("height", "auto").ok();
    let scroll_height = el.scroll_height();
    let new_height = scroll_height.min(200);
    style
        .set_property("height", &format!("{new_height}px"))
        .ok();
}

/// Called from DOM event handlers — uses untracked reads throughout.
fn send_or_cancel(state: &AppState, input_ref: &NodeRef<leptos::html::Textarea>) {
    if state.btn_state.get_untracked() == BtnState::Running {
        state.cancel();
        return;
    }

    let text = state.input_text.get_untracked().trim().to_string();
    if text.is_empty() {
        return;
    }
    state.send_prompt(text);

    // Clear input.
    state.input_text.set(String::new());
    if let Some(el) = input_ref.get_untracked() {
        el.set_value("");
        let style = web_sys::HtmlElement::style(&el);
        style.set_property("height", "auto").ok();
        let _ = el.focus();
    }
}
