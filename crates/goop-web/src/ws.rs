use goop_shared::SessionEvent;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use crate::components::input_button::BtnState;
use crate::state::{AppState, ConnectionState};

/// Open a WebSocket connection to the goop server for the given session.
///
/// Stores the WS handle and sets up callbacks on the given `AppState`.
/// If a connection is already open, it is closed first.
pub fn connect(state: &AppState, session_name: String) {
    // Close existing connection if any.  We do NOT touch connection state
    // or history_buffer here — the old on_close will fire asynchronously,
    // but it captures an old connection_gen and will be a no-op.
    disconnect(state);

    // Bump the generation counter so any in-flight on_close from the
    // old connection becomes a no-op.
    let conn_gen = state.connection_gen.get_untracked() + 1;
    state.connection_gen.set(conn_gen);

    // Begin history catch-up mode: events will be buffered until
    // the server sends HistoryComplete, then flushed in one batch.
    state.history_buffer.set(Vec::new());
    state.connection.set(ConnectionState::CatchingUp);
    // Reset the seq counter — the next session's events start at seq 0.
    state.seq_counter.set(0);
    state.streaming_seq.set(None);
    // Clear the context-usage bar so the previous session's value
    // doesn't linger until history replay completes.
    state.context_usage.set(None);

    let w = web_sys::window().expect("no global window");
    let host = w
        .location()
        .host()
        .unwrap_or_else(|_| "127.0.0.1:8187".into());
    let proto = if w.location().protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    let url = format!("{proto}://{host}/ws?session={session_name}");

    let ws = web_sys::WebSocket::new(&url).expect("failed to create WebSocket");
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    let state_on_open = state.clone();
    let on_open = Closure::<dyn Fn()>::new(move || {
        log::info!("WS connected");
        // Connection opened successfully — reset the auto-reconnect
        // backoff so the next unexpected disconnect starts fresh.
        state_on_open.reconnect_attempt.set(0);
        // Connection state stays at CatchingUp — HistoryComplete promotes
        // it to Connected in handle_event().  The dot turns green via
        // is_ws_open() which already covers CatchingUp.
        state_on_open.btn_state.update(|s| {
            if *s == BtnState::Disabled {
                *s = BtnState::Idle;
            }
        });
    });
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    on_open.forget();

    let state_on_msg = state.clone();
    let on_message =
        Closure::<dyn Fn(web_sys::MessageEvent)>::new(move |evt: web_sys::MessageEvent| {
            if let Some(text) = evt.data().as_string() {
                match serde_json::from_str::<SessionEvent>(&text) {
                    Ok(event) => {
                        let state = state_on_msg.clone();
                        if let Some(window) = web_sys::window() {
                            let cb = Closure::once(move || {
                                state.handle_event(event);
                            });
                            let _ = window.set_timeout_with_callback(cb.as_ref().unchecked_ref());
                            cb.forget();
                        }
                    }
                    Err(e) => log::warn!("failed to parse WS message: {e}"),
                }
            }
        });
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    // Capture the generation at the time this connection was created.
    // If this on_close fires after a newer connect() has already bumped
    // the gen, the cleanup is silently skipped.
    let state_on_close = state.clone();
    let on_close = Closure::<dyn Fn(web_sys::CloseEvent)>::new(move |_| {
        log::info!("WS disconnected");
        let state = state_on_close.clone();
        if let Some(window) = web_sys::window() {
            let cb = Closure::once(move || {
                // Only clean up if this is still the current connection.
                if state.connection_gen.get_untracked() == conn_gen {
                    state.connection.set(ConnectionState::Disconnected);
                    state.ws.set(None);
                    state.history_buffer.set(Vec::new());
                    state.btn_state.set(BtnState::Disabled);

                    // Auto-reconnect on unexpected disconnect (server
                    // restart, network loss).  Bump the backoff
                    // counter first, then schedule reconnection.
                    // schedule_reconnect checks connection_gen before
                    // attempting, so a manual reconnect will preempt.
                    state.reconnect_attempt.update(|n| *n += 1);
                    state.schedule_reconnect(conn_gen);
                }
            });
            let _ = window.set_timeout_with_callback(cb.as_ref().unchecked_ref());
            cb.forget();
        }
    });
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    on_close.forget();

    state.ws.set(Some(ws));
}

/// Close the current WebSocket connection, if any.
///
/// Does NOT touch `connection` or `history_buffer` — those are owned
/// by the `on_close` handler (which checks `connection_gen` before
/// mutating state) and by `connect()`.
pub fn disconnect(state: &AppState) {
    if let Some(ws) = state.ws.get_untracked() {
        state.ws.set(None);
        ws.close().ok();
    }
}
