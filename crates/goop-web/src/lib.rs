mod app;
mod components;
mod markdown;
mod pwa;
mod state;
mod stt;
mod ws;

use leptos::mount::mount_to_body;
use leptos::prelude::*;
use wasm_bindgen::prelude::wasm_bindgen;

use crate::app::App;

#[wasm_bindgen(start)]
pub fn main() {
    console_error_panic_hook::set_once();
    console_log::init_with_level(log::Level::Debug).ok();

    // Show panics in the DOM so we can debug.
    std::panic::set_hook(Box::new(|info| {
        if let Some(body) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.body())
        {
            body.set_text_content(Some(&format!("PANIC: {info}")));
        }
    }));

    mount_to_body(|| view! { <App /> });

    // After mount_to_body, the Leptos runtime is initialized.
    leptos::task::spawn_local(async {
        pwa::init().await;
    });
}
