//! Agent preamble: builds the system prompt from the Tera template,
//! environment context, and user/project memory files (USER.md, SYSTEM.md,
//! AGENTS.md).
//!
//! Order is deliberate to maximise prompt-cache prefix re-use:
//!   1. Static guidelines (never changes)
//!   2. User + OS info (changes only with system upgrades)
//!   3. USER.md (persistent user memory; changes rarely)
//!   4. SYSTEM.md (per-machine conventions; changes with ssh/disconnect)
//!   5. CWD (changes per session / cd)
//!   6. AGENTS.md (project context; changes per cd/ssh/disconnect)

use std::path::Path;

/// Render the agent preamble from the Tera template and env context.
/// Reads SYSTEM.md and AGENTS.md from the local filesystem.
pub fn build_preamble(cwd: &str, home_dir: &Path) -> String {
    let user_md = read_user_md();
    let system_md = read_system_md();
    let agents_md = {
        let agents_path = Path::new(cwd).join("AGENTS.md");
        std::fs::read_to_string(&agents_path).ok()
    };
    render_preamble(
        cwd,
        home_dir,
        &user_md,
        system_md.as_deref(),
        agents_md.as_deref(),
    )
}

/// Render the agent preamble with pre-supplied SYSTEM.md and AGENTS.md
/// content.
///
/// Both are obtained via the active transport, so they work across SSH.
/// Used when rebuilding the preamble after `cd` / `ssh` / `disconnect`.
pub fn build_preamble_with(
    cwd: &str,
    home_dir: &Path,
    system_md: Option<&str>,
    agents_md: Option<&str>,
) -> String {
    let user_md = read_user_md();
    render_preamble(cwd, home_dir, &user_md, system_md, agents_md)
}

// ── helpers ────────────────────────────────────────────────────────

fn read_user_md() -> String {
    let path = crate::config::user_md_path();
    read_or_create_md(&path)
}

/// Read SYSTEM.md from the path given by [`crate::config::system_md_path`]
/// on the local machine.  Returns `None` if the file doesn't exist (no
/// stub is created — a missing SYSTEM.md means no per-machine conventions
/// are recorded).
fn read_system_md() -> Option<String> {
    let path = crate::config::system_md_path();
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let trimmed = content.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

fn read_or_create_md(path: &Path) -> String {
    if !path.exists() {
        let parent = path.parent().expect("md path always has a parent");
        let _ = std::fs::create_dir_all(parent);
        let _ = std::fs::write(path, "");
    }
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let trimmed = content.trim();
    if trimmed.is_empty() {
        String::from("[empty, no user memories yet.]")
    } else {
        trimmed.to_string()
    }
}

fn render_preamble(
    cwd: &str,
    home_dir: &Path,
    user_md: &str,
    system_md: Option<&str>,
    agents_md: Option<&str>,
) -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| String::from("unknown"));
    let shell = std::env::var("SHELL").unwrap_or_else(|_| String::from("/bin/sh"));

    let mut context = tera::Context::new();
    context.insert("user", &user);
    context.insert("home", &home_dir.display().to_string());
    context.insert("shell", &shell);
    context.insert("os_family", std::env::consts::OS);
    context.insert("arch", std::env::consts::ARCH);
    context.insert("os_distro", &os_release());
    context.insert("cwd", cwd);
    context.insert("user_md_path", &crate::config::user_md_path().display().to_string());
    context.insert("user_md", user_md);
    if let Some(smd) = system_md {
        context.insert("system_md_path", &crate::config::system_md_path().display().to_string());
        context.insert("system_md", smd);
    }
    if let Some(amd) = agents_md {
        context.insert("agents_md", amd);
    }

    let template = include_str!("preamble.md");
    tera::Tera::one_off(template, &context, false).expect("failed to render preamble template")
}

// ── OS detection ──────────────────────────────────────────────────

/// Returns a human-readable OS/distro string, e.g. "NixOS 24.11" or
/// "macOS 15.2".  Falls back to `uname -sr` output.
fn os_release() -> String {
    // Linux: try /etc/os-release first
    if let Ok(contents) = std::fs::read_to_string("/etc/os-release") {
        let mut name = None;
        let mut version = None;
        for line in contents.lines() {
            if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
                // PRETTY_NAME="NixOS 24.11 (Vicuna)"  →  "NixOS 24.11"
                let v = v.trim_matches('"');
                if let Some(paren) = v.rfind(" (") {
                    return v[..paren].to_string();
                }
                return v.to_string();
            }
            if let Some(v) = line.strip_prefix("NAME=") {
                name = Some(v.trim_matches('"').to_string());
            }
            if let Some(v) = line.strip_prefix("VERSION=") {
                version = Some(v.trim_matches('"').to_string());
            }
        }
        if let Some(n) = name {
            if let Some(v) = version {
                return format!("{n} {v}");
            }
            return n;
        }
    }

    // macOS / BSD / fallback: use uname
    if let Ok(output) = std::process::Command::new("uname")
        .args(["-s", "-r"])
        .output()
        && output.status.success()
    {
        return String::from_utf8_lossy(&output.stdout).trim().to_string();
    }

    // Last resort
    std::env::consts::OS.to_string()
}
