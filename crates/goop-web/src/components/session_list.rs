use leptos::prelude::*;

use crate::state::AppState;

/// Sidebar session list — renders sessions from `AppState`.
#[component]
pub fn SessionList() -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState missing");
    let sidebar_open = state.sidebar_open;

    view! {
        <div class="session-list" id="sessionList">
            {move || {
                state.sessions.get()
                    .into_iter()
                    .map(|name| {
                        // Derive is_active reactively — one per session item.
                        let name_active = name.clone();
                        let state_active = state.clone();
                        let is_active = Signal::derive(move || {
                            state_active.current_session.get().as_deref()
                                == Some(name_active.as_str())
                        });

                        let name_for_switch = name.clone();
                        let state_for_switch = state.clone();
                        let switch = move |_| {
                            state_for_switch.connect_session(name_for_switch.clone());
                            sidebar_open.set(false);
                        };

                        let name_for_del = name.clone();
                        let state_for_del = state.clone();
                        let do_delete = move |evt: leptos::ev::MouseEvent| {
                            evt.stop_propagation();
                            let name = name_for_del.clone();
                            let state = state_for_del.clone();
                            leptos::task::spawn_local(async move {
                                state.delete_session(&name).await;
                            });
                        };

                        view! {
                            <div class="session-item" class:active=is_active on:click=switch>
                                <span class="name">{name}</span>
                                <button
                                    class="del-btn"
                                    title="Delete session"
                                    on:click=do_delete
                                >
                                    "×"
                                </button>
                            </div>
                        }
                    })
                    .collect::<Vec<_>>()
            }}
        </div>
    }
}
