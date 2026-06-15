//! Agent preamble: builds the system prompt from the Tera template,
//! environment context, and user/project memory files (USER.md, AGENTS.md).
//!
//! Order is deliberate to maximise prompt-cache prefix re-use:
//!   1. Static guidelines (never changes)
//!   2. User + OS info (changes only with system upgrades)
//!   3. USER.md (persistent user memory; changes rarely)
//!   4. CWD (changes per session / cd)
//!   5. AGENTS.md (project context; may be edited mid-session)

/// Render the agent preamble from the Tera template and env context.
pub fn build_preamble() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| String::from("unknown"));
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("~"));
    let shell = std::env::var("SHELL").unwrap_or_else(|_| String::from("/bin/sh"));
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| String::from("(unknown)"));

    // USER.md
    let user_md = if let Ok(home_dir) = std::env::var("HOME") {
        let user_md_path = std::path::PathBuf::from(&home_dir)
            .join(".config")
            .join("goop")
            .join("USER.md");
        if !user_md_path.exists() {
            let _ = std::fs::create_dir_all(user_md_path.parent().unwrap());
            let _ = std::fs::write(&user_md_path, "");
        }
        let content = std::fs::read_to_string(&user_md_path).unwrap_or_default();
        let trimmed = content.trim();
        if trimmed.is_empty() {
            String::from("[empty, no user memories yet.]")
        } else {
            trimmed.to_string()
        }
    } else {
        String::from("[empty, no user memories yet.]")
    };

    // AGENTS.md (may be present or absent — template handles the conditional)
    let agents_md = std::fs::read_to_string("AGENTS.md").ok();

    let mut context = tera::Context::new();
    context.insert("user", &user);
    context.insert("home", &home);
    context.insert("shell", &shell);
    context.insert("os_family", std::env::consts::OS);
    context.insert("arch", std::env::consts::ARCH);
    context.insert("os_distro", &os_release());
    context.insert("cwd", &cwd);
    context.insert("user_md", &user_md);
    if let Some(ref amd) = agents_md {
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
