use leptos::prelude::*;

/// Shown when no session is connected and there are no messages.
///
/// During history catch-up the layout serves as a skeleton — the icon
/// is shown but the hint text is hidden.
#[component]
pub fn EmptyState(show_hint: Signal<bool>) -> impl IntoView {
    view! {
        <div class="empty-state" id="emptyState">
            <img
                class="empty-icon"
                src="/icon-192.png"
                width="48"
                height="48"
                alt=""
            />
            {move || {
                if show_hint.get() {
                    view! { <div class="hint">"Select or create a session to begin"</div> }
                        .into_any()
                } else {
                    ().into_any()
                }
            }}
        </div>
    }
}
