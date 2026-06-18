use leptos::prelude::*;

use crate::markdown::render_markdown;
use crate::state::UiMessage;

/// Render a single `UiMessage` variant.
#[component]
pub fn Message(msg: UiMessage) -> impl IntoView {
    match msg {
        UiMessage::UserPrompt { content, .. } => view! {
            <div class="msg user">{content}</div>
        }
        .into_any(),

        UiMessage::Thinking { .. } => view! {
            <div class="msg thinking">"thinking…"</div>
        }
        .into_any(),

        UiMessage::AssistantFinal { raw, .. } => {
            let html = render_markdown(&raw);
            view! {
                <div class="msg assistant rendered" inner_html=html></div>
            }
            .into_any()
        }

        UiMessage::ToolCall {
            name,
            args,
            result,
            expanded,
            ..
        } => {
            let toggle = move |_| expanded.update(|v| *v = !*v);
            view! {
                <div class="msg tool tool-call" class:open=expanded>
                    <div class="tool-header" on:click=toggle>
                        <span class="arrow">"▸"</span>
                        <div class="info">
                            <div class="name">{name.clone()}</div>
                            {args.iter().map(|(k, v)| {
                                view! {
                                    <div class="arg">
                                        <b>{format!("{k}:")}</b>
                                        " "
                                        {truncate(v, 120)}
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    </div>
                    <div class="tool-body">
                        {result.as_ref().map(|s| truncate(s, 500)).unwrap_or_default()}
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
    }
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
