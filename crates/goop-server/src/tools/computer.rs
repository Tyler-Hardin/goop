//! Computer-use tools: `screenshot`, `cursor_position`, `mouse_move`,
//! `mouse_click`, `key_type`, `key_press`, `window_list`, `window_focus`,
//! `window_get_active`, `open_url`.
//!
//! These tools are local-only — they always operate on the local machine
//! regardless of the current transport.

use std::sync::Arc;

use rig::tool::ToolError;

use crate::session_state::SessionState;
use crate::tools::define_tool;

// ── helpers ───────────────────────────────────────────────────────

fn run_cmd(bin: &str, args: &[&str]) -> Result<std::process::Output, std::io::Error> {
    std::process::Command::new(bin).args(args).output()
}

fn require_bin(bin: &str) -> Result<(), ToolError> {
    match std::process::Command::new("which").arg(bin).output() {
        Ok(out) if out.status.success() => Ok(()),
        _ => Err(ToolError::ToolCallError(Box::new(std::io::Error::other(
            format!(
                "{bin} not found — install it with your package manager (e.g. nix-shell -p {bin})"
            ),
        )))),
    }
}

fn run_tool_cmd(bin: &str, args: &[&str]) -> Result<std::process::Output, ToolError> {
    run_cmd(bin, args).map_err(|e| ToolError::ToolCallError(Box::new(e)))
}

fn check_cmd(out: std::process::Output, ok_msg: impl Into<String>) -> Result<String, ToolError> {
    if out.status.success() {
        Ok(ok_msg.into())
    } else {
        Err(ToolError::ToolCallError(Box::new(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))))
    }
}

async fn tool_blocking<F>(bin: &str, f: F) -> Result<String, ToolError>
where
    F: FnOnce() -> Result<String, ToolError> + Send + 'static,
{
    require_bin(bin)?;
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ToolError::ToolCallError(Box::new(e)))?
}

// ═══════════════════════════════════════════════════════════════════
// Screenshot
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct Screenshot, args = ScreenshotArgs,
    tool_name: "screenshot",
    desc: "Take a screenshot of the current desktop and run OCR (tesseract) to extract visible text. Saves the image to the given path (default: /tmp/goop_screenshot.png). Set ocr=false to skip OCR. Requires scrot and tesseract.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Optional save path (default: /tmp/goop_screenshot.png)" },
            "ocr": { "type": "boolean", "description": "Run OCR on the screenshot? (default: true)" }
        }
    }),
    args { path: Option<std::path::PathBuf>, ocr: Option<bool> },
    |this, args| {
        require_bin("scrot")?;
        let ocr = args.ocr.unwrap_or(true);
        let img_path = args.path.unwrap_or_else(|| std::path::PathBuf::from("/tmp/goop_screenshot.png"));

        tokio::task::spawn_blocking(move || {
            let out = run_cmd("scrot", &["--overwrite", &img_path.to_string_lossy()])
                .map_err(|e| ToolError::ToolCallError(Box::new(e)))?;
            if !out.status.success() {
                return Err(ToolError::ToolCallError(Box::new(
                    std::io::Error::other(format!("scrot failed: {}", String::from_utf8_lossy(&out.stderr))),
                )));
            }
            let mut result = format!("Screenshot saved to {}", img_path.display());
            if ocr {
                if require_bin("tesseract").is_err() {
                    result.push_str("\n(OCR skipped: tesseract not installed)");
                    return Ok(result);
                }
                let base = img_path.with_extension("");
                let out = run_cmd("tesseract", &[&img_path.to_string_lossy(), &base.to_string_lossy()])
                    .map_err(|e| ToolError::ToolCallError(Box::new(e)))?;
                if !out.status.success() {
                    result.push_str(&format!("\n(OCR failed: {})", String::from_utf8_lossy(&out.stderr)));
                } else {
                    let txt_path = base.with_extension("txt");
                    match std::fs::read_to_string(&txt_path) {
                        Ok(ocr_text) => {
                            let trimmed = ocr_text.trim();
                            if trimmed.is_empty() {
                                result.push_str("\n(OCR returned no text)");
                            } else {
                                result.push_str(&format!("\n\nOCR text:\n{trimmed}"));
                            }
                        }
                        Err(e) => { result.push_str(&format!("\n(OCR output unreadable: {e}")); }
                    }
                }
            } else {
                result.push_str(" (OCR disabled)");
            }
            Ok(result)
        })
        .await
        .map_err(|e| ToolError::ToolCallError(Box::new(e)))?
    }
);

// ═══════════════════════════════════════════════════════════════════
// CursorPosition
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct CursorPosition, args = CursorPositionArgs,
    tool_name: "cursor_position",
    desc: "Get current mouse cursor position. Returns 'x y' coordinates (origin top-left).",
    params: serde_json::json!({ "type": "object", "properties": {} }),
    args {},
    |this, _args| {
        tool_blocking("xdotool", || {
            let out = run_tool_cmd("xdotool", &["getmouselocation", "--shell"])?;
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let mut x = None;
            let mut y = None;
            for line in stdout.lines() {
                if let Some(v) = line.strip_prefix("X=") { x = Some(v.to_string()); }
                if let Some(v) = line.strip_prefix("Y=") { y = Some(v.to_string()); }
            }
            match (x, y) {
                (Some(x), Some(y)) => Ok(format!("{x} {y}")),
                _ => Err(ToolError::ToolCallError(Box::new(
                    std::io::Error::other(format!("unexpected xdotool output: {stdout}")),
                ))),
            }
        }).await
    }
);

// ═══════════════════════════════════════════════════════════════════
// MouseMove
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct MouseMove, args = MouseMoveArgs,
    tool_name: "mouse_move",
    desc: "Move mouse cursor to absolute screen coordinates (x, y). Origin is top-left corner.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "x": { "type": "integer", "description": "X coordinate" },
            "y": { "type": "integer", "description": "Y coordinate" }
        },
        "required": ["x", "y"]
    }),
    args { x: i32, y: i32 },
    |this, args| {
        tool_blocking("xdotool", move || {
            check_cmd(
                run_tool_cmd("xdotool", &["mousemove", &args.x.to_string(), &args.y.to_string()])?,
                format!("Moved cursor to ({}, {})", args.x, args.y),
            )
        }).await
    }
);

// ═══════════════════════════════════════════════════════════════════
// MouseClick
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct MouseClick, args = MouseClickArgs,
    tool_name: "mouse_click",
    desc: "Click a mouse button. button: 'left' (default), 'right', or 'middle'. If x,y are given, moves cursor there first then clicks. Otherwise clicks at current position.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "button": { "type": "string", "description": "'left', 'right', or 'middle' (default: left)" },
            "x": { "type": "integer", "description": "Optional X coordinate" },
            "y": { "type": "integer", "description": "Optional Y coordinate" }
        }
    }),
    args { button: Option<String>, x: Option<i32>, y: Option<i32> },
    |this, args| {
        tool_blocking("xdotool", move || {
            let btn = match args.button.as_deref() {
                Some("right") => "3", Some("middle") => "2", _ => "1",
            };
            let btn_name = match btn { "3" => "right", "2" => "middle", _ => "left" };
            if let (Some(x), Some(y)) = (args.x, args.y) {
                check_cmd(
                    run_tool_cmd("xdotool", &["mousemove", &x.to_string(), &y.to_string(), "click", btn])?,
                    format!("Clicked {btn_name} at ({x}, {y})"),
                )
            } else {
                check_cmd(
                    run_tool_cmd("xdotool", &["click", btn])?,
                    format!("Clicked {btn_name} at current position"),
                )
            }
        }).await
    }
);

// ═══════════════════════════════════════════════════════════════════
// KeyType
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct KeyType, args = KeyTypeArgs,
    tool_name: "key_type",
    desc: "Type a string of text via the keyboard. Use for entering text into the focused window.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "text": { "type": "string", "description": "Text to type" }
        },
        "required": ["text"]
    }),
    args { text: String },
    |this, args| {
        let text = args.text.clone();
        tool_blocking("xdotool", move || {
            check_cmd(run_tool_cmd("xdotool", &["type", "--", &text])?, format!("Typed: {text}"))
        }).await
    }
);

// ═══════════════════════════════════════════════════════════════════
// KeyPress
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct KeyPress, args = KeyPressArgs,
    tool_name: "key_press",
    desc: "Press a key combination like 'ctrl+c', 'alt+Tab', 'super', 'Return', 'Escape', etc. Keys are xdotool key names.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "combo": { "type": "string", "description": "Key combination (xdotool key name)" }
        },
        "required": ["combo"]
    }),
    args { combo: String },
    |this, args| {
        let combo = args.combo.clone();
        tool_blocking("xdotool", move || {
            check_cmd(run_tool_cmd("xdotool", &["key", &combo])?, format!("Pressed: {combo}"))
        }).await
    }
);

// ═══════════════════════════════════════════════════════════════════
// WindowList
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct WindowList, args = WindowListArgs,
    tool_name: "window_list",
    desc: "List all open windows with their IDs, WM_CLASS (stable identifier like 'Navigator.firefox'), and titles. Use with window_focus to switch to a specific window.",
    params: serde_json::json!({ "type": "object", "properties": {} }),
    args {},
    |this, _args| {
        tool_blocking("wmctrl", || {
            let out = run_tool_cmd("wmctrl", &["-lx"])?;
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            if stdout.trim().is_empty() { return Ok(String::from("No windows found.")); }
            let lines: Vec<String> = stdout.lines().map(|line| {
                let trimmed = line.trim();
                let cols = trimmed.split_whitespace().collect::<Vec<_>>();
                if cols.len() >= 5 {
                    format!("{}  class={}  \"{}\"", cols[0], cols[2], cols[4..].join(" "))
                } else {
                    trimmed.to_string()
                }
            }).collect();
            Ok(lines.join("\n"))
        }).await
    }
);

// ═══════════════════════════════════════════════════════════════════
// WindowFocus
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct WindowFocus, args = WindowFocusArgs,
    tool_name: "window_focus",
    desc: "Focus (raise and activate) a window by searching its title or WM_CLASS. Uses substring match. Prefer `class` (e.g. 'Navigator.firefox') for stable matching across page title changes.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "class": { "type": "string", "description": "WM_CLASS to match" },
            "title": { "type": "string", "description": "Window title substring" }
        }
    }),
    args { title: Option<String>, class: Option<String> },
    |this, args| {
        tool_blocking("wmctrl", move || {
            if let Some(ref cls) = args.class {
                check_cmd(run_tool_cmd("wmctrl", &["-x", "-a", cls])?, format!("Focused window by class '{cls}'"))
            } else if let Some(ref t) = args.title {
                check_cmd(run_tool_cmd("wmctrl", &["-a", t])?, format!("Focused window by title '{t}'"))
            } else {
                Err(ToolError::ToolCallError(Box::new(std::io::Error::other("Must provide either `title` or `class`"))))
            }
        }).await
    }
);

// ═══════════════════════════════════════════════════════════════════
// WindowGetActive
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct WindowGetActive, args = WindowGetActiveArgs,
    tool_name: "window_get_active",
    desc: "Get the currently active (focused) window: its ID, title, and geometry (position and size).",
    params: serde_json::json!({ "type": "object", "properties": {} }),
    args {},
    |this, _args| {
        tool_blocking("xdotool", || {
            let id_out = run_tool_cmd("xdotool", &["getactivewindow"])?;
            let id = String::from_utf8_lossy(&id_out.stdout).trim().to_string();
            if id.is_empty() { return Err(ToolError::ToolCallError(Box::new(std::io::Error::other("No active window")))); }
            let name = String::from_utf8_lossy(
                &run_cmd("xdotool", &["getwindowname", &id]).map(|o| o.stdout).unwrap_or_default(),
            ).trim().to_string();
            let geom_raw = String::from_utf8_lossy(
                &run_cmd("xdotool", &["getwindowgeometry", "--shell", &id]).map(|o| o.stdout).unwrap_or_default(),
            ).into_owned();
            let mut x = ""; let mut y = ""; let mut w = ""; let mut h = "";
            for line in geom_raw.lines() {
                if let Some(v) = line.strip_prefix("X=") { x = v; }
                if let Some(v) = line.strip_prefix("Y=") { y = v; }
                if let Some(v) = line.strip_prefix("WIDTH=") { w = v; }
                if let Some(v) = line.strip_prefix("HEIGHT=") { h = v; }
            }
            Ok(format!("Window {id}: \"{name}\" — position=({x},{y}) size={w}x{h}"))
        }).await
    }
);

// ═══════════════════════════════════════════════════════════════════
// OpenUrl
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct OpenUrl, args = OpenUrlArgs,
    tool_name: "open_url",
    desc: "Open a URL in the default web browser using xdg-open.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "url": { "type": "string", "description": "URL to open" }
        },
        "required": ["url"]
    }),
    args { url: String },
    |this, args| {
        let url = args.url.clone();
        tool_blocking("xdg-open", move || {
            let out = run_tool_cmd("xdg-open", &[&url])?;
            if out.status.success() {
                Ok(format!("Opened {url}"))
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr);
                if stderr.is_empty() { Ok(format!("Opened {url}")) }
                else { Ok(format!("Opened {url} (stderr: {stderr})")) }
            }
        }).await
    }
);
