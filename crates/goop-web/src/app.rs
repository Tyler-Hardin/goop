use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use crate::components::{
    empty_state::EmptyState, header::Header, input_bar::InputBar, message_log::MessageLog,
    refresh_indicator::RefreshIndicator, session_list::SessionList, swipe_sidebar::SwipeSidebar,
};
use crate::state::AppState;

/// Root component — provides `AppState` context and renders the full layout.
#[component]
pub fn App() -> impl IntoView {
    let state = AppState::new();

    // Clone early for the closure.
    let state_for_init = state.clone();

    // On mount: fetch session list and auto-connect to hash or first session.
    Effect::new(move || {
        let state = state_for_init.clone();
        leptos::task::spawn_local(async move {
            state.fetch_sessions().await;

            // Determine initial session: URL hash > first in list.
            let hash_session = session_from_hash();
            if let Some(name) = hash_session {
                // If not in the list, add it (may exist on disk).
                let mut sessions = state.sessions.get_untracked();
                if !sessions.contains(&name) {
                    sessions.push(name.clone());
                    sessions.sort();
                    state.sessions.set(sessions);
                }
                state.connect_session(name);
            } else {
                let sessions = state.sessions.get_untracked();
                if let Some(first) = sessions.first() {
                    state.connect_session(first.clone());
                }
            }
        });
    });

    // ── global keyboard shortcuts ────────────────────────────────────

    // Runs once on mount (CSR-only app).  Ctrl+C cancels the running
    // prompt; when nothing is running, the browser handles it normally.
    {
        let state = state.clone();
        let handler = Closure::wrap(Box::new(move |evt: web_sys::KeyboardEvent| {
            if evt.ctrl_key() && evt.key() == "c" && state.running.get_untracked() {
                evt.prevent_default();
                state.cancel();
            }
        }) as Box<dyn FnMut(web_sys::KeyboardEvent)>);

        if let Some(window) = web_sys::window() {
            let _ = window
                .add_event_listener_with_callback("keydown", handler.as_ref().unchecked_ref());
        }

        // Leak the closure — App is the root component and never unmounts.
        handler.forget();
    }

    // ── PWA foreground reconnect ─────────────────────────────────────

    // When the page becomes visible again after being backgrounded (PWA
    // pause/resume on mobile), the WebSocket is often dead but the
    // on_close setTimeout callback may not have fired yet, so
    // `connection` still reads Connected.  Check the actual socket
    // state and reconnect if needed.
    {
        let state = state.clone();
        let handler = Closure::wrap(Box::new(move || {
            let visible = web_sys::window()
                .and_then(|w| w.document())
                .map(|d| !d.hidden())
                .unwrap_or(false);
            if !visible {
                return;
            }
            let session = match state.current_session.get_untracked() {
                Some(ref s) => s.clone(),
                None => return,
            };
            // If we're already Connected, verify the socket is still open
            // (the browser may have closed it while backgrounded without
            // the setTimeout callback having run yet).
            if state.connection.get_untracked().is_connected() {
                let alive = state
                    .ws
                    .get_untracked()
                    .map(|ws| ws.ready_state() == web_sys::WebSocket::OPEN)
                    .unwrap_or(false);
                if alive {
                    return;
                }
            }
            log::info!("visibilitychange: reconnecting to {session}");
            state.connect_session(session);
        }) as Box<dyn FnMut()>);

        if let Some(document) = web_sys::window().and_then(|w| w.document()) {
            let _ = document.add_event_listener_with_callback(
                "visibilitychange",
                handler.as_ref().unchecked_ref(),
            );
        }
        handler.forget();
    }

    // Provide state to all descendants.
    provide_context(state.clone());

    let has_session = Signal::derive(move || state.current_session.get().is_some());
    let show_empty = Signal::derive(move || {
        state.messages.get().is_empty() && state.current_session.get().is_none()
    });
    // Hint text only appears when we're not mid-catch-up (otherwise the
    // empty-state layout serves as a skeleton while history loads).
    let empty_show_hint = Signal::derive(move || !state.connection.get().is_catching_up());

    let sidebar_open = state.sidebar_open;
    let new_session = {
        let state = state.clone();
        move |_| {
            let state = state.clone();
            leptos::task::spawn_local(async move {
                let name = web_sys::window()
                    .and_then(|w| {
                        w.prompt_with_message("Session name (leave blank for auto-generated):")
                            .ok()
                    })
                    .and_then(|s| s)
                    .filter(|s| !s.trim().is_empty());
                if let Some(session_name) = state.create_session(name).await {
                    state.connect_session(session_name);
                }
            });
        }
    };

    view! {
        <SidebarOverlay sidebar_open />
        <div id="edgeZone"></div>
        <nav id="sidebar" class:open=sidebar_open>
            <div class="sidebar-header">
                <img
                    class="logo-icon"
                    src="/icon-192.png"
                    width="18"
                    height="18"
                    alt=""
                />
                <span>"goop"</span>
            </div>
            <SessionList />
            <div class="sidebar-footer">
                <button class="new-btn" id="newSessionBtn" on:click=new_session>
                    "+ New session"
                </button>
            </div>
        </nav>

        <div class="main">
            <RefreshIndicator />
            <Header />
            // Show empty state or message log.
            <Show
                when=move || !show_empty.get()
                fallback=move || view! { <EmptyState show_hint=empty_show_hint /> }
            >
                <MessageLog />
            </Show>
            // Input bar is visible when a session is selected.
            <Show when=move || has_session.get()>
                <InputBar />
            </Show>
        </div>

        <SwipeSidebar sidebar_open />

        // Standalone new-session fab — only shown on mobile when no session
        // is active (otherwise the one inside the footer handles it).
        <Show when=move || !has_session.get()>
            <button
                id="newSessionFab"
                title="New session"
                on:click={
                    let state = state.clone();
                    move |_| {
                        let state = state.clone();
                        leptos::task::spawn_local(async move {
                            let name = web_sys::window()
                                .and_then(|w| {
                                    w.prompt_with_message(
                                        "Session name (leave blank for auto-generated):",
                                    )
                                    .ok()
                                })
                                .and_then(|s| s)
                                .filter(|s| !s.trim().is_empty());
                            if let Some(session_name) = state.create_session(name).await {
                                state.connect_session(session_name);
                            }
                        });
                    }
                }
            >
                "+"
            </button>
        </Show>
    }
}

// ── sidebar overlay (mobile) ─────────────────────────────────────────

#[component]
fn SidebarOverlay(sidebar_open: RwSignal<bool>) -> impl IntoView {
    let close = move |_| sidebar_open.set(false);
    view! {
        <div
            class="sidebar-overlay"
            class:open=sidebar_open
            on:click=close
        ></div>
    }
}

// ── hash helpers ─────────────────────────────────────────────────────

fn session_from_hash() -> Option<String> {
    let hash = web_sys::window()?.location().hash().ok()?;
    if hash.is_empty() {
        return None;
    }
    // Strip leading '#'.
    let hash = &hash[1..];
    hash.strip_prefix("session=").map(|s| s.to_string())
}
