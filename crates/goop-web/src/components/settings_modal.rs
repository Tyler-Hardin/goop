use goop_shared::{Setting, SettingsUpdate, ToolGroup};
use leptos::prelude::*;

use crate::state::AppState;

/// A modal dialog for changing session settings mid-conversation.
///
/// Each field can be set to a new value, cleared (revert to global default),
/// or left alone.  The client sends a [`SettingsUpdate`] over WebSocket;
/// the server merges the changes and broadcasts a `SettingsChanged` event.
#[component]
pub fn SettingsModal(
    #[prop(into)] on_close: Callback<()>,
) -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState missing");

    // Initialize from current session overrides.
    let model = RwSignal::new(
        state.current_settings.get_untracked().model.unwrap_or_default(),
    );
    let model_overridden =
        RwSignal::new(state.current_settings.get_untracked().model.is_some());

    let ollama_url = RwSignal::new(
        state.current_settings.get_untracked().ollama_base_url.unwrap_or_default(),
    );
    let ollama_url_overridden =
        RwSignal::new(state.current_settings.get_untracked().ollama_base_url.is_some());

    let current_tool_groups = state.current_settings.get_untracked()
        .enabled_tool_groups.clone();
    let file_ops = RwSignal::new(current_tool_groups.as_ref()
        .map(|g| g.contains(&ToolGroup::FileOps)).unwrap_or(true));
    let shell = RwSignal::new(current_tool_groups.as_ref()
        .map(|g| g.contains(&ToolGroup::Shell)).unwrap_or(true));
    let ssh = RwSignal::new(current_tool_groups.as_ref()
        .map(|g| g.contains(&ToolGroup::Ssh)).unwrap_or(true));
    let web_fetch = RwSignal::new(current_tool_groups.as_ref()
        .map(|g| g.contains(&ToolGroup::WebFetch)).unwrap_or(true));
    let computer_use = RwSignal::new(current_tool_groups.as_ref()
        .map(|g| g.contains(&ToolGroup::ComputerUse)).unwrap_or(false));
    let tool_groups_overridden =
        RwSignal::new(state.current_settings.get_untracked().enabled_tool_groups.is_some());

    let saved = RwSignal::new(false);

    let model_ref = NodeRef::<leptos::html::Input>::new();

    // Focus the model input on mount.
    {
        let model_ref = model_ref.clone();
        Effect::new(move || {
            if let Some(input) = model_ref.get_untracked() {
                let _ = input.focus();
            }
        });
    }

    let save = {
        let state = state.clone();
        let model = model.clone();
        let model_overridden = model_overridden.clone();
        let ollama_url = ollama_url.clone();
        let ollama_url_overridden = ollama_url_overridden.clone();
        let file_ops = file_ops.clone();
        let shell = shell.clone();
        let ssh = ssh.clone();
        let web_fetch = web_fetch.clone();
        let computer_use = computer_use.clone();
        let tool_groups_overridden = tool_groups_overridden.clone();
        let saved = saved.clone();
        let on_close = on_close.clone();
        move |_: leptos::ev::MouseEvent| {
            let mut update = SettingsUpdate::default();

            // Model.
            if model_overridden.get_untracked() {
                let v = model.get_untracked().trim().to_string();
                update.model = Some(if v.is_empty() {
                    Setting::Clear
                } else {
                    Setting::Set(v)
                });
            }

            // Ollama base URL.
            if ollama_url_overridden.get_untracked() {
                let v = ollama_url.get_untracked().trim().to_string();
                update.ollama_base_url = Some(if v.is_empty() {
                    Setting::Clear
                } else {
                    Setting::Set(v)
                });
            }

            // Tool groups.
            if tool_groups_overridden.get_untracked() {
                let mut groups = Vec::new();
                if file_ops.get_untracked() { groups.push(ToolGroup::FileOps); }
                if shell.get_untracked() { groups.push(ToolGroup::Shell); }
                if ssh.get_untracked() { groups.push(ToolGroup::Ssh); }
                if web_fetch.get_untracked() { groups.push(ToolGroup::WebFetch); }
                if computer_use.get_untracked() { groups.push(ToolGroup::ComputerUse); }
                update.enabled_tool_groups = Some(Setting::Set(groups));
            }

            if update.model.is_none()
                && update.ollama_base_url.is_none()
                && update.enabled_tool_groups.is_none()
            {
                on_close.run(());
                return;
            }

            state.update_settings(update);
            saved.set(true);
            on_close.run(());
        }
    };

    let close = {
        let on_close = on_close.clone();
        move |_: leptos::ev::MouseEvent| {
            on_close.run(());
        }
    };

    let on_keydown = {
        let save = save.clone();
        let close = close.clone();
        move |evt: leptos::ev::KeyboardEvent| {
            match evt.key().as_str() {
                "Enter" => {
                    evt.prevent_default();
                    save(leptos::ev::MouseEvent::new("click").unwrap());
                }
                "Escape" => {
                    evt.prevent_default();
                    close(leptos::ev::MouseEvent::new("click").unwrap());
                }
                _ => {}
            }
        }
    };

    view! {
        <div class="modal-backdrop" on:click=close>
            <div class="modal-dialog settings-dialog" on:click=|evt| evt.stop_propagation() on:keydown=on_keydown>
                <div class="modal-header">
                    <span class="modal-title">"Settings"</span>
                </div>
                <div class="modal-body">
                    // ── model ──────────────────────────────────────
                    <div class="settings-field">
                        <label class="settings-field-label" for="settings-model">
                            <input
                                type="checkbox"
                                class="settings-field-toggle"
                                prop:checked=move || model_overridden.get()
                                on:change=move |_| model_overridden.update(|v| *v = !*v)
                            />
                            "Model"
                        </label>
                        <Show when=move || model_overridden.get()>
                            <input
                                type="text"
                                id="settings-model"
                                class="modal-input"
                                node_ref=model_ref
                                prop:value=move || model.get()
                                on:input=move |evt| model.set(event_target_value(&evt))
                                placeholder="deepseek/deepseek-v4-pro"
                            />
                        </Show>
                        <Show when=move || !model_overridden.get()>
                            <span class="settings-inherited">"(inherited from global)"</span>
                        </Show>
                    </div>

                    // ── ollama base URL ────────────────────────────
                    <div class="settings-field">
                        <label class="settings-field-label" for="settings-ollama-url">
                            <input
                                type="checkbox"
                                class="settings-field-toggle"
                                prop:checked=move || ollama_url_overridden.get()
                                on:change=move |_| ollama_url_overridden.update(|v| *v = !*v)
                            />
                            "Ollama base URL"
                        </label>
                        <Show when=move || ollama_url_overridden.get()>
                            <input
                                type="text"
                                id="settings-ollama-url"
                                class="modal-input"
                                prop:value=move || ollama_url.get()
                                on:input=move |evt| ollama_url.set(event_target_value(&evt))
                                placeholder="http://localhost:11434"
                            />
                        </Show>
                        <Show when=move || !ollama_url_overridden.get()>
                            <span class="settings-inherited">"(inherited from global)"</span>
                        </Show>
                    </div>

                    // ── tool groups ────────────────────────────────
                    <div class="settings-field">
                        <label class="settings-field-label">
                            <input
                                type="checkbox"
                                class="settings-field-toggle"
                                prop:checked=move || tool_groups_overridden.get()
                                on:change=move |_| tool_groups_overridden.update(|v| *v = !*v)
                            />
                            "Tool groups"
                        </label>
                        <Show when=move || tool_groups_overridden.get()>
                            <div class="settings-checkboxes">
                                <label class="settings-checkbox">
                                    <input type="checkbox"
                                        prop:checked=move || file_ops.get()
                                        on:change=move |_| file_ops.update(|v| *v = !*v)
                                    />
                                    "file_ops"
                                </label>
                                <label class="settings-checkbox">
                                    <input type="checkbox"
                                        prop:checked=move || shell.get()
                                        on:change=move |_| shell.update(|v| *v = !*v)
                                    />
                                    "shell"
                                </label>
                                <label class="settings-checkbox">
                                    <input type="checkbox"
                                        prop:checked=move || ssh.get()
                                        on:change=move |_| ssh.update(|v| *v = !*v)
                                    />
                                    "ssh"
                                </label>
                                <label class="settings-checkbox">
                                    <input type="checkbox"
                                        prop:checked=move || web_fetch.get()
                                        on:change=move |_| web_fetch.update(|v| *v = !*v)
                                    />
                                    "web_fetch"
                                </label>
                                <label class="settings-checkbox">
                                    <input type="checkbox"
                                        prop:checked=move || computer_use.get()
                                        on:change=move |_| computer_use.update(|v| *v = !*v)
                                    />
                                    "computer_use"
                                </label>
                            </div>
                        </Show>
                        <Show when=move || !tool_groups_overridden.get()>
                            <span class="settings-inherited">"(inherited from global)"</span>
                        </Show>
                    </div>

                    <Show when=move || saved.get()>
                        <div class="settings-saved">"Settings updated ✓"</div>
                    </Show>
                </div>
                <div class="modal-footer">
                    <button class="modal-btn modal-btn-cancel" on:click=close>
                        "Cancel"
                    </button>
                    <button class="modal-btn modal-btn-create" on:click=save>
                        "Save"
                    </button>
                </div>
            </div>
        </div>
    }
}
