//! PWA: service worker registration and push notification subscription.
//!
//! Called once at startup.  Registers `/sw.js` and, when ready,
//! requests notification permission and subscribes to push.
//!
//! Also listens for the `appinstalled` event — when the browser
//! transitions from tab to standalone PWA mode the push subscription
//! endpoint can change, so we re-subscribe.

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{PushManager, ServiceWorkerContainer, ServiceWorkerRegistration, window};

/// Register the service worker, set up push, and listen for install.
///
/// This is a fire-and-forget async operation called during app init.
pub async fn init() {
    register_service_worker().await;
    listen_app_installed();
}

/// Listen for the `appinstalled` event and re-subscribe push.
///
/// In Chrome (and some other browsers) the push subscription endpoint
/// changes when the PWA transitions from browser-tab to standalone
/// mode.  Without this, push notifications silently stop working for
/// users who install the app.
fn listen_app_installed() {
    let Some(window) = window() else {
        return;
    };

    let cb = Closure::<dyn Fn()>::new(move || {
        log::info!("PWA: installed — re-subscribing push");
        leptos::task::spawn_local(async {
            resubscribe_push().await;
        });
    });

    let _ = window.add_event_listener_with_callback("appinstalled", cb.as_ref().unchecked_ref());
    cb.forget();
}

/// Fetch the current service worker registration and re-subscribe push.
///
/// Called from the `appinstalled` handler (not at initial boot — that
/// path goes through [`register_service_worker`]).
async fn resubscribe_push() {
    let Some(window) = window() else {
        return;
    };

    let sw_container: ServiceWorkerContainer =
        match js_sys::Reflect::get(&window.navigator(), &"serviceWorker".into()) {
            Ok(v) if !v.is_undefined() => v.unchecked_into(),
            _ => return,
        };

    // getRegistration() returns the existing registration (or null).
    let reg: ServiceWorkerRegistration =
        match wasm_bindgen_futures::JsFuture::from(sw_container.get_registration()).await {
            Ok(v) if !v.is_null() && !v.is_undefined() => v.unchecked_into(),
            _ => {
                log::warn!("PWA: get_registration failed after install");
                return;
            }
        };

    subscribe_push(reg).await;
}

async fn register_service_worker() {
    let Some(window) = window() else {
        return;
    };

    // Check if serviceWorker is available.
    let sw_container: ServiceWorkerContainer =
        match js_sys::Reflect::get(&window.navigator(), &"serviceWorker".into()) {
            Ok(v) if !v.is_undefined() => v.unchecked_into(),
            _ => return,
        };

    // Register the service worker.
    let reg: ServiceWorkerRegistration =
        match wasm_bindgen_futures::JsFuture::from(sw_container.register("/sw.js")).await {
            Ok(reg) => {
                log::info!("PWA: service worker registered");
                reg.unchecked_into()
            }
            Err(_) => {
                log::warn!("PWA: service worker registration failed");
                return;
            }
        };

    // Once SW is ready, subscribe to push.
    subscribe_push(reg).await;
}

async fn subscribe_push(reg: ServiceWorkerRegistration) {
    let Some(window) = window() else {
        return;
    };

    // Check Notification API.
    let notification: js_sys::Object = match js_sys::Reflect::get(&window, &"Notification".into()) {
        Ok(v) if !v.is_undefined() => v.unchecked_into(),
        _ => {
            log::info!("PWA: Notification API not available");
            return;
        }
    };

    // Check PushManager.
    let push_manager: PushManager = match reg.push_manager() {
        Ok(pm) => pm,
        Err(_) => {
            log::info!("PWA: PushManager not available");
            return;
        }
    };

    // Request notification permission.
    let perm_promise: js_sys::Promise =
        js_sys::Reflect::get(&notification, &"requestPermission".into())
            .ok()
            .and_then(|f| f.dyn_ref::<js_sys::Function>().cloned())
            .map(|f| f.call0(&notification))
            .and_then(|r| r.ok())
            .and_then(|v| v.dyn_into::<js_sys::Promise>().ok())
            .unwrap_or_else(|| js_sys::Promise::resolve(&JsValue::NULL));

    let perm_result = wasm_bindgen_futures::JsFuture::from(perm_promise).await;
    let perm_str = perm_result
        .as_ref()
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default();

    if perm_str != "granted" {
        log::info!("PWA: notification permission not granted ({perm_str})");
        return;
    }

    // Fetch VAPID public key from the server.
    let public_key = match fetch_vapid_public_key().await {
        Some(k) => k,
        None => {
            log::warn!("PWA: failed to fetch VAPID public key");
            return;
        }
    };

    // Convert base64 to Uint8Array.
    let Ok(app_server_key) = url_base64_to_uint8array(&public_key) else {
        log::warn!("PWA: invalid VAPID public key");
        return;
    };

    // Build PushSubscriptionOptionsInit.
    let options = js_sys::Object::new();
    js_sys::Reflect::set(&options, &"userVisibleOnly".into(), &JsValue::TRUE).ok();
    js_sys::Reflect::set(&options, &"applicationServerKey".into(), &app_server_key).ok();

    // Call pushManager.subscribe(options).
    let subscribe_fn: js_sys::Function =
        match js_sys::Reflect::get(&push_manager, &"subscribe".into()) {
            Ok(f) => f.unchecked_into(),
            Err(_) => {
                log::warn!("PWA: pushManager.subscribe not available");
                return;
            }
        };

    let sub_result = match wasm_bindgen_futures::JsFuture::from(
        subscribe_fn
            .call1(&push_manager, &options)
            .ok()
            .and_then(|v| v.dyn_into::<js_sys::Promise>().ok())
            .unwrap_or_else(|| js_sys::Promise::resolve(&JsValue::NULL)),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            log::warn!("PWA: push subscribe failed: {:?}", e);
            return;
        }
    };
    let sub: web_sys::PushSubscription = sub_result.unchecked_into();

    // Extract toJSON and POST to server.
    let to_json_fn: js_sys::Function = match js_sys::Reflect::get(&sub, &"toJSON".into()) {
        Ok(f) => f.unchecked_into(),
        Err(_) => return,
    };
    let json_val = to_json_fn.call0(&sub).unwrap_or(JsValue::NULL);
    let json_str = js_sys::JSON::stringify(&json_val)
        .ok()
        .and_then(|s| s.as_string())
        .unwrap_or_default();

    let body = format!("{{\"subscription\":{json_str}}}");

    let request = match gloo_net::http::Request::post("/api/push-subscribe")
        .header("Content-Type", "application/json")
        .body(body)
    {
        Ok(req) => req,
        Err(e) => {
            log::warn!("PWA: failed to build push subscribe request: {e}");
            return;
        }
    };
    let _ = request.send().await;

    log::info!("PWA: push subscribed");
}

async fn fetch_vapid_public_key() -> Option<String> {
    let resp = gloo_net::http::Request::get("/api/vapid-public-key")
        .send()
        .await
        .ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    json.get("publicKey")?.as_str().map(|s| s.to_string())
}

/// Convert a URL-safe base64-encoded string to a `Uint8Array`.
fn url_base64_to_uint8array(s: &str) -> Result<js_sys::Uint8Array, JsValue> {
    // Convert URL-safe base64 to standard base64.
    let standard = s.replace('-', "+").replace('_', "/");
    // Add padding.
    let padding = (4 - (standard.len() % 4)) % 4;
    let padded = standard + &"=".repeat(padding);

    // Decode via the global `atob` function.
    let window = window().ok_or(JsValue::from_str("no window"))?;
    let atob: js_sys::Function = js_sys::Reflect::get(&window, &"atob".into())?.dyn_into()?;
    let raw: JsValue = atob.call1(&window, &padded.into())?;
    let raw_str: String = raw.as_string().ok_or(JsValue::from_str("atob failed"))?;

    let bytes: Vec<u8> = raw_str.as_bytes().to_vec();
    let array = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
    array.copy_from(&bytes);
    Ok(array)
}
