use leptos::control_flow::For;
use leptos::{ev, prelude::*};

use crate::markdown::render_markdown;
use crate::state::{EditOverlay, UiMessage};

/// Render a single `UiMessage` variant.
#[component]
pub fn Message(msg: UiMessage) -> impl IntoView {
    match msg {
        UiMessage::UserPrompt {
            content,
            deleted,
            edit,
            ..
        } => {
            let edit_disp = edit;
            view! {
                <div class="msg user" class:deleted=deleted class:edited=move || edit.get().is_some()>
                    {move || {
                        let e = edit_disp.get();
                        match e {
                            Some(e) if !e.show_original.get() => e.replacement,
                            _ => content.clone(),
                        }
                    }}
                    {edit_badge(edit)}
                </div>
            }
                .into_any()
        }

        UiMessage::Thinking { .. } => view! {
            <div class="msg thinking">"thinking…"</div>
        }
        .into_any(),

        UiMessage::AssistantFinal {
            raw, deleted, edit, ..
        } => {
            let edit_disp = edit;
            let html = move || {
                let e = edit_disp.get();
                let text = match e {
                    Some(e) if !e.show_original.get() => e.replacement,
                    _ => raw.clone(),
                };
                render_markdown(&text)
            };
            view! {
                <div class="msg assistant rendered" class:deleted=deleted class:edited=move || edit.get().is_some()>
                    <div class="rendered-inner" inner_html=html></div>
                    {edit_badge(edit)}
                </div>
            }
                .into_any()
        }

        UiMessage::ToolCall {
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
            let name_for_view = name.clone();
            let args_for_view = args.clone();
            let call_edit = edit;
            let result_sig = result;
            let result_edit_sig = result_edit;
            view! {
                <div class="msg tool tool-call" class:open=expanded class:deleted=deleted>
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
            let label = if manual {
                "✦ manual compaction"
            } else {
                "✦ compacted"
            };
            view! {
                <div class="msg group compacted" class:open=expanded>
                    <div class="group-header" on:click=toggle>
                        <span class="arrow">"▸"</span>
                        <div class="group-summary">
                            <div class="group-meta">{format!("{label} · {count} messages · {model}")}</div>
                            <div class="rendered-inner" inner_html=summary_html></div>
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
            view! {
                <div class="msg group tool-summary" class:open=expanded>
                    <div class="group-header" on:click=toggle>
                        <span class="arrow">"▸"</span>
                        <div class="group-summary">
                            <div class="group-meta">{format!("◇ tool pair summarized · {model}")}</div>
                            <div class="rendered-inner" inner_html=summary_html></div>
                        </div>
                    </div>
                    <div class="group-children">
                        <Message msg=(*child).clone() />
                    </div>
                </div>
            }
                .into_any()
        }
    }
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

fn truncate(s: &str, n: usize) -> String {
    if s.len() > n {
        // Find a valid UTF-8 boundary at or before the cutoff.
        let end = s.floor_char_boundary(n.saturating_sub(1));
        format!("{}…", &s[..end])
    } else {
        s.to_string()
    }
}
