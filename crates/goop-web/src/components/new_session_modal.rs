use leptos::prelude::*;
use serde::Deserialize;

/// A modal dialog for creating a new session.
///
/// The CWD field accepts a path; a "Browse" button opens an inline directory
/// browser backed by `GET /api/browse-dir`.  Navigating the browser
/// updates the CWD field in real time.  Leave the name blank for an
/// auto-generated `YYYYMMDD_NNN` name.
#[component]
pub fn NewSessionModal(
    #[prop(into)] on_create: Callback<(Option<String>, Option<String>)>,
    #[prop(into)] on_close: Callback<()>,
) -> impl IntoView {
    let name = RwSignal::new(String::new());
    let cwd = RwSignal::new(String::new());
    let browse_open = RwSignal::new(false);
    let browse_path = RwSignal::new(String::new());
    let browse_entries = RwSignal::new(Vec::<BrowseEntry>::new());
    let browse_parent = RwSignal::new(None::<String>);
    let browse_error = RwSignal::new(None::<String>);

    let name_ref = NodeRef::<leptos::html::Input>::new();

    // Focus the name input on mount.
    {
        let name_ref = name_ref.clone();
        Effect::new(move || {
            if let Some(input) = name_ref.get_untracked() {
                let _ = input.focus();
            }
        });
    }

    let fetch_dir = {
        let browse_path = browse_path.clone();
        let browse_entries = browse_entries.clone();
        let browse_parent = browse_parent.clone();
        let browse_error = browse_error.clone();
        let cwd = cwd.clone();
        move |path: String| {
            let browse_path = browse_path.clone();
            let browse_entries = browse_entries.clone();
            let browse_parent = browse_parent.clone();
            let browse_error = browse_error.clone();
            let cwd = cwd.clone();
            leptos::task::spawn_local(async move {
                let url = format!("/api/browse-dir?path={}", url_escape(&path));
                match gloo_net::http::Request::get(&url).send().await {
                    Ok(r) => match r.json::<BrowseDirResponse>().await {
                        Ok(resp) => {
                            let resolved = resp.path.clone();
                            browse_path.set(resolved.clone());
                            browse_parent.set(resp.parent);
                            browse_entries.set(resp.entries);
                            browse_error.set(None);
                            // Update the CWD field in real time as the user navigates.
                            cwd.set(resolved);
                        }
                        Err(e) => {
                            browse_error.set(Some(format!("{e}")));
                        }
                    },
                    Err(e) => {
                        browse_error.set(Some(format!("{e}")));
                    }
                }
            });
        }
    };

    let open_browse = {
        let cwd = cwd.clone();
        let browse_open = browse_open.clone();
        let fetch_dir = fetch_dir.clone();
        move |_| {
            let start = cwd.get_untracked().trim().to_string();
            let start = if start.is_empty() { "~".into() } else { start };
            fetch_dir(start);
            browse_open.set(true);
        }
    };

    let browse_click = {
        let fetch_dir = fetch_dir.clone();
        move |entry_name: String, is_dir: bool| {
            if is_dir {
                let next = format!(
                    "{}/{}",
                    browse_path.get_untracked().trim_end_matches('/'),
                    entry_name
                );
                fetch_dir(next);
            }
        }
    };

    let browse_up = {
        let fetch_dir = fetch_dir.clone();
        move |_| {
            if let Some(ref parent) = browse_parent.get_untracked() {
                fetch_dir(parent.clone());
            }
        }
    };

    let reset_home = {
        let cwd = cwd.clone();
        let fetch_dir = fetch_dir.clone();
        move |_| {
            cwd.set("~".into());
            fetch_dir("~".into());
        }
    };

    let reset_default = {
        let fetch_dir = fetch_dir.clone();
        let browse_open = browse_open.clone();
        move |_| {
            fetch_dir(".".into());
            browse_open.set(true);
        }
    };

    let create = {
        let name = name.clone();
        let cwd = cwd.clone();
        let on_create = on_create.clone();
        move |_: leptos::ev::MouseEvent| {
            let name_val = name.get_untracked().trim().to_string();
            let name_opt = if name_val.is_empty() {
                None
            } else {
                Some(name_val)
            };
            let cwd_val = cwd.get_untracked().trim().to_string();
            let cwd_opt = if cwd_val.is_empty() { None } else { Some(cwd_val) };
            on_create.run((name_opt, cwd_opt));
        }
    };

    let close = move |_: leptos::ev::MouseEvent| {
        on_close.run(());
    };

    // Keyboard: Enter in name → create; Escape → close.
    let on_keydown = {
        let on_create = on_create.clone();
        let on_close = on_close.clone();
        let name = name.clone();
        let cwd = cwd.clone();
        move |evt: leptos::ev::KeyboardEvent| {
            match evt.key().as_str() {
                "Enter" => {
                    evt.prevent_default();
                    let name_val = name.get_untracked().trim().to_string();
                    let name_opt = if name_val.is_empty() {
                        None
                    } else {
                        Some(name_val)
                    };
                    let cwd_val = cwd.get_untracked().trim().to_string();
                    let cwd_opt = if cwd_val.is_empty() { None } else { Some(cwd_val) };
                    on_create.run((name_opt, cwd_opt));
                }
                "Escape" => {
                    evt.prevent_default();
                    on_close.run(());
                }
                _ => {}
            }
        }
    };

    view! {
        <div class="modal-backdrop" on:click=close>
            <div class="modal-dialog" on:click=|evt| evt.stop_propagation() on:keydown=on_keydown>
                <div class="modal-header">
                    <span class="modal-title">"New session"</span>
                </div>
                <div class="modal-body">
                    <label class="modal-label" for="ns-name">
                        "Session name"
                        <span class="modal-hint">"(leave blank for auto-generated)"</span>
                    </label>
                    <input
                        type="text"
                        id="ns-name"
                        class="modal-input"
                        node_ref=name_ref
                        prop:value=name
                        on:input=move |evt| name.set(event_target_value(&evt))
                        placeholder="YYYYMMDD_NNN (auto-generated)"
                    />

                    <label class="modal-label" for="ns-cwd">
                        "Working directory"
                        <span class="modal-hint">"(empty = server CWD)"</span>
                    </label>
                    <div class="modal-cwd-row">
                        <input
                            type="text"
                            id="ns-cwd"
                            class="modal-input"
                            prop:value=cwd
                            on:input=move |evt| cwd.set(event_target_value(&evt))
                            placeholder="~"
                        />
                        <button class="modal-btn modal-btn-browse" on:click=open_browse>
                            "Browse"
                        </button>
                    </div>
                    <div class="modal-cwd-resets">
                        <button class="modal-btn-reset" on:click=reset_home>
                            "~ (home)"
                        </button>
                        <button class="modal-btn-reset" on:click=reset_default>
                            "server CWD"
                        </button>
                    </div>

                    // ── directory browser ──────────────────────────
                    <Show when=move || browse_open.get()>
                        <div class="browse-panel">
                            <div class="browse-top">
                                <span class="browse-dir-label">"📁 "</span>
                                <span class="browse-dir-path">{move || browse_path.get()}</span>
                            </div>
                            <Show when=move || browse_error.get().is_some()>
                                <div class="browse-error">
                                    {move || browse_error.get().unwrap_or_default()}
                                </div>
                            </Show>
                            <div class="browse-entries">
                                <Show when=move || browse_parent.get().is_some()>
                                    <div class="browse-entry browse-parent" on:click=browse_up>
                                        <span class="browse-entry-icon">"↩ "</span>
                                        <span class="browse-entry-name">".."</span>
                                    </div>
                                </Show>
                                {move || {
                                    browse_entries.get()
                                        .into_iter()
                                        .map(|e| {
                                            let name = e.name.clone();
                                            let icon = if e.is_dir { "📁 " } else { "📄 " };
                                            let cls = if e.is_dir { "browse-entry browse-dir" } else { "browse-entry browse-file" };
                                            let click = {
                                                let name = name.clone();
                                                let browse_click = browse_click.clone();
                                                let is_dir = e.is_dir;
                                                move |_| browse_click(name.clone(), is_dir)
                                            };
                                            view! {
                                                <div class=cls on:click=click>
                                                    <span class="browse-entry-icon">{icon}</span>
                                                    <span class="browse-entry-name">{name}</span>
                                                </div>
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                }}
                            </div>
                        </div>
                    </Show>
                </div>
                <div class="modal-footer">
                    <button class="modal-btn modal-btn-cancel" on:click=close>
                        "Cancel"
                    </button>
                    <button class="modal-btn modal-btn-create" on:click=create>
                        "Create"
                    </button>
                </div>
            </div>
        </div>
    }
}

// ── directory browser types ──────────────────────────────────────────

#[derive(Deserialize, Clone)]
struct BrowseDirResponse {
    path: String,
    parent: Option<String>,
    entries: Vec<BrowseEntry>,
}

#[derive(Deserialize, Clone)]
struct BrowseEntry {
    name: String,
    is_dir: bool,
}

/// Minimal URL-encoding for path characters.
fn url_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}
