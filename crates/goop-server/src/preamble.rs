//! Agent preamble: builds the system prompt from the Tera template,
//! environment context, and user/project memory files (USER.md, AGENTS.md).
//!
//! Order is deliberate to maximise prompt-cache prefix re-use:
//!   1. Static guidelines (never changes)
//!   2. User + OS info (changes only with system upgrades)
//!   3. USER.md (persistent user memory; changes rarely)
//!   4. CWD (changes per session / cd)
//!   5. AGENTS.md (project context; may be edited mid-session)

use std::path::Path;

/// Render the agent preamble from the Tera template and env context.
/// Reads AGENTS.md from `{cwd}/AGENTS.md` on the local filesystem.
pub fn build_preamble(cwd: &str, home_dir: &Path) -> String {
    let user_md = read_user_md(home_dir);
    let agents_md = {
        let agents_path = Path::new(cwd).join("AGENTS.md");
        std::fs::read_to_string(&agents_path).ok()
    };
    render_preamble(cwd, home_dir, &user_md, agents_md.as_deref())
}

/// Render the agent preamble with a pre-supplied AGENTS.md content.
///
/// `agents_md` is the content of the project's AGENTS.md (if any), obtained
/// via the active transport.  This variant is used when rebuilding the
/// preamble after `cd` / `ssh` / `disconnect`, where AGENTS.md may reside
/// on a remote host.
pub fn build_preamble_with(cwd: &str, home_dir: &Path, agents_md: Option<&str>) -> String {
    let user_md = read_user_md(home_dir);
    render_preamble(cwd, home_dir, &user_md, agents_md)
}

// ── helpers ────────────────────────────────────────────────────────

fn read_user_md(home_dir: &Path) -> String {
    let user_md_path = home_dir.join(".config").join("goop").join("USER.md");
    if !user_md_path.exists() {
        let parent = user_md_path
            .parent()
            .expect("user_md_path always has a parent");
        let _ = std::fs::create_dir_all(parent);
        let _ = std::fs::write(&user_md_path, "");
    }
    let content = std::fs::read_to_string(&user_md_path).unwrap_or_default();
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
    context.insert("user_md", user_md);
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
