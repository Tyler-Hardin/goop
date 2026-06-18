use wasm_bindgen::JsCast;

/// Render raw markdown into sanitized HTML using marked.js + DOMPurify.
///
/// Both must be loaded via `<script>` tags (in `index.html`) before this
/// function is called.  Falls back to HTML-escaped plain text if not.
pub fn render_markdown(raw: &str) -> String {
    let window = match web_sys::window() {
        Some(w) => w,
        None => return html_escape(raw),
    };

    // Call marked.parse(raw)
    let marked = match js_sys::Reflect::get(&window, &"marked".into()) {
        Ok(m) => m,
        Err(_) => return html_escape(raw),
    };
    let marked_parse = match js_sys::Reflect::get(&marked, &"parse".into()) {
        Ok(f) => f,
        Err(_) => return html_escape(raw),
    };
    let marked_parse: &js_sys::Function = match marked_parse.dyn_ref() {
        Some(f) => f,
        None => return html_escape(raw),
    };
    let html = match marked_parse.call1(&marked, &raw.into()) {
        Ok(h) => h,
        Err(_) => return html_escape(raw),
    };

    // Call DOMPurify.sanitize(html)
    let dompurify = match js_sys::Reflect::get(&window, &"DOMPurify".into()) {
        Ok(d) => d,
        Err(_) => return html.as_string().unwrap_or_else(|| html_escape(raw)),
    };
    let sanitize = match js_sys::Reflect::get(&dompurify, &"sanitize".into()) {
        Ok(f) => f,
        Err(_) => return html.as_string().unwrap_or_else(|| html_escape(raw)),
    };
    let sanitize: &js_sys::Function = match sanitize.dyn_ref() {
        Some(f) => f,
        None => return html.as_string().unwrap_or_else(|| html_escape(raw)),
    };
    let safe = match sanitize.call1(&dompurify, &html) {
        Ok(s) => s,
        Err(_) => return html.as_string().unwrap_or_else(|| html_escape(raw)),
    };

    safe.as_string().unwrap_or_else(|| html_escape(raw))
}

/// Minimal HTML escape for plain-text fallback.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
