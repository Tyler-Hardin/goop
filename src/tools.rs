use rig_derive::rig_tool;

#[rig_tool(
    description = "Read file at path, optionally with start_line and end_line (both 1-indexed, inclusive). Returns line-numbered content.",
    required(command)
)]
pub async fn read(
    path: std::path::PathBuf,
    start_line: Option<u64>,
    end_line: Option<u64>,
) -> Result<String, rig::tool::ToolError> {
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;

    let all_lines: Vec<&str> = content.lines().collect();
    let total = all_lines.len() as u64;

    let start = start_line.unwrap_or(1).max(1);
    let end = end_line.unwrap_or(total).min(total);

    if start > total {
        return Err(rig::tool::ToolError::ToolCallError(Box::new(
            std::io::Error::other(format!(
                "start_line {start} exceeds file length ({total} lines)"
            )),
        )));
    }

    if start > end {
        return Err(rig::tool::ToolError::ToolCallError(Box::new(
            std::io::Error::other(format!("start_line {start} > end_line {end}")),
        )));
    }

    let output: String = all_lines[(start - 1) as usize..end as usize]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{}", start as usize + i, line))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(output)
}

#[rig_tool(
    description = "Replace old_str with new_str in file at path. old_str must be unique.",
    required(command)
)]
pub async fn replace(
    path: std::path::PathBuf,
    old_str: String,
    new_str: String,
) -> Result<String, rig::tool::ToolError> {
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    let count = content.matches(&old_str).count();
    if count == 0 {
        Err(rig::tool::ToolError::ToolCallError(Box::new(
            std::io::Error::new(std::io::ErrorKind::NotFound, "old_str not found"),
        )))
    } else if count > 1 {
        Err(rig::tool::ToolError::ToolCallError(Box::new(
            std::io::Error::other(format!("old_str found {count} times, must be unique")),
        )))
    } else {
        let new_content = content.replacen(&old_str, &new_str, 1);
        tokio::fs::write(&path, &new_content)
            .await
            .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
        Ok(format!("Replaced 1 occurrence in {}", path.display()))
    }
}

#[rig_tool(description = "Run command in shell", required(command))]
pub async fn shell(command: String) -> Result<String, rig::tool::ToolError> {
    tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .output()
        .await
        .map(|out| {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.is_empty() {
                stdout.into_owned()
            } else {
                format!("{stdout}{stderr}")
            }
        })
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))
}

#[rig_tool(
    description = "Read an HTML file at path and return extracted plain text (headings, links, body text). Useful for local crate docs, cached pages, etc.",
    required(path)
)]
pub async fn read_html(path: std::path::PathBuf) -> Result<String, rig::tool::ToolError> {
    let html = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    tokio::task::spawn_blocking(move || {
        html2text::from_read(html.as_bytes(), 80).map_err(|e| {
            rig::tool::ToolError::ToolCallError(Box::new(std::io::Error::other(e.to_string())))
        })
    })
    .await
    .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?
}

#[rig_tool(
    description = "Fetch a URL and return extracted plain text from the HTML (headings, links, body text). Use for reading web docs, wiki pages, etc.",
    required(url)
)]
pub async fn web_fetch(url: String) -> Result<String, rig::tool::ToolError> {
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(rig::tool::ToolError::ToolCallError(Box::new(
            std::io::Error::other(format!("HTTP {status}")),
        )));
    }
    let html = resp
        .text()
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    let text = tokio::task::spawn_blocking({
        let html = html.clone();
        move || {
            html2text::from_read(html.as_bytes(), 80).map_err(|e| {
                rig::tool::ToolError::ToolCallError(Box::new(std::io::Error::other(e.to_string())))
            })
        }
    })
    .await
    .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))??;

    // Write cached copies to temp files so the model can re-read with
    // the `read` or `read_html` tools without re-fetching.
    let dir = std::env::temp_dir().join("goop");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    let stem = slugify(&url);
    let txt_path = dir.join(format!("{stem}.txt"));
    let html_path = dir.join(format!("{stem}.html"));
    tokio::fs::write(&txt_path, &text)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    tokio::fs::write(&html_path, &html)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;

    Ok(format!(
        "{text}\n\n---\nCached: {} (plain text) and {} (raw HTML) — use `read` or `read_html` or `shell` (e.g. grep) to re-examine if needed.",
        txt_path.display(),
        html_path.display(),
    ))
}

/// Turn a URL into a safe filename fragment.
fn slugify(url: &str) -> String {
    url.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect::<String>()
        .chars()
        .take(120)
        .collect()
}

#[rig_tool(description = "Write content to file at path", required(path, content))]
pub async fn write(
    path: std::path::PathBuf,
    content: String,
) -> Result<String, rig::tool::ToolError> {
    tokio::fs::write(&path, &content)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    Ok(format!(
        "Wrote {} bytes to {}",
        content.len(),
        path.display()
    ))
}

// ═══════════════════════════════════════════════════════════════════
// Computer-use helpers
// ═══════════════════════════════════════════════════════════════════

/// Run a command synchronously and return (stdout, stderr, exit_status).
fn run_cmd(bin: &str, args: &[&str]) -> Result<std::process::Output, std::io::Error> {
    std::process::Command::new(bin).args(args).output()
}

/// Check that a binary exists; return a ToolError if not.
fn require_bin(bin: &str) -> Result<(), rig::tool::ToolError> {
    match std::process::Command::new("which").arg(bin).output() {
        Ok(out) if out.status.success() => Ok(()),
        _ => Err(rig::tool::ToolError::ToolCallError(Box::new(
            std::io::Error::other(format!(
                "{bin} not found — install it with your package manager (e.g. nix-shell -p {bin})"
            )),
        ))),
    }
}

/// Run a synchronous command, auto-wrapping I/O errors into ToolError.
fn run_tool_cmd(bin: &str, args: &[&str]) -> Result<std::process::Output, rig::tool::ToolError> {
    run_cmd(bin, args).map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))
}

/// Check a command's exit status. Returns `ok_msg` on success, or a
/// ToolError with stderr on failure.
fn check_cmd(
    out: std::process::Output,
    ok_msg: impl Into<String>,
) -> Result<String, rig::tool::ToolError> {
    if out.status.success() {
        Ok(ok_msg.into())
    } else {
        Err(rig::tool::ToolError::ToolCallError(Box::new(
            std::io::Error::other(String::from_utf8_lossy(&out.stderr).into_owned()),
        )))
    }
}

/// Run a closure on the blocking thread pool after verifying `bin` is
/// installed.  The single helper replaces the repeated `require_bin` +
/// `spawn_blocking` + join-error boilerplate in every computer-use tool.
async fn tool_blocking<F>(bin: &str, f: F) -> Result<String, rig::tool::ToolError>
where
    F: FnOnce() -> Result<String, rig::tool::ToolError> + Send + 'static,
{
    require_bin(bin)?;
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?
}

// ═══════════════════════════════════════════════════════════════════
// Computer-use tools (Linux/X11)
// ═══════════════════════════════════════════════════════════════════

#[rig_tool(
    description = "Take a screenshot of the current desktop and run OCR (tesseract) to extract visible text. Saves the image to the given path (default: /tmp/goop_screenshot.png). Set ocr=false to skip OCR. Requires scrot and tesseract.",
    required(command)
)]
pub async fn screenshot(
    path: Option<std::path::PathBuf>,
    ocr: Option<bool>,
) -> Result<String, rig::tool::ToolError> {
    require_bin("scrot")?;
    let ocr = ocr.unwrap_or(true);

    let img_path = path.unwrap_or_else(|| std::path::PathBuf::from("/tmp/goop_screenshot.png"));

    tokio::task::spawn_blocking(move || {
        // Take screenshot
        let out = run_cmd("scrot", &["--overwrite", &img_path.to_string_lossy()])
            .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
        if !out.status.success() {
            return Err(rig::tool::ToolError::ToolCallError(Box::new(
                std::io::Error::other(format!(
                    "scrot failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                )),
            )));
        }

        let mut result = format!("Screenshot saved to {}", img_path.display());

        if ocr {
            if require_bin("tesseract").is_err() {
                result.push_str("\n(OCR skipped: tesseract not installed)");
                return Ok(result);
            }
            let base = img_path.with_extension("");
            let out = run_cmd(
                "tesseract",
                &[&img_path.to_string_lossy(), &base.to_string_lossy()],
            )
            .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
            if !out.status.success() {
                result.push_str(&format!(
                    "\n(OCR failed: {})",
                    String::from_utf8_lossy(&out.stderr)
                ));
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
                    Err(e) => {
                        result.push_str(&format!("\n(OCR output unreadable: {e}"));
                    }
                }
            }
        } else {
            result.push_str(" (OCR disabled)");
        }

        Ok(result)
    })
    .await
    .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?
}

#[rig_tool(
    description = "Get current mouse cursor position. Returns 'x y' coordinates (origin top-left).",
    required(command)
)]
pub async fn cursor_position() -> Result<String, rig::tool::ToolError> {
    tool_blocking("xdotool", || {
        let out = run_tool_cmd("xdotool", &["getmouselocation", "--shell"])?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut x = None;
        let mut y = None;
        for line in stdout.lines() {
            if let Some(v) = line.strip_prefix("X=") {
                x = Some(v.to_string());
            }
            if let Some(v) = line.strip_prefix("Y=") {
                y = Some(v.to_string());
            }
        }
        match (x, y) {
            (Some(x), Some(y)) => Ok(format!("{x} {y}")),
            _ => Err(rig::tool::ToolError::ToolCallError(Box::new(
                std::io::Error::other(format!("unexpected xdotool output: {stdout}")),
            ))),
        }
    })
    .await
}

#[rig_tool(
    description = "Move mouse cursor to absolute screen coordinates (x, y). Origin is top-left corner.",
    required(command)
)]
pub async fn mouse_move(x: i32, y: i32) -> Result<String, rig::tool::ToolError> {
    tool_blocking("xdotool", move || {
        check_cmd(
            run_tool_cmd("xdotool", &["mousemove", &x.to_string(), &y.to_string()])?,
            format!("Moved cursor to ({x}, {y})"),
        )
    })
    .await
}

#[rig_tool(
    description = "Click a mouse button. button: 'left' (default), 'right', or 'middle'. If x,y are given, moves cursor there first then clicks. Otherwise clicks at current position.",
    required(command)
)]
pub async fn mouse_click(
    button: Option<String>,
    x: Option<i32>,
    y: Option<i32>,
) -> Result<String, rig::tool::ToolError> {
    tool_blocking("xdotool", move || {
        let btn = match button.as_deref() {
            Some("right") => "3",
            Some("middle") => "2",
            _ => "1",
        };
        let btn_name = match btn {
            "3" => "right",
            "2" => "middle",
            _ => "left",
        };

        if let (Some(x), Some(y)) = (x, y) {
            check_cmd(
                run_tool_cmd(
                    "xdotool",
                    &["mousemove", &x.to_string(), &y.to_string(), "click", btn],
                )?,
                format!("Clicked {btn_name} at ({x}, {y})"),
            )
        } else {
            check_cmd(
                run_tool_cmd("xdotool", &["click", btn])?,
                format!("Clicked {btn_name} at current position"),
            )
        }
    })
    .await
}

#[rig_tool(
    description = "Type a string of text via the keyboard. Use for entering text into the focused window.",
    required(command)
)]
pub async fn key_type(text: String) -> Result<String, rig::tool::ToolError> {
    tool_blocking("xdotool", move || {
        check_cmd(
            run_tool_cmd("xdotool", &["type", "--", &text])?,
            format!("Typed: {text}"),
        )
    })
    .await
}

#[rig_tool(
    description = "Press a key combination like 'ctrl+c', 'alt+Tab', 'super', 'Return', 'Escape', etc. Keys are xdotool key names.",
    required(command)
)]
pub async fn key_press(combo: String) -> Result<String, rig::tool::ToolError> {
    tool_blocking("xdotool", move || {
        check_cmd(
            run_tool_cmd("xdotool", &["key", &combo])?,
            format!("Pressed: {combo}"),
        )
    })
    .await
}

#[rig_tool(
    description = "List all open windows with their IDs, WM_CLASS (stable identifier like 'Navigator.firefox'), and titles. Use with window_focus to switch to a specific window.",
    required(command)
)]
pub async fn window_list() -> Result<String, rig::tool::ToolError> {
    tool_blocking("wmctrl", || {
        let out = run_tool_cmd("wmctrl", &["-lx"])?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();

        if stdout.trim().is_empty() {
            return Ok(String::from("No windows found."));
        }

        // wmctrl -lx format: WINDOW_ID  DESKTOP  WM_CLASS  HOSTNAME  TITLE
        let lines: Vec<String> = stdout
            .lines()
            .map(|line| {
                let trimmed = line.trim();
                let cols = trimmed.split_whitespace().collect::<Vec<_>>();
                if cols.len() >= 5 {
                    let id = cols[0];
                    let class = cols[2];
                    let title = cols[4..].join(" ");
                    format!("{id}  class={class}  \"{title}\"")
                } else {
                    trimmed.to_string()
                }
            })
            .collect();

        Ok(lines.join("\n"))
    })
    .await
}

#[rig_tool(
    description = "Focus (raise and activate) a window by searching its title or WM_CLASS. Uses substring match. Prefer `class` (e.g. 'Navigator.firefox') for stable matching across page title changes.",
    required(command)
)]
pub async fn window_focus(
    title: Option<String>,
    class: Option<String>,
) -> Result<String, rig::tool::ToolError> {
    tool_blocking("wmctrl", move || {
        if let Some(ref cls) = class {
            check_cmd(
                run_tool_cmd("wmctrl", &["-x", "-a", cls])?,
                format!("Focused window by class '{cls}'"),
            )
        } else if let Some(ref t) = title {
            check_cmd(
                run_tool_cmd("wmctrl", &["-a", t])?,
                format!("Focused window by title '{t}'"),
            )
        } else {
            Err(rig::tool::ToolError::ToolCallError(Box::new(
                std::io::Error::other("Must provide either `title` or `class`"),
            )))
        }
    })
    .await
}

#[rig_tool(
    description = "Get the currently active (focused) window: its ID, title, and geometry (position and size).",
    required(command)
)]
pub async fn window_get_active() -> Result<String, rig::tool::ToolError> {
    tool_blocking("xdotool", || {
        let id_out = run_tool_cmd("xdotool", &["getactivewindow"])?;
        let id = String::from_utf8_lossy(&id_out.stdout).trim().to_string();
        if id.is_empty() {
            return Err(rig::tool::ToolError::ToolCallError(Box::new(
                std::io::Error::other("No active window"),
            )));
        }

        let name = String::from_utf8_lossy(
            &run_cmd("xdotool", &["getwindowname", &id])
                .map(|o| o.stdout)
                .unwrap_or_default(),
        )
        .trim()
        .to_string();

        let geom_raw = String::from_utf8_lossy(
            &run_cmd("xdotool", &["getwindowgeometry", "--shell", &id])
                .map(|o| o.stdout)
                .unwrap_or_default(),
        )
        .into_owned();

        let mut x = "";
        let mut y = "";
        let mut w = "";
        let mut h = "";
        for line in geom_raw.lines() {
            if let Some(v) = line.strip_prefix("X=") {
                x = v;
            }
            if let Some(v) = line.strip_prefix("Y=") {
                y = v;
            }
            if let Some(v) = line.strip_prefix("WIDTH=") {
                w = v;
            }
            if let Some(v) = line.strip_prefix("HEIGHT=") {
                h = v;
            }
        }

        Ok(format!(
            "Window {id}: \"{name}\" — position=({x},{y}) size={w}x{h}"
        ))
    })
    .await
}

#[rig_tool(
    description = "Open a URL in the default web browser using xdg-open.",
    required(url)
)]
pub async fn open_url(url: String) -> Result<String, rig::tool::ToolError> {
    tool_blocking("xdg-open", move || {
        let out = run_tool_cmd("xdg-open", &[&url])?;
        if out.status.success() {
            Ok(format!("Opened {url}"))
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // xdg-open often succeeds even with stderr output, so just note it
            if stderr.is_empty() {
                Ok(format!("Opened {url}"))
            } else {
                Ok(format!("Opened {url} (stderr: {stderr})"))
            }
        }
    })
    .await
}
