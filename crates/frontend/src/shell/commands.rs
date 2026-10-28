//! The commands a tab runs, each a [`TerminalCommand`] registered by name in [`COMMANDS`]: those
//! the page does itself (cd, open, upload, the text filters, xargs, fd -x...), and those it
//! decides how to hand on (ls and mkdir with options, cp and mv -i, sudo). Every other command
//! [`terminal_worker_core::cli`] defines goes to the server through web/app.js's
//! accountCommand, which shows the dialogs some need. Their arguments, and their help, are their
//! definitions there.

use futures::FutureExt;
use futures::future::LocalBoxFuture;
use leptos::prelude::*;
use terminal_worker_core::cli::{self, files, terminal, text as filters};
use terminal_worker_core::{Entry, text};
use wasm_bindgen::{JsCast, closure::Closure};
use wasm_bindgen_futures::JsFuture;

use super::{LogLine, Output, Shell, asks, js_message, listing_text};
use crate::{
    authorize, choose_files, endpoint, listing, open_file, record_sound, request, set_directory,
    upload_files,
};

/// A command the tab runs.
pub trait TerminalCommand: Sync {
    /// Its name, as [`terminal_worker_core::cli`] defines it.
    fn name(&self) -> &'static str;
    /// Run `args`, its whole line, in `shell`, with what the stage before wrote, if any; `piped`
    /// if what it writes goes on to another stage.
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        stdin: Option<String>,
        piped: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>>;
}

/// Every command the tab runs itself, or hands on as it decides.
pub static COMMANDS: &[&dyn TerminalCommand] = &[
    &Echo,
    &Filter("cat"),
    &Filter("head"),
    &Filter("tail"),
    &Filter("grep"),
    &Filter("wc"),
    &Filter("sort"),
    &Filter("uniq"),
    &Xargs,
    &Fd,
    &Jobs,
    &Asking("cp"),
    &Asking("mv"),
    &Asking("sudo"),
    &Ls,
    &Mkdir,
    &Cd,
    &Pwd,
    &Clear,
    &Open,
    &Present,
    &Stream,
    &Upload,
    &Help,
];

/// The command named `name`, if the tab runs it.
pub fn find(name: &str) -> Option<&'static dyn TerminalCommand> {
    COMMANDS.iter().copied().find(|c| c.name() == name)
}

/// `args` without the command's name.
fn rest(args: &[String]) -> &[String] {
    args.get(1..).unwrap_or_default()
}

struct Echo;
impl TerminalCommand for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn run(
        &self,
        _: Shell,
        args: Vec<String>,
        _: Option<String>,
        _: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            let typed = rest(&args);
            let echo = cli::parse::<filters::Echo>(typed)?;
            // echo has no --: one before its words is a word, as GNU echo writes it.
            let start = typed.len() - echo.words.len();
            let start = if start > 0 && typed[start - 1] == "--" {
                start - 1
            } else {
                start
            };
            Ok(Output::text(format!(
                "{}{}",
                typed[start..].join(" "),
                if echo.no_newline { "" } else { "\n" }
            )))
        }
        .boxed_local()
    }
}

/// A text filter: cat, head, tail, grep, wc, sort or uniq.
struct Filter(&'static str);
impl TerminalCommand for Filter {
    fn name(&self) -> &'static str {
        self.0
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        stdin: Option<String>,
        _: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        debug_assert!(text::FILTERS.contains(&self.0));
        async move { shell.filter(&args, stdin).await }.boxed_local()
    }
}

struct Xargs;
impl TerminalCommand for Xargs {
    fn name(&self) -> &'static str {
        "xargs"
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        stdin: Option<String>,
        _: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move { shell.xargs(&args, stdin).await }.boxed_local()
    }
}

/// fd: with -x or -X, the server searches and the tab runs the command; otherwise the server.
struct Fd;
impl TerminalCommand for Fd {
    fn name(&self) -> &'static str {
        "fd"
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        _: Option<String>,
        piped: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            let find = terminal_worker_core::find::parse(rest(&args))?;
            if find.exec.is_some() {
                shell.fd_exec(&args).await
            } else {
                shell.manage(&args, piped).await
            }
        }
        .boxed_local()
    }
}

struct Jobs;
impl TerminalCommand for Jobs {
    fn name(&self) -> &'static str {
        "jobs"
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        _: Option<String>,
        _: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            let jobs = cli::parse::<cli::archives::Jobs>(rest(&args))?;
            shell.jobs(jobs.id.as_deref()).await
        }
        .boxed_local()
    }
}

/// cp, mv, and sudo running either: with -i, the tab asks about each file the server says
/// exists; otherwise the server does all.
struct Asking(&'static str);
impl TerminalCommand for Asking {
    fn name(&self) -> &'static str {
        self.0
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        _: Option<String>,
        piped: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            if asks(&args) {
                shell.interactive(&args).await
            } else {
                shell.manage(&args, piped).await
            }
        }
        .boxed_local()
    }
}

/// ls of one directory, which the tab lists itself; with options, the server.
struct Ls;
impl TerminalCommand for Ls {
    fn name(&self) -> &'static str {
        "ls"
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        _: Option<String>,
        piped: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            let ls = cli::parse::<files::Ls>(rest(&args))?;
            if ls.long || ls.directory || ls.all {
                return shell.manage(&args, piped).await;
            }
            let path = shell.path(ls.path.as_deref().unwrap_or("."))?;
            let entries = listing(&path).await?;
            if path == shell.cwd.get_untracked() {
                shell.entries.set(entries.clone());
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
        .boxed_local()
    }
}

/// mkdir of one directory, with its parents, as the API does; with -p or several, the server.
struct Mkdir;
impl TerminalCommand for Mkdir {
    fn name(&self) -> &'static str {
        "mkdir"
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        _: Option<String>,
        piped: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            let mkdir = cli::parse::<files::Mkdir>(rest(&args))?;
            let [dir] = mkdir.dirs.as_slice() else {
                return shell.manage(&args, piped).await;
            };
            if mkdir.parents {
                return shell.manage(&args, piped).await;
            }
            let path = shell.path(dir)?;
            authorize(terminal_worker_core::parent(&path)).await?;
            request(&endpoint("mkdir", &path), "POST").await?;
            shell.refresh().await?;
            Ok(Output::text(format!("Created {path}")))
        }
        .boxed_local()
    }
}

struct Cd;
impl TerminalCommand for Cd {
    fn name(&self) -> &'static str {
        "cd"
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        _: Option<String>,
        _: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            let cd = cli::parse::<files::Cd>(rest(&args))?;
            let path = match &cd.dir {
                Some(dir) => shell.path(dir)?,
                None => shell.account.home.get_untracked(),
            };
            request(&format!("{}&enter=true", endpoint("stat", &path)), "GET").await?;
            let result = listing(&path).await;
            shell.cwd.set(path.clone());
            set_directory(&path);
            match result {
                Ok(entries) => {
                    shell.entries.set(entries);
                    shell.directory_error.set(String::new());
                }
                Err(error) => {
                    shell.entries.set(Vec::new());
                    shell.directory_error.set(error);
                }
            }
            Ok(Output::default())
        }
        .boxed_local()
    }
}

struct Pwd;
impl TerminalCommand for Pwd {
    fn name(&self) -> &'static str {
        "pwd"
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        _: Option<String>,
        _: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            cli::parse::<files::Pwd>(rest(&args))?;
            Ok(Output::text(format!("{}\n", shell.cwd.get_untracked())))
        }
        .boxed_local()
    }
}

struct Clear;
impl TerminalCommand for Clear {
    fn name(&self) -> &'static str {
        "clear"
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        _: Option<String>,
        _: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            cli::parse::<terminal::Clear>(rest(&args))?;
            shell.lines.set(Vec::new());
            Ok(Output::default())
        }
        .boxed_local()
    }
}

struct Open;
impl TerminalCommand for Open {
    fn name(&self) -> &'static str {
        "open"
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        _: Option<String>,
        _: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            let open = cli::parse::<files::Open>(rest(&args))?;
            let path = shell.path(&open.file)?;
            let entry: Entry =
                serde_json::from_value(request(&endpoint("stat", &path), "GET").await?)
                    .map_err(|e| e.to_string())?;
            if entry.directory {
                return Err("This is a directory; use cd".into());
            }
            JsFuture::from(open_file(&path, &entry.kind))
                .await
                .map_err(js_message)?;
            Ok(Output::default())
        }
        .boxed_local()
    }
}

struct Present;
impl TerminalCommand for Present {
    fn name(&self) -> &'static str {
        "present"
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        _: Option<String>,
        _: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            let present = cli::parse::<files::Present>(rest(&args))?;
            let encode = |text: &str| String::from(js_sys::encode_uri_component(text));
            if let Some(name) = present.stop {
                let stopped =
                    request(&format!("/api/present?name={}", encode(&name)), "DELETE").await?;
                let device = stopped["device"].as_str().unwrap_or(&name);
                return Ok(Output::text(format!("Stopped {device}\n")));
            }
            let path = shell.path(present.file.as_deref().unwrap_or_default())?;
            let mut url = endpoint("present", &path);
            if let Some(name) = &present.name {
                url.push_str(&format!("&name={}", encode(name)));
            }
            let started = request(&url, "POST").await?;
            let device = started["device"].as_str().unwrap_or_default().to_string();
            // Shown presented, a PDF a page at a time and Markdown as slides, from where the
            // account showed it last.
            let kind = terminal_worker_core::kind(&path);
            let place = &started["place"];
            let kept = match (kind, place.is_array()) {
                ("pdf", true) => serde_json::json!({ "present": true, "page": place[0] }),
                ("pdf", false) => serde_json::json!({ "present": true }),
                (_, true) => serde_json::json!({ "slides": true, "slide": place }),
                (_, false) => serde_json::json!({ "slides": true }),
            };
            crate::viewer::set_place(&path, kept);
            JsFuture::from(open_file(&path, kind))
                .await
                .map_err(js_message)?;
            let name = device.rsplit('/').next().unwrap_or_default();
            Ok(Output::text(format!(
                "Presenting {path} as {device}: whoever may read it follows it with open {device}; present --stop {name} ends it\n"
            )))
        }
        .boxed_local()
    }
}

struct Stream;
impl TerminalCommand for Stream {
    fn name(&self) -> &'static str {
        "stream"
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        _: Option<String>,
        _: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            let stream = cli::parse::<files::Stream>(rest(&args))?;
            let path = shell.path(&stream.file)?;
            let lower = path.to_lowercase();
            if !lower.ends_with(".weba") && !lower.ends_with(".webm") {
                return Err(format!(
                    "stream: {}: browsers record Opus in WebM; name the recording NAME.weba",
                    stream.file
                ));
            }
            // A line that follows the recording; Ctrl+C in this tab stops it, as it cancels an
            // upload.
            let line = RwSignal::new(String::from("● starting the microphone"));
            shell.push(LogLine::Live(line));
            let progress = Closure::<dyn Fn(String)>::new(move |text: String| line.set(text));
            shell.recording.set(true);
            let result = JsFuture::from(record_sound(
                &path,
                stream.name,
                progress.as_ref().unchecked_ref(),
            ))
            .await;
            shell.recording.set(false);
            drop(progress);
            let saved = result
                .map_err(js_message)
                .inspect_err(|e| line.set(format!("✕ {e}")))?;
            let get = |name: &str| js_sys::Reflect::get(&saved, &name.into()).unwrap_or_default();
            let seconds = get("seconds").as_f64().unwrap_or_default() as u64;
            let size = get("size").as_f64().unwrap_or_default() as u64;
            let note = get("note").as_string().unwrap_or_default();
            line.set(format!(
                "✓ {path} · {:02}:{:02} · {}{note}",
                seconds / 60,
                seconds % 60,
                crate::viewer::size(size)
            ));
            shell.refresh().await?;
            Ok(Output::default())
        }
        .boxed_local()
    }
}

struct Upload;
impl TerminalCommand for Upload {
    fn name(&self) -> &'static str {
        "upload"
    }
    fn run(
        &self,
        shell: Shell,
        args: Vec<String>,
        _: Option<String>,
        _: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            // The files were chosen as the line started, while the tap that asked was fresh.
            let chosen = shell.chosen.try_update_value(Option::take).flatten();
            let upload = cli::parse::<files::Upload>(rest(&args))?;
            let path = shell.path(upload.dir.as_deref().unwrap_or("."))?;
            shell.choosing.set(true);
            let files = JsFuture::from(chosen.unwrap_or_else(choose_files)).await;
            shell.choosing.set(false);
            let files = files.map_err(js_message)?;
            if js_sys::Array::from(&files).length() == 0 {
                return Ok(Output::default());
            }
            // A line that follows the upload, here rather than over the page.
            let line = RwSignal::new(String::from("↑ starting"));
            shell.push(LogLine::Live(line));
            let progress = Closure::<dyn Fn(String)>::new(move |text: String| line.set(text));
            shell.uploading.set(true);
            let result = JsFuture::from(upload_files(
                &files,
                &path,
                progress.as_ref().unchecked_ref(),
            ))
            .await;
            shell.uploading.set(false);
            drop(progress);
            let results: Vec<String> = result
                .map_err(js_message)
                .inspect_err(|e| line.set(format!("✕ {e}")))?
                .dyn_into::<js_sys::Array>()
                .map(|a| a.iter().filter_map(|r| r.as_string()).collect())
                .unwrap_or_default();
            let failed = results.iter().any(|r| r.starts_with('✕'));
            line.set(results.join("\n"));
            shell.refresh().await?;
            Ok(Output {
                failed,
                ..Output::default()
            })
        }
        .boxed_local()
    }
}

/// help: every command by section, or one's help, as -h shows it.
struct Help;
impl TerminalCommand for Help {
    fn name(&self) -> &'static str {
        "help"
    }
    fn run(
        &self,
        _: Shell,
        args: Vec<String>,
        _: Option<String>,
        _: bool,
    ) -> LocalBoxFuture<'static, Result<Output, String>> {
        async move {
            let help = cli::parse::<terminal::Help>(rest(&args))?;
            let text = match &help.command {
                Some(name) => cli::help(name)
                    .ok_or_else(|| format!("Unknown command: {name}. Enter help for commands."))?,
                None => cli::commands(),
            };
            Ok(Output::text(text + "\n"))
        }
        .boxed_local()
    }
}
