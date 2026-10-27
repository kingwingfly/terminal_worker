//! One terminal tab: its directory, transcript, history and completion, and the commands it
//! runs. A line is a pipeline: each stage's output is the next one's input. The server runs
//! file and account commands; text filters, xargs and fd -x run here, as do browser
//! interactions such as open and upload.

use futures::channel::oneshot;
use futures::future::LocalBoxFuture;
use futures::{FutureExt, StreamExt};
use leptos::prelude::*;
use serde_json::{Value, json};
use terminal_worker_core::completion::{self, Candidate, Query};
use terminal_worker_core::glob::{self, Part};
use terminal_worker_core::shell::{self as grammar, Delimiter, Word};
use terminal_worker_core::{Entry, find, resolve, split_command, text};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::{
    account_command, authorize, cancel_uploads, choose_files, completion_focus, endpoint,
    focus_input, listing, open_file, request, set_directory, upload_files,
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

/// Commands the server runs, through web/app.js's accountCommand; ls and mkdir too, with
/// options or several paths.
const SERVER: &[&str] = &[
    "help",
    "man",
    "vim",
    "touch",
    "sudo",
    "su",
    "login",
    "logout",
    "exit",
    "passwd",
    "chmod",
    "chown",
    "useradd",
    "usermod",
    "userdel",
    "groupadd",
    "groupmod",
    "groupdel",
    "whoami",
    "id",
    "groups",
    "users",
    "rm",
    "mv",
    "cp",
    "fd",
    "zip",
    "unzip",
    "tar",
    "ln",
    "readlink",
    "chgrp",
    "getfacl",
    "setfacl",
    "getfattr",
    "setfattr",
    "faststart",
];
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
    pub history: RwSignal<Vec<String>>,
    pub history_index: RwSignal<usize>,
    pub completion: RwSignal<Option<CompletionSession>>,
    pub completing: RwSignal<bool>,
    pub completion_status: RwSignal<String>,
    pub completion_epoch: RwSignal<u64>,
    pub completion_offset: RwSignal<i64>,
    /// Whether Ctrl+C asked the running line to stop, ...
    cancelled: RwSignal<bool>,
    /// ... which, for an upload, cancels it.
    uploading: RwSignal<bool>,
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
/// Commands for which -h is an option, as on Linux: chown, chgrp, getfattr and setfattr act on a
/// link itself with it, tar follows links. Alone, it still asks for help.
const H_OPTION: &[&str] = &["chown", "chgrp", "getfattr", "setfattr", "tar"];
/// Whether `args` ask for help before `--`.
fn wants_help(args: &[String]) -> bool {
    // sudo COMMAND: COMMAND's -h is what counts.
    let at = usize::from(
        args.first().is_some_and(|a| a == "sudo")
            && args.get(1).is_some_and(|a| !a.starts_with('-')),
    );
    let h_option = H_OPTION.contains(&args.get(at).map(String::as_str).unwrap_or_default());
    args.iter()
        .skip(1)
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--help" || arg == "-h" && (!h_option || args.len() - at == 2))
}
/// Whether `args` are mv or cp -i, under sudo or not, which ask before replacing a file: run
/// one by one, never in a batch, however xargs or fd -x start them.
fn asks(args: &[String]) -> bool {
    let args = &args[usize::from(args.first().is_some_and(|a| a == "sudo"))..];
    matches!(args.first().map(String::as_str), Some("mv" | "cp"))
        && args
            .iter()
            .skip(1)
            .take_while(|a| a.as_str() != "--")
            .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('i'))
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
            history: RwSignal::new(history),
            history_index: RwSignal::new(index),
            completion: RwSignal::new(None),
            completing: RwSignal::new(false),
            completion_status: RwSignal::new(String::new()),
            completion_epoch: RwSignal::new(0),
            completion_offset: RwSignal::new(0),
            cancelled: RwSignal::new(false),
            uploading: RwSignal::new(false),
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
        self.log(format!("! {}", message.as_ref()));
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
    /// The jobs and unfinished uploads of this account, or `jobs cancel ID`.
    async fn jobs(self, args: &[String]) -> Result<Output, String> {
        let value = request("/api/jobs", "GET").await?;
        let jobs = value["jobs"].as_array().cloned().unwrap_or_default();
        let uploads = value["uploads"].as_array().cloned().unwrap_or_default();
        match args.get(1).map(String::as_str) {
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
                    let size = upload["size"].as_u64().unwrap_or(0).max(1);
                    text.push_str(&format!(
                        "{}  uploading  {}  {}%\n",
                        &upload["id"].as_str().unwrap_or_default()[..8],
                        upload["path"].as_str().unwrap_or_default(),
                        upload["readable"].as_u64().unwrap_or(0) * 100 / size,
                    ));
                }
                if text.is_empty() {
                    text = "No jobs or unfinished uploads\n".into();
                }
                Ok(Output::text(text))
            }
            Some("cancel") if args.len() == 3 => {
                let wanted = args[2].as_str();
                let starts = |v: &Value| v["id"].as_str().is_some_and(|id| id.starts_with(wanted));
                if let Some(job) = jobs.iter().find(|j| starts(j)) {
                    let id = job["id"].as_str().unwrap_or_default();
                    request(&format!("/api/jobs?id={id}"), "DELETE").await?;
                    Ok(Output::text(format!("Cancelling job {}\n", &id[..8])))
                } else if let Some(upload) = uploads.iter().find(|u| starts(u)) {
                    let id = upload["id"].as_str().unwrap_or_default();
                    request(&format!("/api/uploads?id={id}"), "DELETE").await?;
                    self.refresh().await?;
                    Ok(Output::text(format!(
                        "Cancelled the upload of {}\n",
                        upload["path"].as_str().unwrap_or_default()
                    )))
                } else {
                    Err(format!("jobs: no job or upload {wanted}"))
                }
            }
            _ => Err("Usage: jobs [cancel ID]".into()),
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
                .all(|c| c[0] == first[0] && !wants_help(c) && !asks(c))
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
        let mut delimiter = Delimiter::Blank;
        let (mut most, mut replace, mut skip_empty, mut trace) = (XARGS_MAX, None, false, false);
        let mut i = 1;
        while i < args.len() && args[i].starts_with('-') {
            let value = |i: usize| {
                args.get(i + 1)
                    .cloned()
                    .ok_or_else(|| format!("xargs: {} needs a value", args[i]))
            };
            match args[i].as_str() {
                "--" => {
                    i += 1;
                    break;
                }
                "-0" | "--null" => delimiter = Delimiter::Char('\0'),
                "-r" | "--no-run-if-empty" => skip_empty = true,
                "-t" | "--verbose" => trace = true,
                "-d" => {
                    delimiter = Delimiter::Char(parse_delimiter(&value(i)?)?);
                    i += 1;
                }
                "-n" => {
                    let n = value(i)?;
                    most = n
                        .parse()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| format!("xargs: invalid number {n}"))?;
                    i += 1;
                }
                "-I" => {
                    replace = Some(value(i)?);
                    delimiter = Delimiter::Line;
                    i += 1;
                }
                other => {
                    return Err(format!("xargs: unsupported option {other}; see man xargs"));
                }
            }
            i += 1;
        }
        let command: Vec<String> = if i < args.len() {
            args[i..].to_vec()
        } else {
            vec!["echo".into()]
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
        let cut = args
            .iter()
            .position(|a| ["-x", "-X", "--exec", "--exec-batch"].contains(&a.as_str()))
            .unwrap_or(args.len());
        let value = self.call(&args[..cut]).await?;
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
        // The options again, for each replacement, less -i, which may come with others, as in -ri.
        let mut options = Vec::new();
        for arg in args[sudo + 1..].iter().take_while(|a| a.as_str() != "--") {
            if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
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
            let Some(cmd) = args.first().cloned() else {
                return Ok(Output::default());
            };
            if wants_help(&args) {
                return self.manage(&args, piped).await;
            }
            let several = args.len() > 2 || args.get(1).is_some_and(|s| s.starts_with('-'));
            match cmd.as_str() {
                "echo" => {
                    let newline = args.get(1).map(String::as_str) != Some("-n");
                    let words = &args[if newline { 1 } else { 2 }.min(args.len())..];
                    Ok(Output::text(format!(
                        "{}{}",
                        words.join(" "),
                        if newline { "\n" } else { "" }
                    )))
                }
                c if text::FILTERS.contains(&c) => self.filter(&args, stdin).await,
                "xargs" => self.xargs(&args, stdin).await,
                "fd" if args
                    .iter()
                    .any(|a| ["-x", "-X", "--exec", "--exec-batch"].contains(&a.as_str())) =>
                {
                    self.fd_exec(&args).await
                }
                _ if asks(&args) => self.interactive(&args).await,
                "jobs" => self.jobs(&args).await,
                c if SERVER.contains(&c) || (matches!(c, "ls" | "mkdir") && several) => {
                    self.manage(&args, piped).await
                }
                _ => self.builtin(&args, piped).await,
            }
        }
        .boxed_local()
    }
    /// The commands the page runs itself.
    async fn builtin(self, args: &[String], piped: bool) -> Result<Output, String> {
        let cmd = args[0].as_str();
        if args.len() > 2 {
            return Err("Only one path is accepted; quote paths containing spaces".into());
        }
        let arg = args.get(1).map(String::as_str);
        let path = self.path(arg.unwrap_or("."))?;
        match cmd {
            "pwd" => Ok(Output::text(format!("{}\n", self.cwd.get_untracked()))),
            "clear" => {
                self.lines.set(Vec::new());
                Ok(Output::default())
            }
            "ls" => {
                let entries = listing(&path).await?;
                if path == self.cwd.get_untracked() {
                    self.entries.set(entries.clone());
                }
                if piped {
                    Ok(Output::text(listing_text(&entries, false)))
                } else if entries.is_empty() {
                    Ok(Output::text("(empty directory)"))
                } else {
                    Ok(Output {
                        view: Some(LogLine::Listing {
                            entries,
                            long: false,
                        }),
                        ..Default::default()
                    })
                }
            }
            "cd" => {
                let path = if arg.is_none() {
                    self.account.home.get_untracked()
                } else {
                    path
                };
                request(&format!("{}&enter=true", endpoint("stat", &path)), "GET").await?;
                let result = listing(&path).await;
                self.cwd.set(path.clone());
                set_directory(&path);
                match result {
                    Ok(entries) => {
                        self.entries.set(entries);
                        self.directory_error.set(String::new());
                    }
                    Err(error) => {
                        self.entries.set(Vec::new());
                        self.directory_error.set(error);
                    }
                }
                Ok(Output::default())
            }
            "mkdir" => {
                if arg.is_none() {
                    return Err("Usage: mkdir [-p] PATH...".into());
                }
                authorize(terminal_worker_core::parent(&path)).await?;
                request(&endpoint("mkdir", &path), "POST").await?;
                self.refresh().await?;
                Ok(Output::text(format!("Created {path}")))
            }
            "open" => {
                if arg.is_none() {
                    return Err("Usage: open <FILE>".into());
                }
                let entry: Entry =
                    serde_json::from_value(request(&endpoint("stat", &path), "GET").await?)
                        .map_err(|e| e.to_string())?;
                if entry.directory {
                    return Err("This is a directory; use cd".into());
                }
                JsFuture::from(open_file(&path, &entry.kind))
                    .await
                    .map_err(|e| e.as_string().unwrap_or_else(|| "Open failed".into()))?;
                Ok(Output::default())
            }
            "upload" => {
                let files = self.chosen.try_update_value(Option::take).flatten();
                let files = JsFuture::from(files.unwrap_or_else(choose_files))
                    .await
                    .map_err(js_message)?;
                if js_sys::Array::from(&files).length() == 0 {
                    return Ok(Output::default());
                }
                // A line that follows the upload, here rather than over the page.
                let line = RwSignal::new(String::from("↑ starting"));
                self.push(LogLine::Live(line));
                let progress = Closure::<dyn Fn(String)>::new(move |text: String| line.set(text));
                self.uploading.set(true);
                let result = JsFuture::from(upload_files(
                    &files,
                    &path,
                    progress.as_ref().unchecked_ref(),
                ))
                .await;
                self.uploading.set(false);
                drop(progress);
                let results: Vec<String> = result
                    .map_err(js_message)
                    .inspect_err(|e| line.set(format!("✕ {e}")))?
                    .dyn_into::<js_sys::Array>()
                    .map(|a| a.iter().filter_map(|r| r.as_string()).collect())
                    .unwrap_or_default();
                let failed = results.iter().any(|r| r.starts_with('✕'));
                line.set(results.join("\n"));
                self.refresh().await?;
                Ok(Output {
                    failed,
                    ..Output::default()
                })
            }
            _ => Err(format!("Unknown command: {cmd}. Enter help for commands.")),
        }
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
