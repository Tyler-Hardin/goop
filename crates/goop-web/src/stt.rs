use wasm_bindgen::prelude::*;

// ── JS imports ───────────────────────────────────────────────────────

#[wasm_bindgen(module = "/js/stt.js")]
extern "C" {
    /// Start recording from the microphone.
    /// Returns a Promise that resolves when recording begins, rejects on error.
    fn sttStart() -> js_sys::Promise;

    /// Stop recording.
    /// `cancelled` — if true, discard the recording.
    /// Returns a Promise that resolves with a WAV `ArrayBuffer` or `null`.
    fn sttStop(cancelled: bool) -> js_sys::Promise;

    /// True while the JS recorder is active.
    fn sttIsRecording() -> bool;
}

// ── Rust wrapper ─────────────────────────────────────────────────────

/// Start recording.  Returns `Err(msg)` if mic access is denied or
/// MediaRecorder is unsupported.
pub async fn start() -> Result<(), String> {
    let promise = sttStart();
    wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map(|_| ())
        .map_err(|e| e.as_string().unwrap_or_else(|| "microphone error".into()))
}

/// Stop recording.  Returns the WAV data bytes, or `None` if cancelled
/// or the recording was empty.
pub async fn stop(cancelled: bool) -> Option<Vec<u8>> {
    let promise = sttStop(cancelled);
    let result = wasm_bindgen_futures::JsFuture::from(promise).await.ok()?;

    if result.is_null() || result.is_undefined() {
        return None;
    }

    let array_buf: js_sys::ArrayBuffer = result.unchecked_into();
    let uint8 = js_sys::Uint8Array::new(&array_buf);
    Some(uint8.to_vec())
}

/// Check whether the JS recorder is currently active.
#[allow(dead_code)]
pub fn is_recording() -> bool {
    sttIsRecording()
}
