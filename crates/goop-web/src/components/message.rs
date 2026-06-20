use goop_shared::EditContent;
use leptos::control_flow::For;
use leptos::{ev, prelude::*};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use crate::markdown::render_markdown;
use crate::state::{AppState, EditOverlay, UiMessage};

/// Render a single `UiMessage` variant.
///
/// Editable variants (`UserPrompt`, `AssistantFinal`, `ToolCall`) carry action
/// buttons (✎ edit, ✕ delete) in a row **below** the message content — not
/// overlaid on top (the OpenWebUI placement).  The row is always in the DOM
/// but `opacity: 0` by default; `:hover` on the message reveals it.  On touch
/// devices a tap on the message triggers `:hover` (browsers fire `mouseover`
/// on touch), so the pattern doubles as tap-to-reveal — no JS needed.
///
/// Deleting is a **two-step inline confirm**: clicking ✕ replaces the row
/// with "Delete? ✓ ✕".  This prevents accidental deletes on mobile, where a
/// single tap could otherwise fire immediately.  The confirm state lives in a
/// local `RwSignal` that persists for the message's lifetime (the `<For>`
/// cache keeps the component instance alive across re-renders).
///
/// Editing a text message swaps its content for an inline `<textarea>`;
/// saving sends a `ClientMessage::Edit` which the server echoes back as an
/// `Edited` overlay event, setting an [`EditOverlay`] that the [`edit_badge`]
/// toggle controls.  Deleting sends a `ClientMessage::Delete`; the server
/// appends a `Deleted` overlay (and, for a tool call/result, one for the
/// matching half) which comes back and sets the `deleted` flag.
///
/// Actions are hidden while the LLM is running (`state.running`) and on
/// already-deleted messages — editing the agent's memory mid-turn is
/// confusing, and a deleted message has nothing to edit.
///
/// **Always-render pattern:** display, edit, and both action states are
/// always in the DOM, toggled via `class:hidden`.  This avoids the Leptos
/// `FnOnce`-in-`Fn` trap: event-handler closures are moved directly into
/// `on:click` / `on:input` (once), not inside a `move ||` reactive closure
/// that runs many times.
#[component]
pub fn Message(msg: UiMessage) -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState missing");
    let running = state.running;
    let llm_view = state.llm_view;

    match msg {
        UiMessage::UserPrompt {
            seq,
            content,
            deleted,
            edit,
            ..
        } => {
            let editing = RwSignal::new(false);
            let confirm_delete = RwSignal::new(false);
            let textarea_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
            let edit_sig = edit;
            let state_fork = state.clone();
            let state_del = state.clone();
            // Clone for start_edit before `content` is moved into the view.
            let content_for_edit = content.clone();

            let start_edit = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                if running.get() {
                    return;
                }
                let text = {
                    let e = edit_sig.get();
                    match e {
                        Some(e) if !e.show_original.get() => e.replacement.clone(),
                        _ => content_for_edit.clone(),
                    }
                };
                editing.set(true);
                focus_and_fill_textarea(&textarea_ref, &text);
            };

            // Editing a user prompt forks the conversation (edit-and-
            // regenerate, like ChatGPT): the server appends a new `UserPrompt`
            // with this text branching from the prompt's parent and reruns the
            // turn.  The old branch is preserved; the client re-catches-up to
            // the new branch via a `Reset`.  This is distinct from the
            // `AssistantFinal`/`ToolCall` edit, which overlays the change in
            // place ("writing into the LLM's mind") without regenerating.
            let save_edit = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                if let Some(el) = textarea_ref.get() {
                    let text = el.value();
                    if !text.trim().is_empty() {
                        state_fork.fork_message(seq, text);
                    }
                }
                editing.set(false);
            };

            let cancel_edit = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                editing.set(false);
            };

            let on_input = move |_| {
                if let Some(el) = textarea_ref.get() {
                    resize_textarea(&el);
                }
            };

            let request_delete = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                confirm_delete.set(true);
            };

            let confirm_delete_action = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                confirm_delete.set(false);
                state_del.delete_message(seq);
            };

            let cancel_delete = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                confirm_delete.set(false);
            };

            let actions_hidden =
                move || editing.get() || deleted.get() || running.get() || llm_view.get();

            view! {
                <div class="msg-wrap user" class:confirming=confirm_delete>
                    <div
                        class="msg user"
                        class:deleted=deleted
                        class:edited=move || edit_sig.get().is_some()
                        class:editing=editing
                    >
                        <div class="msg-display" class:hidden=editing>
                            {move || {
                                let e = edit_sig.get();
                                match e {
                                    Some(e) if !e.show_original.get() => e.replacement.clone(),
                                    _ => content.clone(),
                                }
                            }}
                            {edit_badge(edit_sig)}
                        </div>
                        <div class="msg-edit" class:hidden=move || !editing.get()>
                            <textarea
                                class="msg-edit-area"
                                node_ref=textarea_ref
                                on:input=on_input
                                rows="1"
                            ></textarea>
                            <div class="msg-edit-actions">
                                <button class="msg-edit-btn save" on:click=save_edit>"↻ Save & regenerate"</button>
                                <button class="msg-edit-btn cancel" on:click=cancel_edit>"Cancel"</button>
                            </div>
                        </div>
                    </div>
                    {message_actions(
                        actions_hidden,
                        confirm_delete,
                        start_edit,
                        request_delete,
                        confirm_delete_action,
                        cancel_delete,
                    )}
                </div>
            }
                .into_any()
        }

        UiMessage::Thinking { .. } => view! {
            <div class="msg thinking">"thinking…"</div>
        }
        .into_any(),

        UiMessage::AssistantFinal {
            seq,
            raw,
            deleted,
            edit,
            ..
        } => {
            let editing = RwSignal::new(false);
            let confirm_delete = RwSignal::new(false);
            let textarea_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
            let edit_sig = edit;
            let state_edit = state.clone();
            let state_del = state.clone();
            let raw_for_edit = raw.clone();

            let display_raw = move || {
                let e = edit_sig.get();
                match e {
                    Some(e) if !e.show_original.get() => e.replacement.clone(),
                    _ => raw.clone(),
                }
            };

            let html = move || render_markdown(&display_raw());

            let start_edit = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                if running.get() {
                    return;
                }
                let text = {
                    let e = edit_sig.get();
                    match e {
                        Some(e) if !e.show_original.get() => e.replacement.clone(),
                        _ => raw_for_edit.clone(),
                    }
                };
                editing.set(true);
                focus_and_fill_textarea(&textarea_ref, &text);
            };

            let save_edit = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                if let Some(el) = textarea_ref.get() {
                    let text = el.value();
                    if !text.trim().is_empty() {
                        state_edit.edit_message(seq, EditContent::Text(text));
                    }
                }
                editing.set(false);
            };

            let cancel_edit = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                editing.set(false);
            };

            let on_input = move |_| {
                if let Some(el) = textarea_ref.get() {
                    resize_textarea(&el);
                }
            };

            let request_delete = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                confirm_delete.set(true);
            };

            let confirm_delete_action = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                confirm_delete.set(false);
                state_del.delete_message(seq);
            };

            let cancel_delete = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                confirm_delete.set(false);
            };

            let actions_hidden =
                move || editing.get() || deleted.get() || running.get() || llm_view.get();

            view! {
                <div class="msg-wrap assistant" class:confirming=confirm_delete>
                    <div
                        class="msg assistant rendered"
                        class:deleted=deleted
                        class:edited=move || edit_sig.get().is_some()
                        class:editing=editing
                    >
                        <div class="msg-display" class:hidden=editing>
                            <div class="rendered-inner" inner_html=html></div>
                            {edit_badge(edit_sig)}
                        </div>
                        <div class="msg-edit" class:hidden=move || !editing.get()>
                            <textarea
                                class="msg-edit-area"
                                node_ref=textarea_ref
                                on:input=on_input
                                rows="1"
                            ></textarea>
                            <div class="msg-edit-actions">
                                <button class="msg-edit-btn save" on:click=save_edit>"Save"</button>
                                <button class="msg-edit-btn cancel" on:click=cancel_edit>"Cancel"</button>
                            </div>
                        </div>
                    </div>
                    {message_actions(
                        actions_hidden,
                        confirm_delete,
                        start_edit,
                        request_delete,
                        confirm_delete_action,
                        cancel_delete,
                    )}
                </div>
            }
                .into_any()
        }

        UiMessage::ToolCall {
            seq,
            name,
            args,
            result,
            expanded,
            deleted,
            edit,
            result_edit,
            ..
        } => {
            let toggle = move |_| expanded.update(|v| *v = !*v);
            let confirm_delete = RwSignal::new(false);
            let name_for_view = name.clone();
            let args_for_view = args.clone();
            let call_edit = edit;
            let result_sig = result;
            let result_edit_sig = result_edit;
            let state_del = state.clone();

            let request_delete = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                confirm_delete.set(true);
            };

            let confirm_delete_action = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                confirm_delete.set(false);
                state_del.delete_message(seq);
            };

            let cancel_delete = move |evt: ev::MouseEvent| {
                evt.stop_propagation();
                confirm_delete.set(false);
            };

            let actions_hidden = move || deleted.get() || running.get() || llm_view.get();

            view! {
                <div class="msg-wrap tool" class:confirming=confirm_delete>
                    <div
                        class="msg tool tool-call"
                        class:open=expanded
                        class:deleted=deleted
                    >
                        <div class="tool-header" on:click=toggle>
                            <span class="arrow">"▸"</span>
                            {move || {
                                let e = call_edit.get();
                                match e {
                                    Some(e) if !e.show_original.get() => view! {
                                        <div class="info">
                                            <div class="name edited-replacement">{e.replacement.clone()}</div>
                                        </div>
                                    }
                                        .into_any(),
                                    _ => render_tool_info(&name_for_view, &args_for_view),
                                }
                            }}
                            {edit_badge(edit)}
                        </div>
                        <div class="tool-body">
                            {move || {
                                let re = result_edit_sig.get();
                                let text = match re {
                                    Some(re) if !re.show_original.get() => re.replacement,
                                    _ => result_sig.get().unwrap_or_default(),
                                };
                                if text.is_empty() {
                                    String::new()
                                } else {
                                    truncate(&text, 500)
                                }
                            }}
                            {edit_badge(result_edit_sig)}
                        </div>
                    </div>
                    {delete_only_actions(
                        actions_hidden,
                        confirm_delete,
                        request_delete,
                        confirm_delete_action,
                        cancel_delete,
                    )}
                </div>
            }
                .into_any()
        }

        UiMessage::FinalResponse { .. } => view! {
            <div class="msg final">"—"</div>
        }
        .into_any(),

        UiMessage::Error { msg, .. } => view! {
            <div class="msg error">{msg}</div>
        }
        .into_any(),

        UiMessage::Cancelled { .. } => view! {
            <div class="msg final">"cancelled"</div>
        }
        .into_any(),

        UiMessage::CompactedGroup {
            summary,
            model,
            manual,
            children,
            expanded,
            ..
        } => {
            let toggle = move |_| expanded.update(|v| *v = !*v);
            let summary_html = render_markdown(&summary);
            let count = children.len();
            let chat_label = if manual {
                "✦ manual compaction"
            } else {
                "✦ compacted"
            };
            let llm_label = if manual {
                "✦ manual summary"
            } else {
                "✦ summary"
            };

            // Always-render both views, toggled by class:hidden.  Using
            // get_untracked() + early return doesn't work: <For> doesn't
            // re-create the component when llm_view toggles (key unchanged),
            // so the branch is frozen at creation time.
            view! {
                <div class="msg group compacted" class:open=expanded class:hidden=move || llm_view.get()>
                    <div class="group-header" on:click=toggle>
                        <span class="arrow">"▸"</span>
                        <div class="group-summary">
                            <div class="group-meta">{format!("{chat_label} · {count} messages · {model}")}</div>
                            <div class="rendered-inner" inner_html=summary_html.clone()></div>
                        </div>
                    </div>
                    <div class="group-children">
                        <For
                            each=move || children.clone()
                            key=|m| m.id()
                            children=move |m| view! { <Message msg=m /> }
                        />
                    </div>
                </div>
                <div class="msg llm-summary" class:hidden=move || !llm_view.get()>
                    <div class="llm-summary-label">{llm_label}</div>
                    <div class="rendered-inner" inner_html=summary_html></div>
                </div>
            }
                .into_any()
        }

        UiMessage::ToolSummaryGroup {
            summary,
            model,
            child,
            expanded,
            ..
        } => {
            let toggle = move |_| expanded.update(|v| *v = !*v);
            let summary_html = render_markdown(&summary);

            // Always-render both views — see CompactedGroup above.
            view! {
                <div class="msg group tool-summary" class:open=expanded class:hidden=move || llm_view.get()>
                    <div class="group-header" on:click=toggle>
                        <span class="arrow">"▸"</span>
                        <div class="group-summary">
                            <div class="group-meta">{format!("◇ tool pair summarized · {model}")}</div>
                            <div class="rendered-inner" inner_html=summary_html.clone()></div>
                        </div>
                    </div>
                    <div class="group-children">
                        <Message msg=(*child).clone() />
                    </div>
                </div>
                <div class="msg llm-summary" class:hidden=move || !llm_view.get()>
                    <div class="llm-summary-label">"◇ tool summary"</div>
                    <div class="rendered-inner" inner_html=summary_html></div>
                </div>
            }
                .into_any()
        }
    }
}

/// The action row for messages that support both edit and delete
/// (`UserPrompt`, `AssistantFinal`).  Two states are always in the DOM,
/// toggled by `class:hidden`:
///
/// - **Normal:** ✎ edit + ✕ delete-request
/// - **Confirm:** "Delete?" + ✓ confirm + ✕ cancel
///
/// The row is `opacity: 0` by default (CSS `.msg-actions`) and revealed by
/// `:hover` on the parent `.msg` (which on touch devices is triggered by a
/// tap).  The `.confirming` class on the parent forces opacity 1 so the
/// confirm prompt is always visible.
#[allow(clippy::too_many_arguments)]
fn message_actions(
    hidden: impl Fn() -> bool + Send + Sync + 'static,
    confirm_delete: RwSignal<bool>,
    start_edit: impl Fn(ev::MouseEvent) + Send + Sync + 'static,
    request_delete: impl Fn(ev::MouseEvent) + Send + Sync + 'static,
    confirm_delete_action: impl Fn(ev::MouseEvent) + Send + Sync + 'static,
    cancel_delete: impl Fn(ev::MouseEvent) + Send + Sync + 'static,
) -> AnyView {
    view! {
        <div class="msg-actions" class:hidden=hidden>
            <div class="actions-row" class:hidden=confirm_delete>
                <button class="msg-action edit" title="Edit" on:click=start_edit>"✎"</button>
                <button class="msg-action delete" title="Delete" on:click=request_delete>"✕"</button>
            </div>
            <div class="actions-row confirm" class:hidden=move || !confirm_delete.get()>
                <span class="confirm-text">"Delete?"</span>
                <button class="msg-action confirm" title="Confirm delete" on:click=confirm_delete_action>"✓"</button>
                <button class="msg-action cancel" title="Cancel" on:click=cancel_delete>"✕"</button>
            </div>
        </div>
    }
    .into_any()
}

/// The action row for messages that support delete only (`ToolCall`).  Same
/// two-state confirm pattern as [`message_actions`], minus the edit button.
fn delete_only_actions(
    hidden: impl Fn() -> bool + Send + Sync + 'static,
    confirm_delete: RwSignal<bool>,
    request_delete: impl Fn(ev::MouseEvent) + Send + Sync + 'static,
    confirm_delete_action: impl Fn(ev::MouseEvent) + Send + Sync + 'static,
    cancel_delete: impl Fn(ev::MouseEvent) + Send + Sync + 'static,
) -> AnyView {
    view! {
        <div class="msg-actions" class:hidden=hidden>
            <div class="actions-row" class:hidden=confirm_delete>
                <button class="msg-action delete" title="Delete" on:click=request_delete>"✕"</button>
            </div>
            <div class="actions-row confirm" class:hidden=move || !confirm_delete.get()>
                <span class="confirm-text">"Delete?"</span>
                <button class="msg-action confirm" title="Confirm delete" on:click=confirm_delete_action>"✓"</button>
                <button class="msg-action cancel" title="Cancel" on:click=cancel_delete>"✕"</button>
            </div>
        </div>
    }
    .into_any()
}

/// Render the structured name + args block of a `ToolCall` (the "original"
/// view, shown when no edit overlay is active or the user toggled to it).
fn render_tool_info(name: &str, args: &[(String, String)]) -> AnyView {
    view! {
        <div class="info">
            <div class="name">{name.to_string()}</div>
            {args
                .iter()
                .map(|(k, v)| {
                    view! {
                        <div class="arg">
                            <b>{format!("{k}:")}</b>
                            " "
                            {truncate(v, 120)}
                        </div>
                    }
                })
                .collect::<Vec<_>>()}
        </div>
    }
    .into_any()
}

/// A small "✎ edited / ✎ original" toggle badge, shown only when an edit
/// overlay is present.  Clicking toggles between the replacement and the
/// original.  `stop_propagation` keeps the click from also toggling a parent
/// (e.g. a tool-call's expand header).
fn edit_badge(edit: RwSignal<Option<EditOverlay>>) -> AnyView {
    view! {
        {move || {
            edit.get().map(|e| {
                let show_original = e.show_original;
                view! {
                    <span
                        class="edit-badge"
                        on:click=move |evt: ev::MouseEvent| {
                            evt.stop_propagation();
                            show_original.update(|v| *v = !*v);
                        }
                    >
                        {move || if show_original.get() { "✎ original" } else { "✎ edited" }}
                    </span>
                }
            })
        }}
    }
    .into_any()
}

/// Set a textarea's value, focus it, place the cursor at the end, and
/// auto-resize — all on the next tick (after Leptos has updated the DOM to
/// reveal the edit area).
fn focus_and_fill_textarea(node_ref: &NodeRef<leptos::html::Textarea>, text: &str) {
    let node_ref = *node_ref;
    let text = text.to_string();
    if let Some(window) = web_sys::window() {
        let cb = Closure::once(move || {
            if let Some(el) = node_ref.get() {
                el.set_value(&text);
                let _ = el.focus();
                let len = text.len() as u32;
                let _ = el.set_selection_range(len, len);
                resize_textarea(&el);
            }
        });
        let _ = window.set_timeout_with_callback(cb.as_ref().unchecked_ref());
        cb.forget();
    }
}

/// Auto-resize a textarea to fit its content (capped at 200px), matching the
/// input bar's behaviour.
fn resize_textarea(el: &web_sys::HtmlTextAreaElement) {
    let style = web_sys::HtmlElement::style(el);
    style.set_property("height", "auto").ok();
    let scroll_height = el.scroll_height();
    let new_height = scroll_height.min(200);
    style
        .set_property("height", &format!("{new_height}px"))
        .ok();
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() > n {
        // Find a valid UTF-8 boundary at or before the cutoff.
        let end = s.floor_char_boundary(n.saturating_sub(1));
        format!("{}…", &s[..end])
    } else {
        s.to_string()
    }
}
