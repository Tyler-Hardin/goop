use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use leptos::control_flow::For;
use leptos::html::Div;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use crate::components::message::Message;
use crate::markdown::render_markdown;
use crate::state::{AppState, UiMessage};

/// Scrollable message list.
///
/// Renders all messages from `AppState::messages`, plus a live streaming
/// assistant bubble updated via `text_content` (not reactive view diffing)
/// so the DOM node stays stable and CSS fade-in doesn't retrigger on every
/// chunk.
///
/// Auto-scrolls to the bottom on new content unless the user has scrolled
/// away.  Scrolls are throttled via `requestAnimationFrame` so the visual
/// update stays smooth even when streaming chunks arrive faster than 60 fps.
///
/// **Select mode:** when `AppState::select_mode` is on, each agent-visible
/// message gets a checkbox on its left.  Clicking the checkbox toggles the
/// message's selection for manual range compaction (§2.11).  Non-agent-
/// visible messages (Thinking, FinalResponse, etc.) don't get a checkbox.
#[component]
pub fn MessageLog() -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState missing");
    let scroll_ref: NodeRef<leptos::html::Main> = NodeRef::new();
    let stream_ref: NodeRef<Div> = NodeRef::new();

    // Whether the user is at the bottom (so we know when to auto-scroll).
    let user_at_bottom = RwSignal::new(true);

    // Guard: true while a programmatic scrollTo is in flight.  The scroll
    // event handler ignores events when this is set to prevent sub-pixel
    // rounding from flipping user_at_bottom to false.
    let programmatic_scroll = RwSignal::new(false);

    // RAF gate — true while a requestAnimationFrame callback is already
    // scheduled (avoids piling up redundant scrolls).
    let raf_pending = RwSignal::new(false);

    // Alive flag — set to false in on_cleanup so RAF callbacks that fire
    // after the component is unmounted become no-ops (they would otherwise
    // try to access disposed reactive signals and panic).
    let alive = Arc::new(AtomicBool::new(true));
    on_cleanup({
        let alive = alive.clone();
        move || alive.store(false, Ordering::Relaxed)
    });

    // Schedule a scroll-to-bottom on the next animation frame.  If a RAF
    // is already pending this is a no-op — we only ever have one queued.
    //
    // The RAF callback re-checks `user_at_bottom` before scrolling: a user
    // can scroll away during the window between scheduling and the next
    // animation frame, and we must not yank them back.
    //
    // `programmatic_scroll` guards the scroll event handler: sub-pixel
    // rounding on HiDPI screens can make a scroll-to-bottom report a
    // position slightly off the true bottom, which would flip
    // `user_at_bottom` to false and break auto-scroll.  We set the flag
    // before calling scrollTo and clear it in the handler (immediate) +
    // double-RAF (safety, in case the browser doesn't fire a scroll event).
    let schedule_scroll = {
        let alive = alive.clone();
        move || {
            if !alive.load(Ordering::Relaxed) {
                return;
            }
            if raf_pending.get_untracked() {
                return;
            }
            raf_pending.set(true);

            let Some(window) = web_sys::window() else {
                return;
            };
            let alive = alive.clone();

            let cb = Closure::<dyn Fn()>::new(move || {
                raf_pending.set(false);
                if !alive.load(Ordering::Relaxed) {
                    return;
                }
                // Re-check before scrolling — the user may have scrolled
                // away between the scheduling call and this frame.
                if !user_at_bottom.get_untracked() {
                    return;
                }
                if let Some(el) = scroll_ref.get_untracked() {
                    programmatic_scroll.set(true);
                    let opts = web_sys::ScrollToOptions::new();
                    opts.set_top(el.scroll_height() as f64);
                    el.scroll_to_with_scroll_to_options(&opts);
                    // scrollTo triggers a synchronous scroll event, which
                    // the handler ignores because programmatic_scroll is set.
                    // If the scroll position didn't change (already at
                    // bottom), no scroll event fires — clear the flag on
                    // the next frame as a safety net.
                    if let Some(w) = web_sys::window() {
                        let alive = alive.clone();
                        let cb2 = Closure::<dyn Fn()>::new(move || {
                            if alive.load(Ordering::Relaxed) {
                                programmatic_scroll.set(false);
                            }
                        });
                        let _ = w.request_animation_frame(cb2.as_ref().unchecked_ref());
                        cb2.forget();
                    }
                }
            });
            let _ = window.request_animation_frame(cb.as_ref().unchecked_ref());
            cb.forget();
        }
    };

    // Auto-scroll when messages or streaming text change, but only if the
    // user hasn't scrolled away.
    //
    // IMPORTANT: this Effect is created AFTER the streaming DOM Effect
    // below, so when streaming_text changes the DOM update RAF fires
    // before the scroll RAF (same-frame ordering).  For messages (which
    // are updated by Leptos's <For> reconciliation before any Effect runs),
    // there is no ordering concern.
    Effect::new(move || {
        let _ = state.messages.get().len();
        let _ = state.streaming_text.get();
        if user_at_bottom.get_untracked() {
            schedule_scroll();
        }
    });

    // Keep the streaming div's text_content in sync with streaming_text.
    // Uses insert_adjacent_text("beforeend", …) for true append — avoids
    // replacing the entire text node on every chunk (quadratic cost for
    // long responses, and DOM reparse flashes).
    //
    // DOM updates are throttled via requestAnimationFrame so at most one
    // paint per frame occurs.  The signal still accumulates on every chunk;
    // only the visible DOM lags by ≤16 ms.  No gate — the browser coalesces
    // multiple RAF callbacks within a single frame automatically.
    //
    // Created BEFORE the scroll Effect above — when streaming_text changes,
    // this RAF fires first (updating the DOM height), then the scroll RAF
    // fires (scrolling to the new height).  Leptos runs effects in creation
    // order synchronously, so both RAFs are scheduled in the correct
    // sequence within the same microtask.
    let streaming_visible = RwSignal::new(false);
    let streamed_len = RwSignal::new(0usize);

    Effect::new(move || {
        let text = state.streaming_text.get();
        let empty = text.is_empty();
        streaming_visible.set(!empty);

        let Some(window) = web_sys::window() else {
            return;
        };
        let alive = alive.clone();
        let cb = Closure::<dyn Fn()>::new(move || {
            if !alive.load(Ordering::Relaxed) {
                return;
            }
            let text = state.streaming_text.get_untracked();
            let prev_len = streamed_len.get_untracked();
            if let Some(el) = stream_ref.get() {
                if text.is_empty() {
                    el.set_text_content(None);
                    streamed_len.set(0);
                } else if prev_len == 0 || text.len() < prev_len {
                    // First chunk of a new stream, or text was flushed —
                    // replace entirely.
                    el.set_text_content(Some(&text));
                    streamed_len.set(text.len());
                } else if text.len() > prev_len {
                    // Append-only the new portion.
                    let chunk = &text[prev_len..];
                    let _ = el.insert_adjacent_text("beforeend", chunk);
                    streamed_len.set(text.len());
                }
            }
        });
        let _ = window.request_animation_frame(cb.as_ref().unchecked_ref());
        cb.forget();
    });

    // Scroll event handler: track user_at_bottom and filter out
    // programmatic scroll events.
    //
    // We guard against our own scrollTo calls with programmatic_scroll
    // because a scroll event from scrollTo may report a position that is
    // slightly off the bottom (sub-pixel rounding on HiDPI screens).  If
    // we let that event update user_at_bottom, it could flip to false and
    // break auto-scroll.
    let on_scroll = {
        move |_| {
            let prog = programmatic_scroll.get_untracked();
            programmatic_scroll.set(false);
            if prog {
                return; // ignore programmatic scroll events
            }
            if let Some(el) = scroll_ref.get_untracked() {
                let threshold: f64 = 2.0;
                let at_bottom =
                    el.scroll_height() as f64 - el.scroll_top() as f64 - el.client_height() as f64
                        <= threshold;
                user_at_bottom.set(at_bottom);
            }
        }
    };

    // Capture signals before `state` is moved into the <For> closure.
    let select_mode = state.select_mode;
    let llm_view = state.llm_view;

    // System prompt (preamble) — shown in both chat and LLM view.
    // Collapsed by default (the preamble can be very long — it includes
    // AGENTS.md).
    let system_prompt = state.system_prompt;
    let sp_expanded = RwSignal::new(false);
    let sp_html = RwSignal::new(String::new());
    Effect::new(move || {
        if let Some(text) = system_prompt.get() {
            sp_html.set(render_markdown(&text));
        } else {
            sp_html.set(String::new());
        }
    });

    view! {
        <main
            id="log"
            node_ref=scroll_ref
            on:scroll=on_scroll
            class:select-mode=move || select_mode.get()
            class:llm-view=move || llm_view.get()
        >
            // System prompt — metadata, not a conversation message.  Rendered
            // above the <For> so it doesn't interfere with compaction
            // targeting, selection indices, or <For> keying.  Visible in
            // both chat and LLM views when a preamble is present.
            <div
                class="msg system-prompt"
                class:hidden=move || system_prompt.get().is_none()
            >
                <div class="system-prompt-header" on:click=move |_| sp_expanded.update(|v| *v = !*v)>
                    <span class="arrow" class:open=move || sp_expanded.get()>"▸"</span>
                    <span class="system-prompt-label">"⚙ System Prompt"</span>
                </div>
                <div class="system-prompt-body" class:hidden=move || !sp_expanded.get()>
                    <div class="rendered-inner" inner_html=move || sp_html.get()></div>
                </div>
            </div>
            // Messages from state, rendered with stable keys via <For>.
            // Each message has a unique `id` — Leptos tracks items by key,
            // so adding a new message only inserts one DOM node instead of
            // recreating the entire list.  This prevents the CSS fadeIn
            // animation from re-triggering on every existing message.
            //
            // **Chat view** (default): `CompactedGroup`/`ToolSummaryGroup`
            // are flattened into their original children — the user sees the
            // full conversation as if compaction never happened.
            //
            // **LLM view** (👁): groups are kept as-is (summaries replace
            // covered messages), and deleted messages are filtered out —
            // this shows exactly what the agent sees.
            //
            // In select mode (chat-view-only), messages are enumerated so
            // clicking sets the range start/end for manual compaction.
            <For
                each=move || {
                    let msgs = state.messages.get();
                    let filtered: Vec<UiMessage> =
                        crate::state::displayed_messages(&msgs, llm_view.get());
                    filtered.into_iter().enumerate().collect::<Vec<_>>()
                }
                key=|(_, msg): &(usize, UiMessage)| msg.id()
                children=move |(idx, msg)| {
                    let st = state.clone();
                    let st_sel = st.clone();

                    view! {
                        <div
                            class="msg-select-wrap"
                            class:selected=move || st_sel.select_mode.get() && st_sel.is_in_selection(idx)
                        >
                            <div
                                class="msg-select-overlay"
                                class:hidden=move || !st.select_mode.get()
                                on:click=move |evt| {
                                    evt.stop_propagation();
                                    st.select_click(idx);
                                }
                            ></div>
                            <Message msg />
                        </div>
                    }
                }
            />
            // Live streaming assistant text (not yet flushed to messages).
            // The "streaming" class suppresses the per-message fadeIn
            // animation — the div stays stable; only text_content changes.
            <div
                class="msg assistant streaming"
                node_ref=stream_ref
                class:visible=streaming_visible
            ></div>
        </main>
    }
}
