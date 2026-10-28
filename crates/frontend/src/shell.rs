//! One terminal tab: its directory, transcript, history and completion, and the commands it
//! runs. A line is a pipeline: each stage's output is the next one's input. The server runs
//! file and account commands; text filters, xargs and fd -x run here, as do browser
//! interactions such as open and upload.

mod commands;

use futures::channel::oneshot;
use futures::future::LocalBoxFuture;
use futures::{FutureExt, StreamExt};
use leptos::prelude::*;
use serde_json::{Value, json};
use terminal_worker_core::completion::{self, Candidate, Query};
use terminal_worker_core::glob::{self, Part};
use terminal_worker_core::shell::{self as grammar, Delimiter, Word};
use terminal_worker_core::{Entry, cli, find, resolve, split_command, text};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::{
    account_command, cancel_choosing, cancel_uploads, choose_files, completion_focus, endpoint,
    focus_input, listing, request, set_directory, stop_recording,
};

/// The most transcript lines kept, ...
const KEPT: usize = 100;
/// ... the most lines one output shows, ...
const SHOWN: usize = 2000;
/// ... the most bytes passed from one stage to the next, and read from one file.
const PIPED_MAX: usize = 10_000_000;
/// How many paths xargs passes to one command, at most.
const XARGS_MAX: usize = 1000;
/// How many commands one batch request runs, at most.
const BATCH: usize = 100;
/// How many filesystem lookups one wildcard pattern makes, and paths it expands to, at most.
const GLOB_LOOKUPS: usize = 1000;
const GLOB_PATHS: usize = 10_000;
/// How many of a wildcard's lookups run at once.
const GLOB_CONCURRENCY: usize = 8;

/// Server commands that xargs and fd -x may run many of in one request.
const BATCHED: &[&str] = &[
    "touch", "rm", "mv", "cp", "chmod", "chown", "chgrp", "mkdir", "ls", "whoami", "id", "groups",
    "ln", "readlink", "getfacl", "setfacl", "getfattr", "setfattr",
];

#[derive(Clone)]
pub enum LogLine {
    Text(String),
    /// Text that changes as a command goes on, such as an upload's progress.
    Live(RwSignal<String>),
    Command {
        user: String,
        cwd: String,
        command: String,
    },
    Listing {
        entries: Vec<Entry>,
        long: bool,
    },
}

/// What a command wrote: its text, or a view of it, which shows only at the end of a pipeline.
#[derive(Default)]
pub struct Output {
    pub stdout: String,
    pub view: Option<LogLine>,
    pub failed: bool,
}
impl Output {
    fn text(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            ..Default::default()
        }
    }
}

#[derive(Clone)]
pub struct CompletionSession {
    pub original: String,
    pub cwd: String,
    pub candidates: Vec<Candidate>,
    pub selected: usize,
}

/// Who is signed in, which every tab shares, since it is one session.
#[derive(Clone, Copy)]
pub struct Account {
    pub writable: RwSignal<bool>,
    pub username: RwSignal<String>,
    pub home: RwSignal<String>,
}

#[derive(Clone, Copy)]
pub struct Shell {
    pub id: u64,
    pub account: Account,
    pub cwd: RwSignal<String>,
    pub entries: RwSignal<Vec<Entry>>,
    pub lines: RwSignal<Vec<LogLine>>,
    pub input: RwSignal<String>,
    pub busy: RwSignal<bool>,
    pub directory_error: RwSignal<String>,
    /// What the file panel says of the last thing clicked in it, which the terminal, perhaps
    /// hidden, would say alone: a refusal, or the error of the line it ran ...
    pub notice: RwSignal<String>,
    /// ... while the running line is one the panel started.
    panel: RwSignal<bool>,
    pub history: RwSignal<Vec<String>>,
    pub history_index: RwSignal<usize>,
    pub completion: RwSignal<Option<CompletionSession>>,
    pub completing: RwSignal<bool>,
    pub completion_status: RwSignal<String>,
    pub completion_epoch: RwSignal<u64>,
    pub completion_offset: RwSignal<i64>,
    /// Whether Ctrl+C asked the running line to stop, ...
    cancelled: RwSignal<bool>,
    /// ... which, for an upload, cancels it, ...
    uploading: RwSignal<bool>,
    /// ... for a recording (`stream`), stops it, ...
    recording: RwSignal<bool>,
    /// ... and, waiting for files to upload, gives it none.
    choosing: RwSignal<bool>,
    /// The files the next upload sends: chosen as the command started, or dropped.
    chosen: StoredValue<Option<js_sys::Promise>, LocalStorage>,
    /// Whether the tab was closed: its browser work stops, but not its server jobs.
    detached: RwSignal<bool>,
    /// The answer a running command waits for, if any.
    prompt: StoredValue<Option<oneshot::Sender<String>>>,
    pub asking: RwSignal<bool>,
}

/// Wait `ms` milliseconds.
async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = JsFuture::from(promise).await;
}
fn js_message(error: JsValue) -> String {
    js_sys::Reflect::get(&error, &JsValue::from_str("message"))
        .ok()
        .and_then(|v| v.as_string())
        .or_else(|| error.as_string())
        .unwrap_or_else(|| "Operation failed".into())
}
/// Whether `args` are mv or cp -i, under sudo or not, which ask before replacing a file: run
/// one by one, never in a batch, however xargs or fd -x start them. As on Linux, the last of -i,
/// -n and -f is what counts.
fn asks(args: &[String]) -> bool {
    let args = &args[usize::from(args.first().is_some_and(|a| a == "sudo"))..];
    match args.split_first() {
        Some((name, rest)) if name == "cp" => {
            cli::parse::<cli::files::Cp>(rest).is_ok_and(|cp| cp.interactive)
        }
        Some((name, rest)) if name == "mv" => {
            cli::parse::<cli::files::Mv>(rest).is_ok_and(|mv| mv.interactive)
        }
        _ => false,
    }
}
/// `args`, a command line clap accepts, as web/app.js reads it for the commands it runs itself,
/// with their dialogs: as clap read them, options before operands and -- gone (sudo -ubob is
/// sudo -u bob, vim -- f is vim f). Other commands go as typed, for the server to read.
fn spelled(args: &[String]) -> Result<Vec<String>, String> {
    use cli::{accounts, files, terminal};
    let Some((name, rest)) = args.split_first() else {
        return Ok(Vec::new());
    };
    let mut out = vec![name.clone()];
    let mut option = |on: bool, option: &str, value: Option<String>| {
        if on || value.is_some() {
            out.push(option.into());
        }
        out.extend(value);
    };
    match name.as_str() {
        "sudo" => {
            let sudo = cli::parse::<accounts::Sudo>(rest)?;
            option(false, "-u", sudo.user);
            option(sudo.forget, "-k", None);
            out.extend(spelled(&sudo.command)?);
        }
        "su" => out.extend(cli::parse::<accounts::Su>(rest)?.operands),
        "login" => out.extend(cli::parse::<accounts::Login>(rest)?.user),
        "passwd" => {
            let passwd = cli::parse::<accounts::Passwd>(rest)?;
            option(passwd.force, "--force", None);
            out.extend(passwd.user);
        }
        // Signed out, app.js makes the account; signed in, the server, which reads it again.
        "useradd" => {
            let useradd = cli::parse::<accounts::Useradd>(rest)?;
            option(useradd.force, "--force", None);
            option(useradd.create_home, "-m", None);
            option(false, "-d", useradd.home);
            option(false, "-g", useradd.group);
            option(false, "-G", useradd.groups);
            if useradd.name.starts_with('-') {
                out.push("--".into());
            }
            out.push(useradd.name);
        }
        "vim" => out.push(cli::parse::<files::Vim>(rest)?.file),
        "man" => out.extend(cli::parse::<terminal::Man>(rest)?.command),
        _ => out.extend(rest.iter().cloned()),
    }
    Ok(out)
}
/// A listing as text, as ls writes into a pipe: one name a line.
fn listing_text(entries: &[Entry], long: bool) -> String {
    entries
        .iter()
        .map(|e| {
            if long {
                format!(
                    "{} {:>2} {} {} {:>8} {}{}\n",
                    e.mode_line(),
                    e.links,
                    e.owner,
                    e.group,
                    e.size,
                    e.name,
                    e.link
                        .as_ref()
                        .map(|t| format!(" -> {t}"))
                        .unwrap_or_default()
                )
            } else {
                format!("{}\n", e.name)
            }
        })
        .collect()
}
/// `name` in the directory `typed`, as typed: `""` stands for the working directory.
fn joined(typed: &str, name: &str) -> String {
    match typed {
        "" => name.to_string(),
        t if t.ends_with('/') => format!("{t}{name}"),
        t => format!("{t}/{name}"),
    }
}
/// `-d`'s delimiter: one character, or an escape such as `\n`.
fn parse_delimiter(value: &str) -> Result<char, String> {
    match value {
        "\\n" => Ok('\n'),
        "\\t" => Ok('\t'),
        "\\0" => Ok('\0'),
        _ => {
            let mut chars = value.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Ok(c),
                _ => Err(format!(
                    "xargs: invalid delimiter {value}; use one character"
                )),
            }
        }
    }
}

impl Shell {
    pub fn new(id: u64, account: Account, cwd: String, history: Vec<String>) -> Self {
        let index = history.len();
        Self {
            id,
            account,
            cwd: RwSignal::new(cwd),
            entries: RwSignal::new(Vec::new()),
            lines: RwSignal::new(Vec::new()),
            input: RwSignal::new(String::new()),
            busy: RwSignal::new(false),
            directory_error: RwSignal::new(String::new()),
            notice: RwSignal::new(String::new()),
            panel: RwSignal::new(false),
            history: RwSignal::new(history),
            history_index: RwSignal::new(index),
            completion: RwSignal::new(None),
            completing: RwSignal::new(false),
            completion_status: RwSignal::new(String::new()),
            completion_epoch: RwSignal::new(0),
            completion_offset: RwSignal::new(0),
            cancelled: RwSignal::new(false),
            uploading: RwSignal::new(false),
            recording: RwSignal::new(false),
            choosing: RwSignal::new(false),
            chosen: StoredValue::new_local(None),
            detached: RwSignal::new(false),
            prompt: StoredValue::new(None),
            asking: RwSignal::new(false),
        }
    }
    fn push(self, line: LogLine) {
        self.lines.update(|l| {
            l.push(line);
            if l.len() > KEPT {
                l.remove(0);
            }
        });
    }
    pub fn log(self, value: impl Into<String>) {
        let value: String = value.into();
        let count = value.lines().count();
        let value = if count > SHOWN {
            let mut kept: String = value.lines().take(SHOWN).collect::<Vec<_>>().join("\n");
            kept.push_str(&format!(
                "\n… {} more lines; pipe the output into head, tail or grep to see them",
                count - SHOWN
            ));
            kept
        } else {
            value
        };
        self.push(LogLine::Text(value));
    }
    fn error(self, message: impl AsRef<str>) {
        if self.panel.get_untracked() {
            self.notice.set(message.as_ref().into());
        }
        self.log(format!("! {}", message.as_ref()));
    }
    /// Run `command` for the file panel, which shows its errors, and says so when the tab is
    /// busy, rather than leave a click unanswered while the terminal may be hidden.
    pub fn run_from_panel(self, command: String) {
        if self.busy.get_untracked() || self.asking.get_untracked() {
            self.notice.set(
                "A command is running in this tab; wait for it, or stop it with Ctrl+C".into(),
            );
            return;
        }
        self.panel.set(true);
        self.run(command);
    }
    pub async fn refresh(self) -> Result<(), String> {
        if let Ok(value) = request("/api/session", "GET").await {
            self.account.writable.set(value["authenticated"] == true);
            self.account
                .username
                .set(value["username"].as_str().unwrap_or("guest").into());
            self.account
                .home
                .set(value["home"].as_str().unwrap_or("/").into());
        }
        match listing(&self.cwd.get_untracked()).await {
            Ok(entries) => {
                self.entries.set(entries);
                self.directory_error.set(String::new());
            }
            Err(error) => {
                self.entries.set(Vec::new());
                self.directory_error.set(error);
            }
        }
        Ok(())
    }
    pub fn path(self, value: &str) -> Result<String, String> {
        let expanded = if value == "~" {
            self.account.home.get_untracked()
        } else if let Some(rest) = value.strip_prefix("~/") {
            format!("{}/{rest}", self.account.home.get_untracked())
        } else {
            value.into()
        };
        resolve(&self.cwd.get_untracked(), &expanded)
    }
    fn cancelled(self) -> bool {
        self.cancelled.get_untracked()
    }

    /// Ask the server, through web/app.js, which handles sudo and the commands that need
    /// dialogs.
    async fn call(self, args: &[String]) -> Result<Value, String> {
        // What clap would refuse goes no further, whether app.js or the server runs it.
        cli::check(args)?;
        let args = &spelled(args)?;
        let value = JsFuture::from(account_command(
            serde_wasm_bindgen::to_value(args).map_err(|e| e.to_string())?,
            &self.cwd.get_untracked(),
        ))
        .await
        .map_err(js_message)?;
        serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
    }
    /// What a server command's reply wrote.
    fn output(self, value: &Value, piped: bool) -> Result<Output, String> {
        let mut out = Output::default();
        if let Some(home) = value["home"].as_str() {
            self.cwd.set(home.into());
            set_directory(home);
        }
        if let Some(errors) = value["errors"].as_array() {
            for error in errors {
                self.error(error.as_str().unwrap_or_default());
            }
            out.failed = true;
        }
        if let Some(entries) = value.get("entries") {
            let entries: Vec<Entry> =
                serde_json::from_value(entries.clone()).map_err(|e| e.to_string())?;
            let long = value["long"] == true;
            if piped {
                out.stdout = listing_text(&entries, long);
            } else {
                out.view = Some(LogLine::Listing { entries, long });
            }
        } else if let Some(text) = value["output"].as_str() {
            out.stdout = text.into();
            if !text.is_empty() && !text.ends_with(['\n', '\0']) {
                out.stdout.push('\n');
            }
        }
        Ok(out)
    }
    async fn manage(self, args: &[String], piped: bool) -> Result<Output, String> {
        let value = self.call(args).await?;
        // zip, unzip and tar start a job on the server; the tab follows it.
        if let Some(job) = value["job"].as_str() {
            return self.follow(job, piped).await;
        }
        let out = self.output(&value, piped)?;
        self.refresh().await?;
        Ok(out)
    }
    /// Follow the job `id` to its end: write its lines as they come, answer its question in
    /// this tab, and cancel it on Ctrl+C.
    async fn follow(self, id: &str, piped: bool) -> Result<Output, String> {
        let mut out = Output::default();
        let mut since = 0;
        let mut cancelling = false;
        loop {
            // A closed tab stops following; the job goes on, and jobs lists it.
            if self.detached.get_untracked() {
                return Ok(out);
            }
            if self.cancelled() && !cancelling {
                cancelling = true;
                request(&format!("/api/jobs?id={id}"), "DELETE").await?;
            }
            let view = request(&format!("/api/jobs?id={id}&since={since}"), "GET").await?;
            for line in view["lines"].as_array().into_iter().flatten() {
                since = line[0].as_u64().unwrap_or(since);
                let text = line[2].as_str().unwrap_or_default();
                if line[1] == true {
                    self.error(text);
                } else if piped {
                    out.stdout.push_str(text);
                    out.stdout.push('\n');
                } else {
                    self.log(text);
                }
            }
            // A page at a time: what a job wrote before it ended is all read before it is
            // taken to have ended.
            if view["more"] == true {
                continue;
            }
            match view["state"].as_str().unwrap_or_default() {
                "done" => {
                    out.failed |= view["progress"]["errors"].as_u64().is_some_and(|e| e > 0);
                    break;
                }
                "failed" | "cancelled" => {
                    out.failed = true;
                    break;
                }
                "question" if !cancelling => {
                    let decisions = self.decide(&view["question"]).await;
                    if self.cancelled() {
                        continue;
                    }
                    let body = json!({"decisions": decisions});
                    let request =
                        gloo_net::http::Request::post(&format!("/api/jobs/answer?id={id}"))
                            .json(&body)
                            .map_err(|e| e.to_string())?;
                    request.send().await.map_err(|e| e.to_string())?;
                    continue;
                }
                _ => {}
            }
            sleep(600).await;
        }
        self.refresh().await?;
        Ok(out)
    }
    /// unzip's question about files that exist: replace each, or not, as Info-ZIP asks.
    async fn decide(self, question: &Value) -> serde_json::Map<String, Value> {
        let mut decisions = serde_json::Map::new();
        let mut all = None;
        for path in question["replace"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let replace = match all {
                Some(answer) => answer,
                None => {
                    let answer = self
                        .ask(format!("replace {path}? [y]es, [n]o, [A]ll, [N]one"))
                        .await;
                    match answer.trim() {
                        "A" | "All" => {
                            all = Some(true);
                            true
                        }
                        "N" | "None" => {
                            all = Some(false);
                            false
                        }
                        a => a.to_lowercase().starts_with('y'),
                    }
                }
            };
            if self.cancelled() {
                break;
            }
            decisions.insert(path.into(), json!(replace));
        }
        decisions
    }
    /// The jobs and unfinished uploads of this account, or, given the ID of `jobs cancel ID`,
    /// cancel that one.
    async fn jobs(self, cancel: Option<&str>) -> Result<Output, String> {
        let value = request("/api/jobs", "GET").await?;
        let jobs = value["jobs"].as_array().cloned().unwrap_or_default();
        let uploads = value["uploads"].as_array().cloned().unwrap_or_default();
        match cancel {
            None => {
                let mut text = String::new();
                for job in &jobs {
                    let command = &job["command"];
                    let archive = command["archive"]
                        .as_str()
                        .or(command["file"].as_str())
                        .unwrap_or_default();
                    let progress = &job["progress"];
                    text.push_str(&format!(
                        "{}  {:<10} {} {}  {}/{}\n",
                        &job["id"].as_str().unwrap_or_default()[..8],
                        job["state"].as_str().unwrap_or_default(),
                        job["kind"].as_str().unwrap_or_default(),
                        archive,
                        progress["done"].as_u64().unwrap_or(0),
                        progress["total"].as_u64().unwrap_or(0),
                    ));
                }
                for upload in &uploads {
                    let id = &upload["id"].as_str().unwrap_or_default()[..8];
                    let path = upload["path"].as_str().unwrap_or_default();
                    let size = upload["size"].as_u64().unwrap_or(0);
                    // A recording has as many bytes as have arrived, and grows.
                    if upload["recording"] == true {
                        text.push_str(&format!(
                            "{id}  recording  {path}  {}\n",
                            crate::viewer::size(size)
                        ));
                        continue;
                    }
                    text.push_str(&format!(
                        "{id}  uploading  {path}  {}%\n",
                        upload["received"].as_u64().unwrap_or(0) * 100 / size.max(1),
                    ));
                }
                if text.is_empty() {
                    text = "No jobs or unfinished uploads\n".into();
                }
                Ok(Output::text(text))
            }
            Some(wanted) => {
                let starts = |v: &Value| v["id"].as_str().is_some_and(|id| id.starts_with(wanted));
                if let Some(job) = jobs.iter().find(|j| starts(j)) {
                    let id = job["id"].as_str().unwrap_or_default();
                    request(&format!("/api/jobs?id={id}"), "DELETE").await?;
                    Ok(Output::text(format!("Cancelling job {}\n", &id[..8])))
                } else if let Some(upload) = uploads.iter().find(|u| starts(u)) {
                    let id = upload["id"].as_str().unwrap_or_default();
                    let path = upload["path"].as_str().unwrap_or_default();
                    request(&format!("/api/uploads?id={id}"), "DELETE").await?;
                    self.refresh().await?;
                    // A recording stops, keeping what arrived; an upload is cancelled.
                    Ok(Output::text(if upload["recording"] == true {
                        format!("Stopped the recording of {path}; what arrived is kept\n")
                    } else {
                        format!("Cancelled the upload of {path}\n")
                    }))
                } else {
                    Err(format!("jobs: no job or upload {wanted}"))
                }
            }
        }
    }
    /// Run commands on the server, a batch a request, each in its own transaction.
    async fn batch(self, commands: Vec<Vec<String>>, trace: bool) -> Output {
        let mut out = Output::default();
        for group in commands.chunks(BATCH) {
            if self.cancelled() {
                break;
            }
            if trace {
                for command in group {
                    self.log(format!("+ {}", command.join(" ")));
                }
            }
            let cwd = self.cwd.get_untracked();
            let body = json!({"batch": group.iter().map(|args| json!({"args": args, "cwd": cwd})).collect::<Vec<_>>()});
            let reply = match gloo_net::http::Request::post("/api/command").json(&body) {
                Ok(request) => request.send().await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let value: Result<Value, String> = match reply {
                Ok(response) if response.ok() => response.json().await.map_err(|e| e.to_string()),
                Ok(response) => Err(response
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|v| v["error"].as_str().map(String::from))
                    .unwrap_or_else(|| format!("Request failed ({})", response.status()))),
                Err(e) => Err(e),
            };
            match value {
                Ok(value) => {
                    for result in value["results"].as_array().into_iter().flatten() {
                        if let Some(error) = result["error"].as_str() {
                            self.error(error);
                            out.failed = true;
                        } else if let Ok(o) = self.output(result, true) {
                            out.stdout.push_str(&o.stdout);
                            out.failed |= o.failed;
                        }
                    }
                }
                Err(e) => {
                    self.error(e);
                    out.failed = true;
                    break;
                }
            }
        }
        let _ = self.refresh().await;
        out
    }
    /// Run each of `commands` as xargs or fd -x would: the server's in batches, others one by
    /// one. Their output joins in order; a failure does not stop the next.
    async fn run_all(self, commands: Vec<Vec<String>>, trace: bool) -> Output {
        let Some(first) = commands.first() else {
            return Output::default();
        };
        if BATCHED.contains(&first[0].as_str())
            && commands
                .iter()
                .all(|c| c[0] == first[0] && !cli::wants_help(c) && !asks(c))
        {
            return self.batch(commands, trace).await;
        }
        let mut out = Output::default();
        for command in commands {
            if self.cancelled() {
                break;
            }
            if trace {
                self.log(format!("+ {}", command.join(" ")));
            }
            match self.invoke(command, None, true).await {
                Ok(o) => {
                    out.stdout.push_str(&o.stdout);
                    out.failed |= o.failed;
                }
                Err(e) => {
                    self.error(e);
                    out.failed = true;
                }
            }
        }
        out
    }

    /// Read `value`, a path, as text for a filter.
    async fn read_text(self, value: &str) -> Result<String, String> {
        let path = self.path(value)?;
        let response = gloo_net::http::Request::get(&endpoint("file", &path))
            .header("Accept-Encoding", "identity")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.ok() {
            let value: Value = response.json().await.unwrap_or_default();
            return Err(value["error"]
                .as_str()
                .unwrap_or("cannot read the file")
                .into());
        }
        if response
            .headers()
            .get("Content-Length")
            .and_then(|l| l.parse::<usize>().ok())
            .is_some_and(|l| l > PIPED_MAX)
        {
            return Err("larger than 10 MB".into());
        }
        let bytes = response.binary().await.map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
    async fn filter(self, args: &[String], stdin: Option<String>) -> Result<Output, String> {
        let command = args[0].as_str();
        let filter = text::parse(command, &args[1..])?;
        let mut out = Output::default();
        let mut inputs = Vec::new();
        if filter.files.is_empty() {
            let Some(stdin) = stdin else {
                return Err(format!(
                    "{command}: name a FILE, or pipe text into it, as in ls | {command}"
                ));
            };
            inputs.push(("-".to_string(), stdin));
            let (stdout, status) = text::apply(&filter, &inputs);
            return Ok(Output {
                stdout,
                failed: status != 0,
                ..Default::default()
            });
        }
        for file in &filter.files {
            if file == "-" {
                inputs.push((file.clone(), stdin.clone().unwrap_or_default()));
                continue;
            }
            match self.read_text(file).await {
                Ok(text) => inputs.push((file.clone(), text)),
                Err(e) => {
                    self.error(format!("{command}: {file}: {e}"));
                    out.failed = true;
                }
            }
        }
        let (stdout, status) = text::apply(&filter, &inputs);
        out.stdout = stdout;
        out.failed |= status != 0;
        Ok(out)
    }
    async fn xargs(self, args: &[String], stdin: Option<String>) -> Result<Output, String> {
        let xargs = cli::parse::<cli::text::Xargs>(&args[1..])?;
        let mut delimiter = match (&xargs.delimiter, xargs.null) {
            (Some(d), _) => Delimiter::Char(parse_delimiter(d)?),
            (None, true) => Delimiter::Char('\0'),
            (None, false) => Delimiter::Blank,
        };
        let most = xargs.max_args.map_or(XARGS_MAX, |n| n as usize);
        let replace = xargs.replace;
        if replace.is_some() {
            delimiter = Delimiter::Line;
        }
        let (skip_empty, trace) = (xargs.no_run_if_empty, xargs.verbose);
        let command: Vec<String> = if xargs.command.is_empty() {
            vec!["echo".into()]
        } else {
            xargs.command
        };
        let items = grammar::xargs_items(&stdin.unwrap_or_default(), delimiter)?;
        if items.is_empty() && (skip_empty || replace.is_some()) {
            return Ok(Output::default());
        }
        let commands: Vec<Vec<String>> = match &replace {
            Some(r) => items
                .iter()
                .map(|item| {
                    command
                        .iter()
                        .map(|a| a.replace(r.as_str(), item))
                        .collect()
                })
                .collect(),
            None if items.is_empty() => vec![command],
            None => items
                .chunks(most)
                .map(|chunk| {
                    command
                        .iter()
                        .cloned()
                        .chain(chunk.iter().cloned())
                        .collect()
                })
                .collect(),
        };
        let mut out = self.run_all(commands, trace).await;
        out.failed |= self.cancelled();
        Ok(out)
    }
    /// fd with -x or -X: the server searches, and this runs the command on what it found.
    async fn fd_exec(self, args: &[String]) -> Result<Output, String> {
        let search = find::parse(&args[1..])?;
        let Some((all, template)) = search.exec else {
            return self.manage(args, true).await;
        };
        // The server searches as the line says, leaving the command to this.
        let value = self.call(args).await?;
        let found = self.output(&value, true)?;
        let results: Vec<String> =
            serde_json::from_value(value["results"].clone()).unwrap_or_default();
        if results.is_empty() {
            return Ok(Output {
                failed: found.failed,
                ..Default::default()
            });
        }
        let commands = if all {
            vec![grammar::exec_all(&template, &results)]
        } else {
            results
                .iter()
                .map(|path| grammar::exec_one(&template, path))
                .collect()
        };
        let mut out = self.run_all(commands, false).await;
        out.failed |= found.failed;
        Ok(out)
    }
    /// The answer to `question`, which the next Enter in this tab gives.
    async fn ask(self, question: String) -> String {
        let (sender, answer) = oneshot::channel();
        self.prompt.set_value(Some(sender));
        self.asking.set(true);
        self.log(question);
        focus_input();
        let answer = answer.await.unwrap_or_default();
        self.asking.set(false);
        answer
    }
    /// mv or cp -i, under sudo too: the server does what replaces nothing, and names each file
    /// that exists, which this asks about, in turn, before replacing it as the same account.
    async fn interactive(self, args: &[String]) -> Result<Output, String> {
        let sudo = usize::from(args[0] == "sudo");
        let command = args[sudo].clone();
        // The options again, for each replacement, long ones too (--archive), less -i, which may
        // come with others, as in -ri; the -f after them overrides any -n.
        let mut options = Vec::new();
        for arg in args[sudo + 1..].iter().take_while(|a| a.as_str() != "--") {
            if arg.starts_with("--") {
                if arg != "--interactive" {
                    options.push(arg.clone());
                }
            } else if arg.starts_with('-') && arg.len() > 1 {
                let rest: String = arg[1..].chars().filter(|c| *c != 'i').collect();
                if !rest.is_empty() {
                    options.push(format!("-{rest}"));
                }
            }
        }
        let value = self.call(args).await?;
        let mut out = self.output(&value, true)?;
        for pair in value["existing"].as_array().into_iter().flatten() {
            if self.cancelled() {
                break;
            }
            let (Some(src), Some(target)) = (pair[0].as_str(), pair[1].as_str()) else {
                continue;
            };
            let answer = self
                .ask(format!("{command}: overwrite '{target}'? (y/n)"))
                .await;
            if self.cancelled() || !answer.trim().to_lowercase().starts_with('y') {
                continue;
            }
            let run: Vec<String> = args[..sudo]
                .iter()
                .cloned()
                .chain([command.clone()])
                .chain(options.iter().cloned())
                .chain(["-f", "--", src, target].map(String::from))
                .collect();
            match self.call(&run).await.and_then(|v| self.output(&v, true)) {
                Ok(o) => {
                    out.stdout.push_str(&o.stdout);
                    out.failed |= o.failed;
                }
                Err(e) => {
                    self.error(e);
                    out.failed = true;
                }
            }
        }
        out.failed |= self.cancelled();
        self.refresh().await?;
        Ok(out)
    }

    /// Run one command, with what the previous stage wrote, if any. `piped`: whether its output
    /// goes on to another stage, rather than to the transcript.
    pub fn invoke(
        self,
        args: Vec<String>,
        stdin: Option<String>,
        piped: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            let Some(name) = args.first().cloned() else {
                return Ok(Output::default());
            };
            // Help is the tab's, made from the command's definition: nothing is asked of the
            // server. Under sudo, it is the command sudo runs that is asked about.
            if cli::wants_help(&args) {
                let asked = match args.get(1) {
                    Some(command) if name == "sudo" && !command.starts_with('-') => command,
                    _ => &name,
                };
                if let Some(help) = cli::help(asked) {
                    return Ok(Output::text(help + "\n"));
                }
            }
            match commands::find(&name) {
                Some(command) => command.run(self, args, stdin, piped).await,
                // What the server runs, or web/app.js with its dialogs.
                None if cli::definition(&name).is_some() => self.manage(&args, piped).await,
                None => Err(format!("Unknown command: {name}. Enter help for commands.")),
            }
        }
        .boxed_local()
    }

    /// Run the pipeline `command`, typed into this tab.
    pub fn run(self, command: String) {
        if self.asking.get_untracked() {
            // The line answers the question a command is waiting on.
            self.input.set(String::new());
            self.log(command.clone());
            if let Some(sender) = self.prompt.try_update_value(Option::take).flatten() {
                let _ = sender.send(command);
            }
            return;
        }
        if self.busy.get_untracked() || command.trim().is_empty() {
            return;
        }
        if let Ok(words) = split_command(&command)
            && words
                .iter()
                .any(|a| a.starts_with("-p") || a.starts_with("--password"))
            && words.iter().any(|a| a == "usermod" || a == "useradd")
        {
            self.input.set(String::new());
            self.log("Passwords cannot be command arguments; use passwd [USER] and enter the password in the dialog.");
            return;
        }
        // Phones open a file chooser only while the tap or key that asked is fresh, before any
        // request is awaited: an upload line chooses its files now.
        if split_command(&command).is_ok_and(|words| words.first().is_some_and(|w| w == "upload"))
            && self.chosen.with_value(Option::is_none)
        {
            self.chosen.set_value(Some(choose_files()));
        }
        self.reset_completion();
        self.input.set(String::new());
        // What the panel said of the last click is old news once another line runs.
        self.notice.set(String::new());
        self.push(LogLine::Command {
            user: self.account.username.get_untracked(),
            cwd: self.cwd.get_untracked(),
            command: command.clone(),
        });
        self.history.update(|h| h.push(command.clone()));
        self.history_index
            .set(self.history.with_untracked(Vec::len));
        self.busy.set(true);
        self.cancelled.set(false);
        spawn_local(async move {
            match grammar::words(&command) {
                Ok(stages) => self.pipeline(stages).await,
                Err(e) => self.error(e),
            }
            self.busy.set(false);
            self.cancelled.set(false);
            self.panel.set(false);
            crate::persist();
            focus_input();
        });
    }
    /// The previous command in the history, or with `up` false the next, into the command line.
    pub fn recall(self, up: bool) {
        self.reset_completion();
        let len = self.history.with_untracked(Vec::len);
        let index = self.history_index.get_untracked();
        let index = if up {
            index.saturating_sub(1)
        } else {
            (index + 1).min(len)
        };
        self.history_index.set(index);
        self.input.set(
            self.history
                .with_untracked(|h| h.get(index).cloned().unwrap_or_default()),
        );
    }
    /// Upload `files`, dropped on the page, as the upload command does, into this tab's directory.
    pub fn upload_dropped(self, files: JsValue) {
        if self.busy.get_untracked() {
            self.error("A command is running in this tab; drop the files again when it ends");
            return;
        }
        self.chosen
            .set_value(Some(js_sys::Promise::resolve(&files)));
        self.run("upload".into());
    }
    async fn pipeline(self, stages: Vec<Vec<Word>>) {
        let last = stages.len().saturating_sub(1);
        let mut stdin = None;
        for (index, words) in stages.into_iter().enumerate() {
            // Wildcards expand as the stage starts, as a shell expands them.
            let args = self.expand(words).await;
            if self.cancelled() {
                break;
            }
            let piped = index < last;
            let result = match args {
                Ok(args) => self.invoke(args, stdin.take(), piped).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(out) if piped => {
                    let mut text = out.stdout;
                    if text.len() > PIPED_MAX {
                        self.error("Output larger than 10 MB was cut short for the next command");
                        let mut end = PIPED_MAX;
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        text.truncate(end);
                    }
                    stdin = Some(text);
                }
                Ok(out) => {
                    if let Some(view) = out.view {
                        self.push(view);
                    } else {
                        let text = out.stdout.replace('\0', "\n");
                        let text = text.trim_end_matches('\n');
                        if !text.is_empty() {
                            self.log(text);
                        }
                    }
                }
                Err(e) => {
                    self.error(e);
                    // Like a shell, the next stage still runs, reading nothing.
                    stdin = Some(String::new());
                }
            }
        }
    }
    /// The arguments `words` stand for: each word's wildcards expanded to the paths they match,
    /// sorted, or the word as it is if they match none, as a shell leaves it.
    async fn expand(self, words: Vec<Word>) -> Result<Vec<String>, String> {
        let mut args = Vec::new();
        for word in words {
            let Some(pattern) = word.pattern else {
                args.push(word.text);
                continue;
            };
            let mut paths = self.glob(&pattern).await?;
            if paths.is_empty() {
                args.push(word.text);
            } else {
                paths.sort();
                args.append(&mut paths);
            }
            if args.len() > GLOB_PATHS {
                return Err(format!("{}: Argument list too long", args[0]));
            }
        }
        Ok(args)
    }
    /// The paths `pattern` matches, as typed: relative ones stay relative. A directory that
    /// cannot be listed matches nothing; a pattern ending in `/` matches directories. The
    /// lookups for one part of the pattern run a few at a time.
    async fn glob(self, pattern: &str) -> Result<Vec<String>, String> {
        let parts = glob::parts(pattern);
        // Brackets that close no set: a name, which the word stands for as it is.
        if !parts.iter().any(|part| matches!(part, Part::Pattern(_))) {
            return Ok(Vec::new());
        }
        let directories = pattern.ends_with('/');
        let mut found = vec![if pattern.starts_with('/') { "/" } else { "" }.to_string()];
        let mut lookups = 0;
        // Before the first pattern, names need no looking up; after it, they must exist.
        let mut looked_up = false;
        for (index, part) in parts.iter().enumerate() {
            let last = index + 1 == parts.len();
            let navigation =
                matches!(part, Part::Name(name) if matches!(name.as_str(), "." | ".."));
            // Except `..`, which leaves a directory: it needs search permission there, even
            // before the first pattern, since resolving it removes that directory from the path.
            if let Part::Name(name) = part
                && !looked_up
                && name != ".."
            {
                found = found.iter().map(|typed| joined(typed, name)).collect();
                continue;
            }
            lookups += found.len() * if navigation && looked_up { 2 } else { 1 };
            if lookups > GLOB_LOOKUPS {
                return Err(format!("{pattern}: the pattern searches too many paths"));
            }
            let lookup = move |typed: String| async move {
                if self.cancelled() {
                    return Vec::new();
                }
                // A path this terminal cannot name (a backslash, too deep) matches nothing.
                let Ok(directory) = self.path(if typed.is_empty() { "." } else { &typed }) else {
                    return Vec::new();
                };
                match part {
                    Part::Name(name) => {
                        // Literal names need search permission, not a listing. Dot components
                        // are not listing entries; check traversal before resolving them away.
                        if navigation
                            && request(&(endpoint("stat", &directory) + "&enter=true"), "GET")
                                .await
                                .is_err()
                        {
                            return Vec::new();
                        }
                        let target = joined(&typed, name);
                        if !looked_up {
                            return vec![target];
                        }
                        let Ok(path) = self.path(&target) else {
                            return Vec::new();
                        };
                        match request(&endpoint("stat", &path), "GET").await {
                            Ok(entry) if entry["directory"] == true || last && !directories => {
                                vec![target]
                            }
                            _ => Vec::new(),
                        }
                    }
                    Part::Pattern(pattern) => match listing(&directory).await {
                        Ok(entries) => entries
                            .into_iter()
                            .filter(|entry| {
                                glob::matches(pattern, &entry.name)
                                    && (entry.directory || last && !directories)
                            })
                            .map(|entry| joined(&typed, &entry.name))
                            .collect(),
                        Err(_) => Vec::new(),
                    },
                }
            };
            found = futures::stream::iter(found)
                .map(lookup)
                .buffered(GLOB_CONCURRENCY)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .flatten()
                .collect();
            looked_up |= matches!(part, Part::Pattern(_));
            if self.cancelled() {
                return Ok(Vec::new());
            }
            if found.is_empty() {
                break;
            }
        }
        if directories {
            for path in &mut found {
                path.push('/');
            }
        }
        Ok(found)
    }
    /// The tab is closing: stop its browser work at the next step, but leave its server jobs
    /// running.
    pub fn detach(self) {
        self.detached.set(true);
        self.cancelled.set(true);
        self.prompt.set_value(None);
        if self.choosing.get_untracked() {
            cancel_choosing();
        }
    }
    /// Ctrl+C: clear the line, and stop what this tab is running at its next step.
    pub fn interrupt(self) {
        self.reset_completion();
        self.input.set(String::new());
        self.history_index
            .set(self.history.with_untracked(Vec::len));
        if self.busy.get_untracked() && !self.cancelled.get_untracked() {
            self.cancelled.set(true);
            if self.uploading.get_untracked() {
                cancel_uploads();
            }
            if self.recording.get_untracked() {
                stop_recording();
            }
            if self.choosing.get_untracked() {
                cancel_choosing();
            }
            // A command waiting for an answer gets none.
            self.prompt.set_value(None);
            self.log("^C");
        }
    }

    pub fn reset_completion(self) {
        self.completion_epoch.update(|epoch| *epoch += 1);
        self.completion.set(None);
        self.completing.set(false);
        self.completion_status.set(String::new());
    }
    pub fn select_completion(self, index: usize) {
        if let Some(mut session) = self.completion.get_untracked() {
            session.selected = index % session.candidates.len();
            let value = session.candidates[session.selected].value.clone();
            self.input.set(value.clone());
            self.completion.set(Some(session));
            completion_focus(&value, false);
        }
    }
    pub fn accept_completion(self) {
        let inside_quote = self.completion.with_untracked(|s| {
            s.as_ref().is_some_and(|s| {
                let c = &s.candidates[s.selected];
                c.descend && c.kind != "command"
            })
        });
        let value = self.input.get_untracked();
        self.reset_completion();
        completion_focus(&value, inside_quote);
    }
    pub fn cancel_completion(self) {
        if let Some(session) = self.completion.get_untracked() {
            self.input.set(session.original);
        }
        self.reset_completion();
    }
    fn finish_completion(self, original: String, cwd: String, candidates: Vec<Candidate>) {
        self.completing.set(false);
        if candidates.is_empty() {
            self.completion_status.set("No matches".into());
            return;
        }
        self.completion_status.set(String::new());
        let selected = self
            .completion_offset
            .get_untracked()
            .rem_euclid(candidates.len() as i64) as usize;
        self.completion.set(Some(CompletionSession {
            original,
            cwd,
            candidates,
            selected,
        }));
        self.select_completion(selected);
    }
    pub fn complete(self, backwards: bool) {
        if self.busy.get_untracked() {
            return;
        }
        let step = if backwards { -1 } else { 1 };
        if self.completing.get_untracked() {
            // Multiple Tabs during a slow R2 listing still advance the eventual selection.
            self.completion_offset.update(|index| *index += step);
            return;
        }
        let input = self.input.get_untracked();
        let cwd = self.cwd.get_untracked();
        if let Some(session) = self.completion.get_untracked()
            && session.cwd == cwd
            && session.candidates[session.selected].value == input
            // A sole command/directory can continue into its arguments/children.
            && (session.candidates.len() != 1 || !session.candidates[0].descend)
        {
            let index = (session.selected as i64 + step).rem_euclid(session.candidates.len() as i64)
                as usize;
            self.select_completion(index);
            return;
        }
        self.reset_completion();
        self.completion_offset.set(if backwards { -1 } else { 0 });
        let epoch = self.completion_epoch.get_untracked();
        match completion::query(&input) {
            Query::Commands(candidates) => self.finish_completion(input, cwd, candidates),
            Query::None => self
                .completion_status
                .set("No path completion is available here".into()),
            Query::Paths(query) => {
                let directory = match self.path(&query.prefix) {
                    Ok(path) => path,
                    Err(error) => {
                        self.completion_status.set(error);
                        return;
                    }
                };
                self.completing.set(true);
                self.completion_status.set("Loading completions...".into());
                spawn_local(async move {
                    let result = listing(&directory).await;
                    // Never let a late request overwrite new typing, history, or a new cwd.
                    if self.completion_epoch.get_untracked() != epoch
                        || self.input.get_untracked() != input
                        || self.cwd.get_untracked() != cwd
                    {
                        return;
                    }
                    match result {
                        Ok(entries) => self.finish_completion(
                            input,
                            cwd,
                            completion::path_candidates(&query, &entries),
                        ),
                        Err(error) => {
                            self.completing.set(false);
                            self.completion_status.set(error);
                        }
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::spelled;

    #[test]
    fn app_js_reads_its_commands_as_clap_read_them() {
        let line = |text: &str| text.split(' ').map(String::from).collect::<Vec<_>>();
        let spell = |text: &str| spelled(&line(text)).unwrap().join(" ");
        assert_eq!(spell("sudo -ubob"), "sudo -u bob");
        assert_eq!(spell("sudo -k"), "sudo -k");
        assert_eq!(spell("vim -- -notes.md"), "vim -notes.md");
        assert_eq!(spell("useradd -- ann"), "useradd ann");
        assert_eq!(
            spell("sudo useradd -m -G team,web -- -odd"),
            "sudo useradd -m -G team,web -- -odd"
        );
        assert_eq!(spell("passwd bob --force"), "passwd --force bob");
        assert_eq!(spell("su - bob"), "su - bob");
        assert_eq!(spell("sudo passwd -- bob"), "sudo passwd bob");
        // The server's commands go as typed.
        assert_eq!(spell("sudo rm -rf -- -h"), "sudo rm -rf -- -h");
    }
}
