use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::state::{AppState, ConnectionState};

#[component]
pub fn Header() -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState missing");

    let connection = state.connection;
    let current_session = state.current_session;
    let running = state.running;
    let sidebar_open = state.sidebar_open;

    // Status text: connected / reconnecting… / no session / catching up…
    let status_text = Signal::derive(move || {
        let session = current_session.get();
        if session.is_none() {
            return "no session".to_string();
        }
        match connection.get() {
            ConnectionState::Connected => {
                if running.get() {
                    "running…".to_string()
                } else {
                    "connected".to_string()
                }
            }
            ConnectionState::CatchingUp => "loading…".to_string(),
            ConnectionState::Disconnected => "reconnecting…".to_string(),
        }
    });

    let toggle_sidebar = move |_| {
        sidebar_open.update(|v| *v = !*v);
    };

    let do_reload = move |_| {
        if let Some(window) = web_sys::window() {
            leptos::task::spawn_local(async move {
                // Unregister all service workers so stale code can't survive.
                if let Ok(sw) = js_sys::Reflect::get(&window.navigator(), &"serviceWorker".into())
                    && !sw.is_undefined()
                {
                    let sw: web_sys::ServiceWorkerContainer = sw.unchecked_into();
                    let promise = sw.get_registrations();
                    if let Ok(regs) = wasm_bindgen_futures::JsFuture::from(promise).await {
                        let regs: js_sys::Array = regs.unchecked_into();
                        for i in 0..regs.length() {
                            if let Some(reg) =
                                regs.get(i).dyn_ref::<web_sys::ServiceWorkerRegistration>()
                                && let Ok(promise) = reg.unregister()
                            {
                                let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                            }
                        }
                    }
                }
                // Nuke all cache storage.
                if let Ok(caches_val) = js_sys::Reflect::get(&window, &"caches".into())
                    && !caches_val.is_undefined()
                {
                    let caches: web_sys::CacheStorage = caches_val.unchecked_into();
                    let promise = caches.keys();
                    if let Ok(keys) = wasm_bindgen_futures::JsFuture::from(promise).await {
                        let keys: js_sys::Array = keys.unchecked_into();
                        for i in 0..keys.length() {
                            if let Some(key) = keys.get(i).as_string() {
                                let _ =
                                    wasm_bindgen_futures::JsFuture::from(caches.delete(&key)).await;
                            }
                        }
                    }
                }
                let _ = window.location().reload();
            });
        }
    };

    view! {
        <header>
            <button class="menu-btn" id="menuBtn" title="Sessions" on:click=toggle_sidebar>
                "☰"
            </button>
            <div class="dot" class:live=move || connection.get().is_ws_open() id="dot"></div>
            <span class="title" id="title">
                {move || current_session.get().unwrap_or_else(|| "goop".into())}
            </span>
            <span class="status" id="status">{move || status_text.get()}</span>
            <button
                class="reload-btn"
                id="reloadBtn"
                title="Force reload (clear cache)"
                on:click=do_reload
            >
                "↻"
            </button>
        </header>
    }
}
