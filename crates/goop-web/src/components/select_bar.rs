use leptos::prelude::*;

use crate::state::AppState;

/// Footer bar shown in select mode (replaces the input bar).
///
/// Displays the number of selected messages and provides a "✦ Compact"
/// button that sends a `CompactRange` request to the server, plus a "✕ Done"
/// button that exits select mode without compacting.
///
/// The compact button is disabled until at least 2 messages are selected
/// (summarizing a single message is pointless).  See §2.11 of the redesign
/// doc.
#[component]
pub fn SelectBar() -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState missing");

    let count = Signal::derive(move || state.selected_seqs.get().len());
    let can_compact = Signal::derive(move || count.get() >= 2);

    let do_compact = {
        let state = state.clone();
        move |_| {
            state.compact_selected();
        }
    };

    let do_cancel = move |_| {
        state.exit_select_mode();
    };

    view! {
        <footer class="select-bar">
            <span class="select-count">
                {move || {
                    let n = count.get();
                    if n == 0 {
                        "Select messages to compact".to_string()
                    } else {
                        format!("{n} selected")
                    }
                }}
            </span>
            <div class="select-actions">
                <button
                    class="select-compact-btn"
                    disabled=move || !can_compact.get()
                    on:click=do_compact
                >
                    "✦ Compact"
                </button>
                <button class="select-cancel-btn" on:click=do_cancel>
                    "✕ Done"
                </button>
            </div>
        </footer>
    }
}
