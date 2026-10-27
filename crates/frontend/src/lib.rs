mod preview;
mod search;
mod shell;
mod uploads;
mod viewer;
mod workers;

use std::cell::RefCell;

use leptos::prelude::*;
use shell::{Account, LogLine, Shell};
use terminal_worker_core::{Entry, Listing};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = terminal)]
    fn config() -> JsValue;
    #[wasm_bindgen(js_namespace = terminal, js_name = open)]
    pub(crate) fn open_file(path: &str, kind: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_namespace = terminal)]
    fn unlock(path: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_namespace = terminal, js_name = accountCommand)]
    fn account_command(args: JsValue, cwd: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_namespace = terminal, js_name = chooseFiles)]
    fn choose_files() -> js_sys::Promise;
    #[wasm_bindgen(js_namespace = terminal, js_name = uploadFiles)]
    fn upload_files(files: &JsValue, path: &str, progress: &js_sys::Function) -> js_sys::Promise;
    #[wasm_bindgen(js_namespace = terminal, js_name = cancelUploads)]
    fn cancel_uploads() -> bool;
    #[wasm_bindgen(js_namespace = terminal, js_name = setDirectory)]
    fn set_directory(path: &str);
    #[wasm_bindgen(js_namespace = terminal, js_name = focusInput)]
    fn focus_input();
    #[wasm_bindgen(js_namespace = terminal, js_name = completionFocus)]
    fn completion_focus(value: &str, inside_quote: bool);
}

async fn request(url: &str, method: &str) -> Result<serde_json::Value, String> {
    let response = match method {
        "POST" => gloo_net::http::Request::post(url).send().await,
        "DELETE" => gloo_net::http::Request::delete(url).send().await,
        _ => gloo_net::http::Request::get(url).send().await,
    }
    .map_err(|e| e.to_string())?;
    let value: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if response.ok() {
        Ok(value)
    } else {
        Err(value["error"].as_str().unwrap_or("Request failed").into())
    }
}
fn endpoint(api: &str, path: &str) -> String {
    format!("/api/{api}?path={}", js_sys::encode_uri_component(path))
}
async fn listing(path: &str) -> Result<Vec<Entry>, String> {
    let value = request(&endpoint("ls", path), "GET").await?;
    serde_json::from_value::<Listing>(value)
        .map(|l| l.entries)
        .map_err(|e| e.to_string())
}
async fn authorize(path: &str) -> Result<(), String> {
    let value = JsFuture::from(unlock(path)).await.map_err(|error| {
        js_sys::Reflect::get(&error, &JsValue::from_str("message"))
            .ok()
            .and_then(|value| value.as_string())
            .unwrap_or_else(|| "Authorization failed".into())
    })?;
    if value.as_bool() == Some(true) {
        Ok(())
    } else {
        Err("Write canceled".into())
    }
}

fn file_icon(kind: &str, directory: bool) -> impl IntoView + use<> {
    let (label, path) = if directory {
        ("directory", "M3 7V5h6l2 2h10v13H3Z")
    } else {
        match kind {
            "image" => ("Image", "M3 3h18v18H3ZM3 17l5-6 4 5 3-3 6 6M15 7h.01"),
            "video" => ("Video", "M3 5h14v14H3ZM17 9l4-3v12l-4-3M8 9l4 3-4 3Z"),
            "audio" => (
                "Audio",
                "M9 18V5l11-2v13M9 8l11-2M9 18a3 3 0 1 1-3-3h3M20 16a3 3 0 1 1-3-3h3",
            ),
            "game" => (
                "Game",
                "M6 7h12l3 11-3 1-4-4h-4l-4 4-3-1ZM6 11h6M9 8v6M16 10h.01M18 13h.01",
            ),
            "slides" => ("Slides", "M3 4h18v13H3ZM12 17v4M7 21h10M7 9h10M7 12h6"),
            "markdown" => (
                "Markdown",
                "M3 4h18v16H3ZM6 15V9l3 3 3-3v6M17 9v6l-2-2M17 15l2-2",
            ),
            "pdf" => ("PDF", "M5 3h10l4 4v14H5ZM14 3v5h5M8 12h8M8 16h6"),
            "archive" => (
                "Archive",
                "M4 4h16v4H4ZM5 8v12h14V8M10 12h4M12 3v1M12 9v2M12 13v2",
            ),
            _ => ("Text file", "M5 3h10l4 4v14H5ZM14 3v5h5M8 12h8M8 16h6"),
        }
    };
    view! { <svg class="file-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" role="img" aria-label=label><path d=path/></svg> }
}
/// How much of a file still uploading has arrived, as shown after its name.
fn upload_progress(entry: &Entry) -> Option<String> {
    let uploaded = entry.uploaded?;
    Some(format!(
        "↑ {}%",
        if entry.size == 0 {
            100
        } else {
            uploaded * 100 / entry.size
        }
    ))
}
fn render_line(line: LogLine) -> AnyView {
    match line {
        LogLine::Text(text) => view! { <pre class="output-line">{text}</pre> }.into_any(),
        LogLine::Live(text) => view! { <pre class="output-line">{move || text.get()}</pre> }.into_any(),
        LogLine::Command { user, cwd, command } => {
            let symbol = if user == "root" { " # " } else { " $ " };
            view! { <pre class="command-echo"><span class="prompt-user">{user}</span><span class="prompt-separator">":"</span><span class="prompt-path">{cwd}</span><span class="prompt-symbol">{symbol}</span><span class="prompt-command">{command}</span></pre> }.into_any()
        }
        LogLine::Listing { entries, long } => {
            view! { <pre class=if long { "listing-output listing-long" } else { "listing-output" }>
                {entries.into_iter().map(|entry| {
                    let class = if entry.directory { "listing-entry entry-directory" } else if entry.mode & 0o111 != 0 { "listing-entry entry-executable" } else { "listing-entry entry-file" };
                    let meta = if long { format!("{} {}:{} {:>6} ", terminal_worker_core::permissions::mode_text(entry.mode, entry.directory), entry.owner, entry.group, entry.size) } else { String::new() };
                    let uploading = upload_progress(&entry).map(|p| format!(" {p}"));
                    view! { <span class=class>{file_icon(&entry.kind, entry.directory)}<span class="ls-meta">{meta}</span><span class="ls-name">{entry.name}{if entry.directory { "/" } else if entry.mode & 0o111 != 0 { "*" } else { "" }}</span><span class="ls-uploading">{uploading}</span></span> }
                }).collect_view()}
            </pre> }.into_any()
        }
    }
}

thread_local! {
    /// The tabs and the active one's id, for saving them as they change.
    static TABS: RefCell<Option<(RwSignal<Vec<Shell>>, RwSignal<u64>)>> = const { RefCell::new(None) };
}
/// Where the tabs are kept for this browser tab, across reloads.
const SAVED: &str = "terminal-tabs";
fn session_storage() -> Option<web_sys::Storage> {
    // Storage can be missing or refused, as in private windows; tabs then are not kept.
    web_sys::window()?.session_storage().ok().flatten()
}
/// Keep every tab's directory and history, and which tab is active.
pub fn persist() {
    let Some((tabs, active)) = TABS.with(|t| *t.borrow()) else {
        return;
    };
    let saved = tabs.with_untracked(|tabs| {
        serde_json::json!({
            "active": tabs.iter().position(|s| s.id == active.get_untracked()).unwrap_or(0),
            "tabs": tabs.iter().map(|s| serde_json::json!({
                "cwd": s.cwd.get_untracked(),
                "history": s.history.with_untracked(|h| h[h.len().saturating_sub(100)..].to_vec()),
            })).collect::<Vec<_>>(),
        })
    });
    if let Some(storage) = session_storage() {
        let _ = storage.set_item(SAVED, &saved.to_string());
    }
}
/// The tabs saved, as (directory, history), and the index of the active one.
fn restore() -> (Vec<(String, Vec<String>)>, usize) {
    let saved: serde_json::Value = session_storage()
        .and_then(|s| s.get_item(SAVED).ok().flatten())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let tabs: Vec<(String, Vec<String>)> = saved["tabs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tab| {
            Some((
                tab["cwd"].as_str()?.to_string(),
                serde_json::from_value(tab["history"].clone()).unwrap_or_default(),
            ))
        })
        .take(9)
        .collect();
    if tabs.is_empty() {
        return (vec![("/".into(), Vec::new())], 0);
    }
    let active = saved["active"].as_u64().unwrap_or(0) as usize;
    let active = active.min(tabs.len() - 1);
    (tabs, active)
}

/// One tab's transcript and command line, shown only while its tab is active.
fn terminal_view(shell: Shell, active: RwSignal<u64>) -> impl IntoView {
    let id = shell.id;
    let current = move || active.get() == id;
    // iOS corrects what is typed unless told not to, with an attribute Leptos does not name.
    let command_input = NodeRef::<leptos::html::Input>::new();
    Effect::new(move || {
        if let Some(input) = command_input.get() {
            let _ = input.set_attribute("autocorrect", "off");
        }
    });
    view! {
        <section class="terminal-panel" aria-label="Terminal" hidden=move || !current()>
        // A tap on the transcript, as on a phone, goes to the command line, unless it selects text.
        <div class="transcript" role="log" aria-live="polite" on:click=move |_| {
            let selecting = web_sys::window().and_then(|w| w.get_selection().ok().flatten()).is_some_and(|s| !s.is_collapsed());
            if !selecting { focus_input(); }
        }>{move || shell.lines.get().into_iter().map(render_line).collect_view()}</div>
        <Show when=move || shell.completion.get().is_some() || !shell.completion_status.get().is_empty()>
            <div class="completion-pager">
                <p class="completion-hint" role="status">{move || shell.completion.get().map(|s| format!("{} / {} · Tab next · Shift+Tab previous · → accept · Esc cancel", s.selected+1, s.candidates.len())).unwrap_or_else(|| shell.completion_status.get())}</p>
                <div id=move || current().then_some("completions") class="completion-options" role="listbox" aria-label="Completions">
                    {move || shell.completion.get().map(|s| s.candidates.into_iter().enumerate().map(|(index, candidate)| {
                        view! { <button type="button" role="option" tabindex="-1" id=format!("completion-{id}-{index}") aria-selected=move || shell.completion.with(|s| s.as_ref().is_some_and(|s| s.selected == index)).to_string()
                            on:mousedown=move |e| e.prevent_default()
                            on:click=move |_| { shell.select_completion(index); shell.accept_completion(); }>
                            <span>{candidate.label}</span><small>{candidate.kind}</small>
                        </button> }
                    }).collect_view())}
                </div>
            </div>
        </Show>
        <form class="command-line" on:submit=move |event| { event.prevent_default(); shell.run(shell.input.get_untracked()); }>
            <label for=move || current().then_some("command")><span class="prompt-user">{move || shell.account.username.get()}</span><span class="prompt-separator">":"</span><span class="prompt-path">{move || shell.cwd.get()}</span><span class="prompt-symbol">{move || if shell.asking.get() { " ?" } else if shell.account.username.get() == "root" { " #" } else { " $" }}</span></label>
            <input id=move || current().then_some("command") class="command-input" aria-label="Terminal command" autocomplete="off" spellcheck="false" autocapitalize="off" enterkeyhint="go" prop:value=move || shell.input.get()
                role="combobox" aria-autocomplete="list" aria-controls=move || current().then_some("completions")
                aria-expanded=move || shell.completion.get().is_some().to_string()
                aria-activedescendant=move || shell.completion.get().map(|s| format!("completion-{id}-{}", s.selected))
                on:input=move |e| { shell.reset_completion(); shell.input.set(event_target_value(&e)); }
                on:keydown=move |e| {
                    if e.is_composing() { return; }
                    if e.ctrl_key() && !e.alt_key() && !e.meta_key() && !e.shift_key() && e.key().eq_ignore_ascii_case("c") {
                        e.prevent_default();
                        shell.interrupt();
                        return;
                    }
                    match e.key().as_str() {
                        "Tab" => { e.prevent_default(); shell.complete(e.shift_key()); },
                        "Escape" => { e.prevent_default(); shell.cancel_completion(); },
                        "ArrowRight" if shell.completion.get_untracked().is_some() => { e.prevent_default(); shell.accept_completion(); },
                        "ArrowUp" | "ArrowDown" if shell.completion.get_untracked().is_some() => { e.prevent_default(); shell.complete(e.key() == "ArrowUp"); },
                        "ArrowUp" | "ArrowDown" => { e.prevent_default(); shell.recall(e.key() == "ArrowUp"); },
                        _ => {},
                    }
                }
                node_ref=command_input/>
            <span class="busy">{move || if shell.asking.get() { "Answer ↵" } else if shell.busy.get() { "Working..." } else { "↵" }}</span>
        </form>
        // The keys a phone's keyboard lacks. Pressing one keeps the focus, and the keyboard, where
        // they are.
        <div class="touch-keys" aria-label="Terminal keys" on:pointerdown=move |e| e.prevent_default()>
            <button type="button" aria-label="Tab: complete" on:click=move |_| { focus_input(); shell.complete(false); }>"Tab"</button>
            <button type="button" aria-label="Previous command" on:click=move |_| { focus_input(); if shell.completion.get_untracked().is_some() { shell.complete(true) } else { shell.recall(true) } }>"↑"</button>
            <button type="button" aria-label="Next command" on:click=move |_| { focus_input(); if shell.completion.get_untracked().is_some() { shell.complete(false) } else { shell.recall(false) } }>"↓"</button>
            <button type="button" aria-label="Ctrl+C: stop" class="touch-stop" on:click=move |_| { focus_input(); shell.interrupt(); }>"^C"</button>
        </div>
        </section>
    }
}

#[component]
fn App() -> impl IntoView {
    let branding: serde_json::Value =
        serde_wasm_bindgen::from_value(config()).expect("terminal branding");
    let title = branding["name"]
        .as_str()
        .unwrap_or("R2 TERMINAL")
        .to_owned();
    let subtitle = branding["subtitle"].as_str().unwrap_or("").to_owned();
    let heading = branding["heading"].as_str().unwrap_or("").to_owned();
    let description = branding["description"].as_str().unwrap_or("").to_owned();
    let layout = RwSignal::new(None::<bool>);
    let files_visible = RwSignal::new(true);
    let terminal_visible = RwSignal::new(true);
    // Either pane may be hidden, never both: hiding one shows the other.
    let toggle = move |this: RwSignal<bool>, other: RwSignal<bool>| {
        if this.get_untracked() {
            this.set(false);
            other.set(true);
        } else {
            this.set(true);
        }
    };
    let account = Account {
        writable: RwSignal::new(false),
        username: RwSignal::new("guest".into()),
        home: RwSignal::new("/".into()),
    };
    // Tabs opened later are owned by the app, as the first ones are.
    let owner = StoredValue::new(Owner::current().expect("the app's owner"));
    let next_id = StoredValue::new(0u64);
    let make = move |cwd: String, history: Vec<String>| {
        let id = next_id.get_value();
        next_id.set_value(id + 1);
        owner
            .get_value()
            .with(|| Shell::new(id, account, cwd, history))
    };
    let (saved, saved_active) = restore();
    let first: Vec<Shell> = saved
        .into_iter()
        .map(|(cwd, history)| make(cwd, history))
        .collect();
    first[0].log("Connected as guest. Access follows account and rwx permissions.\nEnter help or open a file.");
    let active = RwSignal::new(first[saved_active].id);
    let tabs = RwSignal::new(first);
    TABS.with(|t| *t.borrow_mut() = Some((tabs, active)));
    let current = Signal::derive(move || {
        tabs.with(|t| {
            t.iter()
                .find(|s| s.id == active.get())
                .copied()
                .unwrap_or(t[0])
        })
    });
    let switch = move |id: u64| {
        active.set(id);
        let shell = current.get_untracked();
        set_directory(&shell.cwd.get_untracked());
        spawn_local(async move {
            let _ = shell.refresh().await;
        });
        persist();
        focus_input();
    };
    let new_tab = move || {
        if tabs.with_untracked(Vec::len) >= 9 {
            return;
        }
        let from = current.get_untracked();
        let shell = make(from.cwd.get_untracked(), from.history.get_untracked());
        tabs.update(|t| t.push(shell));
        switch(shell.id);
    };
    let close_tab = move |id: u64| {
        if tabs.with_untracked(Vec::len) == 1 {
            return;
        }
        let index = tabs.with_untracked(|t| t.iter().position(|s| s.id == id));
        let Some(index) = index else { return };
        // A closed tab stops what it runs in the page, at its next step; jobs go on.
        tabs.with_untracked(|t| t[index].detach());
        tabs.update(|t| {
            t.remove(index);
        });
        if active.get_untracked() == id {
            let next = tabs.with_untracked(|t| t[index.min(t.len() - 1)].id);
            switch(next);
        } else {
            persist();
        }
    };
    set_directory(&current.get_untracked().cwd.get_untracked());
    for shell in tabs.get_untracked() {
        spawn_local(async move {
            if let Err(e) = shell.refresh().await {
                shell.log(e);
            }
        });
    }
    let listener = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        let before = account.username.get_untracked();
        spawn_local(async move {
            for shell in tabs.get_untracked() {
                if let Err(e) = shell.refresh().await {
                    shell.log(e);
                }
            }
            // The account is the browser session's, so every tab changes with it.
            let after = account.username.get_untracked();
            if after != before {
                for shell in tabs.get_untracked() {
                    if shell.id != active.get_untracked() {
                        shell.log(format!("Account changed to {after} in another tab"));
                    }
                }
            }
        });
    });
    let window = web_sys::window().unwrap();
    window
        .add_event_listener_with_callback("terminal:changed", listener.as_ref().unchecked_ref())
        .unwrap();
    listener.forget();
    // Directories others changed, which tabs showing them list again.
    let remote = Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |e: web_sys::CustomEvent| {
        let dirs: Vec<String> = serde_wasm_bindgen::from_value(e.detail()).unwrap_or_default();
        for shell in tabs.get_untracked() {
            if dirs.contains(&shell.cwd.get_untracked()) {
                spawn_local(async move {
                    let _ = shell.refresh().await;
                });
            }
        }
    });
    window
        .add_event_listener_with_callback("terminal-remote", remote.as_ref().unchecked_ref())
        .unwrap();
    remote.forget();
    // Files dropped on the page upload in the active tab.
    let dropped =
        Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |e: web_sys::CustomEvent| {
            current.get_untracked().upload_dropped(e.detail());
        });
    window
        .add_event_listener_with_callback("terminal-files", dropped.as_ref().unchecked_ref())
        .unwrap();
    dropped.forget();
    // Alt+T opens a tab, Alt+W closes it, Alt+1…9 switch; browsers keep Ctrl+T and Ctrl+W.
    let keys =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            if !e.alt_key() || e.ctrl_key() || e.meta_key() || e.is_composing() {
                return;
            }
            let dialog = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.query_selector("dialog[open]").ok().flatten());
            if dialog.is_some() {
                return;
            }
            match e.code().as_str() {
                "KeyT" => new_tab(),
                "KeyW" => close_tab(active.get_untracked()),
                code => {
                    let Some(n) = code
                        .strip_prefix("Digit")
                        .and_then(|d| d.parse::<usize>().ok())
                        .filter(|n| (1..=9).contains(n))
                    else {
                        return;
                    };
                    let Some(id) = tabs.with_untracked(|t| t.get(n - 1).map(|s| s.id)) else {
                        return;
                    };
                    switch(id);
                }
            }
            e.prevent_default();
        });
    window
        .add_event_listener_with_callback("keydown", keys.as_ref().unchecked_ref())
        .unwrap();
    keys.forget();
    let numbered = move || tabs.get().into_iter().enumerate().collect::<Vec<_>>();
    view! {
        <main class="terminal-shell">
            <header><div class="wordmark"><span class="signal"></span>{title}<span class="subtitle">{subtitle}</span></div>
                <button class="lock" on:click=move |_| current.get_untracked().run(if account.writable.get_untracked() { "logout".into() } else { "login".into() })>
                    // Kept to one line: a long name shortens, the action stays.
                    <span class="lock-name">{move || if account.writable.get() { format!("● {}", account.username.get()) } else { "○ guest".into() }}</span>
                    <span class="lock-action">{move || if account.writable.get() { " · Sign out" } else { " · Sign in" }}</span>
                </button>
            </header>
            <section class="intro"><span class="eyebrow">"PUBLIC ARCHIVE / 001"</span><h1>{heading}</h1><p>{description}</p></section>
            <section class="workspace" aria-label="File terminal"
                data-files=move || if files_visible.get() { "visible" } else { "hidden" }
                data-terminal=move || if terminal_visible.get() { "visible" } else { "hidden" }
                data-layout=move || match layout.get() { Some(true) => "columns", Some(false) => "rows", None => "auto" }>
                <div class="workspace-toolbar"><div class="pane-toggles">
                    <button class="files-toggle" aria-label="Toggle file panel" aria-expanded=move || files_visible.get().to_string() on:click=move |_| toggle(files_visible, terminal_visible)>{move || if files_visible.get() { "◂ Hide files" } else { "▸ Show files" }}</button>
                    <button class="terminal-toggle" aria-label="Toggle terminal" aria-expanded=move || terminal_visible.get().to_string() on:click=move |_| toggle(terminal_visible, files_visible)>{move || if terminal_visible.get() { "◂ Hide terminal" } else { "▸ Show terminal" }}</button>
                </div>
                    <button class="layout-toggle" aria-label="Toggle workspace layout" title="Toggle workspace layout"
                        on:click=move |_| {
                            let horizontal = layout.get_untracked().unwrap_or_else(|| {
                                web_sys::window().and_then(|w| w.inner_width().ok()).and_then(|v| v.as_f64()).unwrap_or(0.0) >= 900.0
                            });
                            layout.set(Some(!horizontal));
                        }>
                        <span class="layout-to-rows">"▤ Stacked layout"</span>
                        <span class="layout-to-columns">"◫ Side-by-side layout"</span>
                    </button>
                </div>
                <div class="workspace-panes">
                <section class="file-panel" aria-label="File manager">
                <div class="directory-bar"><span>{move || current.get().cwd.get()}</span>
                    <button on:click=move |_| current.get_untracked().run("upload".into())>"↑ Upload files"</button></div>
                <div class="files" aria-label="Current directory">
                    <Show when=move || current.get().cwd.get() != "/"><button class="file" on:click=move |_| current.get_untracked().run("cd ..".into())><span class="file-kind">{file_icon("directory", true)}</span><span>"../"</span></button></Show>
                    {move || current.get().entries.get().into_iter().map(|entry| {
                        let command = format!("{} \"{}\"", if entry.directory { "cd" } else { "open" }, entry.path.replace('"', "\\\""));
                        let size = if entry.directory { String::new() } else { upload_progress(&entry).unwrap_or_else(|| format!("{:.1} KB", entry.size as f64 / 1000.0)) };
                        view! { <button class="file" data-kind=if entry.directory { "directory".to_string() } else { entry.kind.clone() } title=format!("{} {}:{}", terminal_worker_core::permissions::mode_text(entry.mode, entry.directory), entry.owner, entry.group) on:click=move |_| current.get_untracked().run(command.clone())><span class="file-kind">{file_icon(&entry.kind, entry.directory)}</span><span class="filename"><span class="filename-text">{entry.name}{if entry.directory { "/" } else { "" }}</span></span><span class="file-size">{size}</span></button> }
                    }).collect_view()}
                    <p class="directory-error" role="status">{move || current.get().directory_error.get()}</p>
                    <Show when=move || current.get().entries.get().is_empty() && current.get().directory_error.get().is_empty()><p class="empty">"This directory is empty. Drop files here or use mkdir to create a directory."</p></Show>
                </div>
                </section>
                <div class="terminal-tabs">
                    <div class="tab-bar" role="tablist" aria-label="Terminal tabs">
                        <For each=numbered key=|(index, shell)| (*index, shell.id) let:item>
                            {
                                let (index, shell) = item;
                                let id = shell.id;
                                view! {
                                    <span class="tab" role="tab" aria-selected=move || (active.get() == id).to_string()>
                                        <button type="button" class="tab-select" title=move || format!("Alt+{} · {}", index + 1, shell.cwd.get()) on:click=move |_| switch(id)>
                                            {move || format!("{} {}{}", index + 1, shell.cwd.get().rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("/"), if shell.busy.get() { " ●" } else { "" })}
                                        </button>
                                        <Show when=move || { tabs.with(|t| t.len() > 1) }>
                                            <button type="button" class="tab-close" aria-label=format!("Close tab {}", index + 1) on:click=move |_| close_tab(id)>"×"</button>
                                        </Show>
                                    </span>
                                }
                            }
                        </For>
                        <button type="button" class="tab-new" aria-label="New tab" title="New tab · Alt+T" on:click=move |_| new_tab()>"+"</button>
                    </div>
                    <For each=move || tabs.get() key=|shell| shell.id let:shell>
                        {terminal_view(shell, active)}
                    </For>
                </div>
                </div>
            </section>
            <footer><span>"TAB / SHIFT+TAB complete   ↑↓ history   | pipes   ALT+T new tab   Drop files to upload"</span><span>"ACCESS: USER / GROUP / OTHER"</span></footer>
        </main>
    }
}
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    viewer::register();
    uploads::register();
    let root = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id("app")
        .unwrap();
    root.set_text_content(None);
    leptos::mount::mount_to(root.unchecked_into(), App).forget();
    focus_input();
}
