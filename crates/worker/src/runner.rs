//! The ArchiveRunner: one Durable Object, with its container (crates/archiver), a job. Its
//! alarm does the work in steps, keeping its plan in its own storage, so that a restart, of it
//! or of the container, resumes rather than starts over:
//!
//! - extracting: send the archive, list it, plan where each member goes (asking, through the
//!   job, about files that exist), then write member by member, a chunk at a time;
//! - listing: send the archive and write its members' names;
//! - creating: send the inputs, build the archive, and store it a chunk at a time. Building is
//!   deterministic, so after an interruption the chunks already stored are checked against
//!   the new build's, and the upload starts over only if one differs.
//!
//! Bytes go between R2 and the container as streams, and every file written is written through
//! the authority, as the job's account.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use terminal_worker_core::CHUNK_SIZE;
use terminal_worker_core::archive::{Existing, Job as Command, member_path};
use terminal_worker_core::permissions::{mode_text, type_mode_text};
use wasm_bindgen::JsValue;
use worker::*;

use crate::api::chunk_key;
use crate::db::Job;
use crate::files::decode_hex;
use crate::jobs::Spec;
use crate::streams;

/// The archiver's port in its container.
const PORT: u16 = 8080;
/// How long one alarm works before handing over to the next.
const BUDGET: f64 = 20_000.0;
/// How many failed steps in a row end a job.
const ATTEMPTS: u32 = 5;

#[durable_object]
pub struct ArchiveRunner {
    state: State,
    env: Env,
}

/// What a step leaves to do.
enum Step {
    /// More work: the next alarm continues.
    More,
    /// Nothing until someone answers the job's question.
    Wait,
    Done,
}

/// Lines a step writes to its job: to standard output, or with `err`, to standard error.
#[derive(Default)]
struct Lines(Vec<(bool, String)>);
impl Lines {
    fn out(&mut self, line: impl Into<String>) {
        self.0.push((false, line.into()));
    }
    fn err(&mut self, line: impl Into<String>) {
        self.0.push((true, line.into()));
    }
}

#[derive(Deserialize)]
struct Row {
    idx: u64,
    name: String,
    size: u64,
    /// The member's permission bits, if the archive keeps them: a zip made elsewhere may not.
    mode: Option<u32>,
    /// file, directory, symlink or hardlink.
    kind: String,
    target: String,
    action: String,
    /// A symbolic link's target, as written, or the path a hard link names.
    link: Option<String>,
}
/// A plan's columns, as a [`Row`].
const ROW: &str = "idx, name, size, mode, kind, target, action, link";

/// The permission bits a member gets, as each tool gives them: unzip those archived, as tar
/// does for root, setuid, setgid and sticky too; tar for others less the umask (022). Without
/// any, 644 for a file and 755 for a directory.
fn extracted_mode(cmd: &str, root: bool, mode: Option<u32>, directory: bool) -> u32 {
    match mode {
        Some(mode) if cmd == "tar" && root => mode & 0o7777,
        Some(mode) if cmd == "tar" => mode & 0o777 & !0o022,
        Some(mode) => mode & 0o777,
        None if directory => 0o755,
        None => 0o644,
    }
}

impl DurableObject for ArchiveRunner {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }
    async fn fetch(&self, req: Request) -> Result<Response> {
        let url = req.url()?;
        let job = url
            .query_pairs()
            .find(|(k, _)| k == "job")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        if url.path() != "/start" || job.is_empty() {
            return Response::error("Unknown runner request", 404);
        }
        self.state.storage().put("job", &job).await?;
        self.state.storage().set_alarm(1).await?;
        Response::ok("started")
    }
    async fn alarm(&self) -> Result<Response> {
        let storage = self.state.storage();
        let Some(job) = storage.get::<String>("job").await? else {
            return Response::ok("idle");
        };
        match self.work(&job).await {
            Ok(Step::More) => {
                storage.put("failures", 0u32).await?;
                storage.set_alarm(1).await?;
            }
            Ok(Step::Wait) => storage.put("failures", 0u32).await?,
            Ok(Step::Done) => self.close().await,
            Err(e) => {
                let failures = storage.get::<u32>("failures").await?.unwrap_or(0) + 1;
                console_error!(
                    "job {} step failed ({} of {}): {}",
                    job,
                    failures,
                    ATTEMPTS,
                    e
                );
                if failures >= ATTEMPTS {
                    let _ = self
                        .call(
                            Method::Post,
                            &format!("/internal/job/update?id={job}"),
                            Some(json!({"lines": [[true, format!("The job failed: {e}")]]})),
                        )
                        .await;
                    let _ = self.abort_upload(&job).await;
                    let _ = self
                        .call(
                            Method::Post,
                            &format!("/internal/job/finish?id={job}"),
                            Some(json!({"state": "failed"})),
                        )
                        .await;
                    self.close().await;
                } else {
                    // Said in the job too, which would otherwise seem to hang.
                    let _ = self
                        .call(
                            Method::Post,
                            &format!("/internal/job/update?id={job}"),
                            Some(json!({"lines": [[true, format!(
                                "Attempt {failures} of {ATTEMPTS} failed, trying again: {e}"
                            )]]})),
                        )
                        .await;
                    storage.put("failures", failures).await?;
                    storage.set_alarm(i64::from(2000 * failures)).await?;
                }
            }
        }
        Response::ok("")
    }
}

impl ArchiveRunner {
    /// Ask the authority, for the job.
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<(u16, Value)> {
        let headers = Headers::new();
        let mut init = RequestInit::new();
        init.with_method(method);
        if let Some(body) = body {
            headers.set("Content-Type", "application/json")?;
            init.with_body(Some(body.to_string().into()));
        }
        init.with_headers(headers);
        let mut response = self
            .env
            .durable_object("AUTHORITY")?
            .id_from_name("filesystem")?
            .get_stub()?
            .fetch_with_request(Request::new_with_init(
                &format!("https://authority{path}"),
                &init,
            )?)
            .await?;
        let status = response.status_code();
        Ok((status, response.json().await.unwrap_or_default()))
    }
    /// Ask the authority, failing the step on an error reply.
    async fn ask(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let (status, value) = self.call(method, path, body).await?;
        if status >= 400 {
            return Err(Error::RustError(
                value["error"]
                    .as_str()
                    .unwrap_or("The authority refused")
                    .to_string(),
            ));
        }
        Ok(value)
    }
    async fn update(&self, job: &str, lines: Lines, progress: Option<Value>) -> Result<String> {
        let mut body =
            json!({"lines": lines.0.iter().map(|(e, l)| json!([e, l])).collect::<Vec<_>>()});
        if let Some(progress) = progress {
            body["progress"] = progress;
        }
        let value = self
            .ask(
                Method::Post,
                &format!("/internal/job/update?id={job}"),
                Some(body),
            )
            .await?;
        Ok(value["state"].as_str().unwrap_or_default().to_string())
    }
    async fn close(&self) {
        if let Some(container) = self.state.container()
            && container.running()
        {
            let _ = container.destroy(None).await;
        }
        let _ = self.state.storage().delete_all().await;
    }
    /// Cancel the upload in progress, if any.
    async fn abort_upload(&self, job: &str) -> Result<()> {
        if let Some(upload) = self.state.storage().get::<String>("upload").await? {
            self.ask(
                Method::Post,
                &format!("/internal/job/abort?id={job}"),
                Some(json!({"upload": upload})),
            )
            .await?;
            self.state.storage().delete("upload").await?;
        }
        Ok(())
    }

    /// The archiver, started if need be.
    async fn archiver(&self) -> Result<Fetcher> {
        let container = self.state.container().ok_or_else(|| {
            Error::RustError(
                "Containers are not available; see the README's archive section".into(),
            )
        })?;
        // A container that cannot start says so only to its monitor: keep why, so as to stop
        // waiting for it at once.
        let ended = Rc::new(RefCell::new(None::<String>));
        if !container.running() {
            container.start(None)?;
            if let Some(watched) = self.state.container() {
                let ended = ended.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let why = match watched.wait_for_exit().await {
                        Ok(()) => "it exited".to_string(),
                        Err(e) => e.to_string(),
                    };
                    *ended.borrow_mut() = Some(why);
                });
            }
        }
        let fetcher = container.get_tcp_port(PORT)?;
        for _ in 0..120 {
            if let Some(why) = ended.borrow_mut().take() {
                return Err(Error::RustError(format!(
                    "The archiver did not start: {why}"
                )));
            }
            if let Ok(response) = fetcher.fetch("http://archiver/status", None).await
                && response.status_code() == 200
            {
                return Ok(fetcher);
            }
            Delay::from(Duration::from_millis(500)).await;
        }
        Err(Error::RustError("The archiver did not start".into()))
    }
    async fn status(&self, fetcher: &Fetcher, job: &str) -> Result<Value> {
        let mut response = fetcher.fetch("http://archiver/status", None).await?;
        let status: Value = response.json().await?;
        // Another job's files, or none: start again from nothing.
        Ok(if status["job"].as_str() == Some(job) {
            status
        } else {
            json!({})
        })
    }
    /// Send the file the job pins, `file`, to the archiver at `url`.
    async fn send(&self, fetcher: &Fetcher, job: &str, file: &str, url: &str) -> Result<()> {
        let grant = self
            .ask(
                Method::Get,
                &format!("/internal/job/file?id={job}&file={file}"),
                None,
            )
            .await?;
        let size = grant["size"].as_u64().unwrap_or(0);
        let chunk_size = grant["chunk_size"].as_u64().unwrap_or(1).max(1);
        let mut parts: Vec<(u64, String)> = grant["chunks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| Some((c[0].as_u64()?, c[1].as_str()?.to_string())))
            .collect();
        parts.sort();
        let parts = parts
            .into_iter()
            .map(|(index, key)| (key, chunk_size.min(size - index * chunk_size)))
            .collect();
        let mut init = RequestInit::new();
        init.with_method(Method::Put);
        init.with_body(Some(if size == 0 {
            JsValue::from_str("")
        } else {
            streams::objects(self.env.bucket("FILES")?, parts).into()
        }));
        let response = fetcher
            .fetch(format!("http://archiver{url}"), Some(init))
            .await?;
        if response.status_code() != 200 {
            return Err(Error::RustError(format!("the archiver refused {url}")));
        }
        Ok(())
    }
    /// Store `length` bytes the archiver serves at `url` as the R2 object `key`.
    async fn store(
        &self,
        fetcher: &Fetcher,
        url: &str,
        key: &str,
        length: u64,
        sha256: Option<&str>,
    ) -> Result<()> {
        let mut response = fetcher.fetch(format!("http://archiver{url}"), None).await?;
        if response.status_code() != 200 {
            let error: Value = response.json().await.unwrap_or_default();
            return Err(Error::RustError(
                error["error"]
                    .as_str()
                    .unwrap_or("the archiver failed")
                    .to_string(),
            ));
        }
        let ResponseBody::Stream(stream) = response.body() else {
            return Err(Error::RustError("the archiver sent no content".into()));
        };
        let data = Data::ReadableStream(streams::fixed_length(stream, length as f64));
        let bucket = self.env.bucket("FILES")?;
        let mut put = bucket.put(key, data);
        if let Some(bytes) = sha256.and_then(decode_hex) {
            put = put.sha256(bytes);
        }
        put.execute().await?;
        Ok(())
    }

    async fn work(&self, id: &str) -> Result<Step> {
        let job: Job = serde_json::from_value(
            self.ask(Method::Get, &format!("/internal/job?id={id}"), None)
                .await?,
        )
        .map_err(|e| Error::RustError(e.to_string()))?;
        match job.state.as_str() {
            "done" | "failed" | "cancelled" => return Ok(Step::Done),
            "question" => return Ok(Step::Wait),
            "cancelling" => {
                self.abort_upload(id).await?;
                self.ask(
                    Method::Post,
                    &format!("/internal/job/finish?id={id}"),
                    Some(json!({"state": "cancelled"})),
                )
                .await?;
                return Ok(Step::Done);
            }
            _ => {}
        }
        if job.state == "queued" {
            self.ask(
                Method::Post,
                &format!("/internal/job/update?id={id}"),
                Some(json!({"state": "running"})),
            )
            .await?;
        }
        let spec = Spec::of(&job).map_err(Error::RustError)?;
        let deadline = Date::now().as_millis() as f64 + BUDGET;
        let step = match &spec.command {
            Command::Extract { .. } => self.extract(&job, &spec, deadline).await?,
            Command::List { .. } => self.list(&job, &spec).await?,
            Command::Create { .. } => self.create(&job, &spec, deadline).await?,
        };
        if matches!(step, Step::Done) {
            self.ask(
                Method::Post,
                &format!("/internal/job/finish?id={id}"),
                Some(json!({"state": "done"})),
            )
            .await?;
        }
        Ok(step)
    }

    /// Make sure the archiver has the job's archive, and has listed it, unpacked if `unpack`.
    async fn prepare(
        &self,
        fetcher: &Fetcher,
        job: &str,
        spec: &Spec,
        unpack: bool,
    ) -> Result<Value> {
        let status = self.status(fetcher, job).await?;
        let source = spec
            .source
            .as_ref()
            .ok_or_else(|| Error::RustError("the job has no archive".into()))?;
        if status["archive"] != true {
            self.send(fetcher, job, &source.file, &format!("/archive?job={job}"))
                .await?;
        }
        let mut response = fetcher
            .fetch(
                format!(
                    "http://archiver/list?job={job}{}",
                    if unpack { "&unpack=1" } else { "" }
                ),
                Some({
                    let mut init = RequestInit::new();
                    init.with_method(Method::Post);
                    init
                }),
            )
            .await?;
        let status = response.status_code();
        let value: Value = response.json().await?;
        if status != 200 {
            return Err(Error::RustError(format!(
                "{}: {}",
                source.path,
                value["error"].as_str().unwrap_or("cannot be read")
            )));
        }
        Ok(value)
    }

    async fn list(&self, job: &Job, spec: &Spec) -> Result<Step> {
        let Command::List { zip, verbose, .. } = &spec.command else {
            return Ok(Step::Done);
        };
        let fetcher = self.archiver().await?;
        let listing = self.prepare(&fetcher, &job.id, spec, false).await?;
        let mut lines = Lines::default();
        let members = listing["members"].as_array().cloned().unwrap_or_default();
        let source = spec
            .source
            .as_ref()
            .map(|s| s.path.as_str())
            .unwrap_or_default();
        if *zip {
            lines.out(format!("Archive:  {source}"));
            lines.out("  Length  Name");
            lines.out("---------  ----");
            let mut total = 0;
            for m in &members {
                let size = m["size"].as_u64().unwrap_or(0);
                total += size;
                lines.out(format!(
                    "{size:>9}  {}",
                    m["name"].as_str().unwrap_or_default()
                ));
            }
            lines.out("---------  -------");
            lines.out(format!(
                "{total:>9}  {} file{}",
                members.len(),
                if members.len() == 1 { "" } else { "s" }
            ));
        } else {
            for m in &members {
                let name = m["name"].as_str().unwrap_or_default();
                if *verbose {
                    let kind = match m["kind"].as_str() {
                        Some("directory") => 'd',
                        Some("symlink") => 'l',
                        Some("hardlink") => 'h',
                        _ => '-',
                    };
                    let link = m["link"].as_str().unwrap_or_default();
                    lines.out(format!(
                        "{} {:>9} {name}{}",
                        type_mode_text(m["mode"].as_u64().unwrap_or(0) as u16 & 0o7777, kind),
                        m["size"].as_u64().unwrap_or(0),
                        match kind {
                            'l' => format!(" -> {link}"),
                            'h' => format!(" link to {link}"),
                            _ => String::new(),
                        }
                    ));
                } else {
                    lines.out(name);
                }
            }
        }
        self.update(
            &job.id,
            lines,
            Some(json!({"done": members.len(), "total": members.len()})),
        )
        .await?;
        Ok(Step::Done)
    }

    fn sql(&self) -> SqlStorage {
        self.state.storage().sql()
    }
    fn rows(&self, query: &str, args: Vec<SqlStorageValue>) -> Result<Vec<Row>> {
        self.sql().exec(query, args)?.to_array()
    }
    /// How many members of the plan match `condition`.
    fn count(&self, condition: &str) -> Result<usize> {
        #[derive(Deserialize)]
        struct Count {
            n: usize,
        }
        let counted: Count = self
            .sql()
            .exec(
                &format!("SELECT COUNT(*) AS n FROM plan WHERE {condition}"),
                None,
            )?
            .one()?;
        Ok(counted.n)
    }

    async fn extract(&self, job: &Job, spec: &Spec, deadline: f64) -> Result<Step> {
        let Command::Extract {
            zip,
            existing,
            members: wanted,
            verbose,
            ..
        } = &spec.command
        else {
            return Ok(Step::Done);
        };
        let cmd = if *zip { "unzip" } else { "tar" };
        let id = job.id.as_str();
        let directory = spec.directory.clone().unwrap_or_else(|| "/".into());
        let fetcher = self.archiver().await?;
        let sql = self.sql();
        sql.exec(
            "CREATE TABLE IF NOT EXISTS plan(idx INTEGER PRIMARY KEY, name TEXT NOT NULL, size INTEGER NOT NULL, mode INTEGER, kind TEXT NOT NULL, target TEXT NOT NULL, action TEXT NOT NULL, link TEXT, done INTEGER NOT NULL)",
            None,
        )?;
        // Said once the plan is made, which may have no members at all.
        let planned = self.state.storage().get::<bool>("planned").await? == Some(true);
        let status = self.status(&fetcher, id).await?;
        if !planned || status["listed"] != true {
            // The container lost its files, or never had them: send and unpack again.
            let listing = self.prepare(&fetcher, id, spec, true).await?;
            if !planned {
                return self
                    .plan(
                        id, cmd, spec, &directory, *existing, wanted, *verbose, &listing,
                    )
                    .await;
            }
        }
        if spec.decisions.is_none() {
            let asking: Vec<String> = self
                .rows(
                    &format!("SELECT {ROW} FROM plan WHERE action = 'ask' ORDER BY idx"),
                    vec![],
                )?
                .into_iter()
                .map(|row| row.target)
                .collect();
            if !asking.is_empty() {
                // A job waiting for its answer does not come here: the step that planned failed
                // before its question reached the job, so ask it now.
                self.ask(
                    Method::Post,
                    &format!("/internal/job/update?id={id}"),
                    Some(json!({"question": {"replace": asking}, "state": "question"})),
                )
                .await?;
                return Ok(Step::Wait);
            }
        }
        let decisions = spec.decisions.clone().unwrap_or_default();
        let total = self.count("1")?;
        let mut done = self.count("done = 1")?;
        // A member that a restart interrupted starts again.
        self.abort_upload(id).await?;
        let mut lines = Lines::default();
        // Those of the steps before, planning's included, go on counting.
        let mut errors = serde_json::from_str::<Value>(&job.progress).unwrap_or_default()["errors"]
            .as_u64()
            .unwrap_or(0);
        // Members go in order, so the next one is past the last, found through the key; symbolic
        // links after all the rest, as GNU tar makes them, so that nothing this archive holds
        // is written through one it makes.
        let mut after = -1;
        let mut links = false;
        loop {
            let Some(row) = self
                .rows(
                    &format!("SELECT {ROW} FROM plan WHERE done = 0 AND (kind = 'symlink') = ? AND idx > ? ORDER BY idx LIMIT 1"),
                    vec![SqlStorageValue::Integer(links as i64), SqlStorageValue::Integer(after)],
                )?
                .pop()
            else {
                if links {
                    break;
                }
                links = true;
                after = -1;
                continue;
            };
            after = row.idx as i64;
            let replace = match row.action.as_str() {
                "replace" => true,
                "ask" => decisions.get(&row.target).copied().unwrap_or(false),
                _ => false,
            };
            match row.action.as_str() {
                "mkdir" => {
                    let (status, reply) = self
                        .call(
                            Method::Post,
                            &format!("/internal/job/mkdir?id={id}"),
                            Some(json!({"path": row.target})),
                        )
                        .await?;
                    if status >= 400 {
                        errors += 1;
                        lines.err(format!(
                            "{cmd}: {}: {}",
                            row.target,
                            reply["error"].as_str().unwrap_or("cannot create")
                        ));
                    } else if *verbose {
                        lines.out(if *zip {
                            format!("   creating: {}/", row.target)
                        } else {
                            row.name.clone()
                        });
                    }
                }
                "write" | "replace" | "ask"
                    if row.action != "ask" || decisions.contains_key(&row.target) =>
                {
                    let written = if row.kind == "file" {
                        // A file is made anew, with its mode from the start.
                        let mode = extracted_mode(cmd, job.actor == 0, row.mode, false);
                        self.write(&fetcher, id, &row, &directory, replace, mode)
                            .await?
                            .map(|made| made.is_some())
                    } else {
                        self.link(id, &row, &directory, replace).await?
                    };
                    match written {
                        Ok(true) => {
                            if *verbose {
                                lines.out(match (*zip, row.kind.as_str()) {
                                    (true, "symlink") => format!(
                                        "    linking: {}  -> {}",
                                        row.target,
                                        row.link.as_deref().unwrap_or_default()
                                    ),
                                    (true, _) => format!("  inflating: {}", row.target),
                                    (false, _) => row.name.clone(),
                                });
                            }
                        }
                        Ok(false) => {}
                        Err(message) => {
                            errors += 1;
                            lines.err(format!("{cmd}: {}: {message}", row.target));
                        }
                    }
                }
                _ => {}
            }
            sql.exec(
                "UPDATE plan SET done = 1 WHERE idx = ?",
                vec![SqlStorageValue::Integer(row.idx as i64)],
            )?;
            done += 1;
            let state = self
                .update(
                    id,
                    std::mem::take(&mut lines),
                    Some(json!({"done": done, "total": total, "errors": errors})),
                )
                .await?;
            if state == "cancelling" {
                return Ok(Step::More);
            }
            if Date::now().as_millis() as f64 > deadline {
                return Ok(Step::More);
            }
        }
        // Directories get their modes last, deepest first, so that none stops its own filling:
        // those that were there before too, whatever mode they had. One that is not there, or
        // not where the job may look, failed to be made, and said so.
        let directories = self.rows(
            &format!("SELECT {ROW} FROM plan WHERE action = 'mkdir' ORDER BY length(target) DESC"),
            vec![],
        )?;
        let paths: Vec<&String> = directories.iter().map(|row| &row.target).collect();
        let found = self
            .ask(
                Method::Post,
                &format!("/internal/job/exists?id={id}"),
                Some(json!({"paths": paths})),
            )
            .await?;
        for row in &directories {
            let mode = extracted_mode(cmd, job.actor == 0, row.mode, true);
            let Some(had) = found["modes"][&row.target].as_u64() else {
                continue;
            };
            if had == u64::from(mode) {
                continue;
            }
            let (status, reply) = self
                .call(
                    Method::Post,
                    &format!("/internal/job/chmod?id={id}"),
                    Some(json!({"path": row.target, "mode": mode})),
                )
                .await?;
            if status >= 400 {
                errors += 1;
                lines.err(format!(
                    "{cmd}: {}: Cannot change mode to {}: {}",
                    row.target,
                    &mode_text(mode as u16, true)[1..],
                    reply["error"].as_str().unwrap_or("not permitted")
                ));
            }
        }
        if !lines.0.is_empty() {
            self.update(
                id,
                lines,
                Some(json!({"done": done, "total": total, "errors": errors})),
            )
            .await?;
        }
        Ok(Step::Done)
    }

    /// Decide where each member goes, skipping those that cannot go anywhere safe, and ask,
    /// through the job, about files that exist when unzip has no -o or -n.
    #[allow(clippy::too_many_arguments)]
    async fn plan(
        &self,
        id: &str,
        cmd: &str,
        spec: &Spec,
        directory: &str,
        existing: Existing,
        wanted: &[String],
        verbose: bool,
        listing: &Value,
    ) -> Result<Step> {
        let mut lines = Lines::default();
        if cmd == "unzip" && verbose {
            lines.out(format!(
                "Archive:  {}",
                spec.source
                    .as_ref()
                    .map(|s| s.path.as_str())
                    .unwrap_or_default()
            ));
        }
        #[allow(clippy::type_complexity)]
        let mut rows: Vec<(
            u64,
            String,
            u64,
            Option<u32>,
            String,
            String,
            String,
            Option<String>,
        )> = Vec::new();
        let mut targets = BTreeMap::new();
        let mut stripped_noted = false;
        let members = listing["members"].as_array().cloned().unwrap_or_default();
        // The members asked for, and whether any of the archive's is each.
        let mut matched = vec![false; wanted.len()];
        for (index, member) in members.iter().enumerate() {
            let name = member["name"].as_str().unwrap_or_default().to_string();
            let trimmed = name.trim_end_matches('/');
            if !wanted.is_empty() {
                let mut any = false;
                for (w, seen) in wanted.iter().zip(&mut matched) {
                    let w = w.trim_end_matches('/');
                    if trimmed == w || trimmed.starts_with(&format!("{w}/")) {
                        *seen = true;
                        any = true;
                    }
                }
                if !any {
                    continue;
                }
            }
            let kind = member["kind"].as_str().unwrap_or("other").to_string();
            let mut link = member["link"].as_str().map(String::from);
            match kind.as_str() {
                "other" => {
                    lines.err(format!(
                        "{cmd}: skipping {name}: not a regular file, directory or link"
                    ));
                    continue;
                }
                "symlink" if link.as_deref().is_none_or(str::is_empty) => {
                    lines.err(format!("{cmd}: skipping {name}: the link has no target"));
                    continue;
                }
                // A hard link names a member, which must be one it may write as safely.
                "hardlink" => match member_path(directory, link.as_deref().unwrap_or_default()) {
                    Ok((path, _)) => link = Some(path),
                    Err(reason) => {
                        lines.err(format!(
                            "{cmd}: skipping {name}: what it links to: {reason}"
                        ));
                        continue;
                    }
                },
                _ => {}
            }
            let (target, stripped) = match member_path(directory, &name) {
                Ok(found) => found,
                Err(reason) => {
                    lines.err(format!("{cmd}: skipping {name}: {reason}"));
                    continue;
                }
            };
            if stripped && !stripped_noted {
                stripped_noted = true;
                lines.err(if cmd == "tar" {
                    "tar: Removing leading `/' from member names".to_string()
                } else {
                    format!("warning: stripped absolute path spec from {name}")
                });
            }
            if targets.insert(target.clone(), kind.clone()).is_some() {
                lines.err(format!(
                    "{cmd}: skipping {name}: another member has the same name"
                ));
                continue;
            }
            let action = if kind == "directory" {
                "mkdir"
            } else {
                "write"
            };
            rows.push((
                index as u64,
                name,
                member["size"].as_u64().unwrap_or(0),
                member["mode"].as_u64().map(|m| m as u32),
                kind,
                target,
                action.to_string(),
                link,
            ));
        }
        let paths: Vec<&String> = rows.iter().map(|r| &r.5).collect();
        let found = self
            .ask(
                Method::Post,
                &format!("/internal/job/exists?id={id}"),
                Some(json!({"paths": paths})),
            )
            .await?;
        let mut conflicts = Vec::new();
        for row in &mut rows {
            let Some(there) = found["existing"][&row.5].as_str() else {
                continue;
            };
            match (row.4.as_str(), there) {
                ("directory", "directory") => {}
                ("directory", _) | (_, "directory") => {
                    lines.err(format!(
                        "{cmd}: {}: cannot replace a {there} with a {}",
                        row.5,
                        if row.4 == "directory" {
                            "directory"
                        } else {
                            "file"
                        }
                    ));
                    row.6 = "skip".into();
                }
                _ => match existing {
                    Existing::Replace => row.6 = "replace".into(),
                    Existing::Skip => row.6 = "skip".into(),
                    Existing::Keep => {
                        lines.err(format!("{cmd}: {}: Cannot open: File exists", row.5));
                        row.6 = "skip".into();
                    }
                    Existing::Ask => {
                        row.6 = "ask".into();
                        conflicts.push(row.5.clone());
                    }
                },
            }
        }
        let sql = self.sql();
        for row in &rows {
            sql.exec(
                "INSERT OR REPLACE INTO plan(idx, name, size, mode, kind, target, action, link, done) VALUES(?, ?, ?, ?, ?, ?, ?, ?, 0)",
                vec![
                    SqlStorageValue::Integer(row.0 as i64),
                    row.1.as_str().into(),
                    SqlStorageValue::Integer(row.2 as i64),
                    row.3.map(i64::from).into(),
                    row.4.as_str().into(),
                    row.5.as_str().into(),
                    row.6.as_str().into(),
                    row.7.as_deref().into(),
                ],
            )?;
        }
        // As Info-ZIP and GNU tar say of what was asked for and is not there, a failure each.
        let mut errors = 0;
        for (w, _) in wanted.iter().zip(&matched).filter(|(_, seen)| !**seen) {
            errors += 1;
            lines.err(if cmd == "tar" {
                format!("tar: {w}: Not found in archive")
            } else {
                format!("caution: filename not matched:  {w}")
            });
        }
        if members.is_empty() && cmd == "unzip" {
            errors += 1;
            lines.err(format!(
                "warning [{}]:  zipfile is empty",
                spec.source
                    .as_ref()
                    .map(|s| s.path.as_str())
                    .unwrap_or_default()
            ));
        }
        let mut body = json!({
            "lines": lines.0.iter().map(|(e, l)| json!([e, l])).collect::<Vec<_>>(),
            "progress": {"done": 0, "total": rows.len(), "errors": errors},
        });
        let asking = !conflicts.is_empty() && spec.decisions.is_none();
        if asking {
            body["question"] = json!({"replace": conflicts});
            body["state"] = json!("question");
        }
        self.ask(
            Method::Post,
            &format!("/internal/job/update?id={id}"),
            Some(body),
        )
        .await?;
        self.state.storage().put("planned", true).await?;
        Ok(if asking {
            Step::Wait
        } else if rows.is_empty() {
            // Nothing to write: the job ends now, rather than plan again.
            Step::Done
        } else {
            Step::More
        })
    }

    /// Write one member, through an upload of its own, a new file with `mode`: Ok with the mode
    /// the file has if written, Ok(None) if left as it was, Err with a message if refused.
    async fn write(
        &self,
        fetcher: &Fetcher,
        id: &str,
        row: &Row,
        directory: &str,
        replace: bool,
        mode: u32,
    ) -> Result<std::result::Result<Option<u32>, String>> {
        let parent = terminal_worker_core::parent(&row.target);
        if parent != directory {
            let (status, reply) = self
                .call(
                    Method::Post,
                    &format!("/internal/job/mkdir?id={id}"),
                    Some(json!({"path": parent})),
                )
                .await?;
            if status >= 400 {
                return Ok(Err(reply["error"]
                    .as_str()
                    .unwrap_or("cannot create its directory")
                    .into()));
            }
        }
        let (status, begin) = self
            .call(
                Method::Post,
                &format!("/internal/job/begin?id={id}"),
                Some(json!({
                    "path": row.target, "size": row.size, "replace": replace, "mode": mode,
                    "unlink": true,
                })),
            )
            .await?;
        if status >= 400 {
            return Ok(Err(begin["error"]
                .as_str()
                .unwrap_or("cannot be written")
                .into()));
        }
        if begin["skipped"] == true {
            return Ok(Ok(None));
        }
        let had = begin["mode"].as_u64().map_or(0o644, |m| m as u32);
        let upload = begin["id"].as_str().unwrap_or_default().to_string();
        self.state.storage().put("upload", &upload).await?;
        let count = row.size.div_ceil(CHUNK_SIZE);
        for index in 0..count {
            let offset = index * CHUNK_SIZE;
            let length = CHUNK_SIZE.min(row.size - offset);
            let key = chunk_key(&upload, index);
            self.store(
                fetcher,
                &format!(
                    "/entry?job={id}&index={}&offset={offset}&length={length}",
                    row.idx
                ),
                &key,
                length,
                None,
            )
            .await?;
            self.ask(
                Method::Post,
                &format!("/internal/job/commit?id={id}"),
                Some(json!({"upload": upload, "index": index, "key": key})),
            )
            .await?;
        }
        let (status, done) = self
            .call(
                Method::Post,
                &format!("/internal/job/commit?id={id}"),
                Some(json!({"upload": upload, "complete": true})),
            )
            .await?;
        self.state.storage().delete("upload").await?;
        if status >= 400 {
            return Ok(Err(done["error"]
                .as_str()
                .unwrap_or("cannot be written")
                .into()));
        }
        Ok(Ok(Some(had)))
    }

    /// Make the symbolic or hard link `row` is: Ok(true) if made, Ok(false) if left as it was,
    /// Err with a message if refused.
    async fn link(
        &self,
        id: &str,
        row: &Row,
        directory: &str,
        replace: bool,
    ) -> Result<std::result::Result<bool, String>> {
        let parent = terminal_worker_core::parent(&row.target);
        if parent != directory {
            let (status, reply) = self
                .call(
                    Method::Post,
                    &format!("/internal/job/mkdir?id={id}"),
                    Some(json!({"path": parent})),
                )
                .await?;
            if status >= 400 {
                return Ok(Err(reply["error"]
                    .as_str()
                    .unwrap_or("cannot create its directory")
                    .into()));
            }
        }
        let endpoint = if row.kind == "symlink" {
            "symlink"
        } else {
            "link"
        };
        let (status, reply) = self
            .call(
                Method::Post,
                &format!("/internal/job/{endpoint}?id={id}"),
                Some(json!({"path": row.target, "target": row.link, "replace": replace})),
            )
            .await?;
        if status >= 400 {
            return Ok(Err(reply["error"]
                .as_str()
                .unwrap_or("cannot be linked")
                .into()));
        }
        Ok(Ok(reply["skipped"] != true))
    }

    async fn create(&self, job: &Job, spec: &Spec, deadline: f64) -> Result<Step> {
        let Command::Create {
            format, verbose, ..
        } = &spec.command
        else {
            return Ok(Step::Done);
        };
        let id = job.id.as_str();
        let storage = self.state.storage();
        let fetcher = self.archiver().await?;
        let status = self.status(&fetcher, id).await?;
        let present: Vec<String> =
            serde_json::from_value(status["inputs"].clone()).unwrap_or_default();
        for (index, input) in spec.inputs.iter().enumerate() {
            if let Some(file) = &input.file
                && !present.contains(&index.to_string())
            {
                self.send(
                    &fetcher,
                    id,
                    file,
                    &format!("/input?job={id}&index={index}"),
                )
                .await?;
                if Date::now().as_millis() as f64 > deadline {
                    return Ok(Step::More);
                }
            }
        }
        let inputs: Vec<Value> = spec
            .inputs
            .iter()
            .enumerate()
            .map(|(index, i)| {
                json!({
                    "name": i.name, "file": i.file.as_ref().map(|_| index.to_string()), "mode": i.mode,
                    "owner": i.owner, "group": i.group, "uid": i.uid, "gid": i.gid, "store": i.store,
                    "symlink": i.symlink, "hardlink": i.hardlink,
                })
            })
            .collect();
        // Built once: a later step stores what the container built for it, unless the container
        // lost it, rather than building it all again, and storing a chunk, each step.
        let cached = storage
            .get::<String>("built")
            .await?
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .filter(|built| {
                status["output"]
                    .as_u64()
                    .is_some_and(|size| built["size"].as_u64() == Some(size))
            });
        let built = match cached {
            Some(built) => built,
            None => {
                let mut init = RequestInit::new();
                init.with_method(Method::Post);
                init.with_body(Some(
                    json!({"format": format, "mtime": spec.mtime, "chunk_size": CHUNK_SIZE, "inputs": inputs})
                        .to_string()
                        .into(),
                ));
                let mut response = fetcher
                    .fetch(format!("http://archiver/build?job={id}"), Some(init))
                    .await?;
                let built: Value = response.json().await?;
                if response.status_code() != 200 {
                    return Err(Error::RustError(
                        built["error"]
                            .as_str()
                            .unwrap_or("the archive could not be built")
                            .into(),
                    ));
                }
                storage.put("built", built.to_string()).await?;
                built
            }
        };
        let size = built["size"].as_u64().unwrap_or(0);
        let hashes: Vec<String> =
            serde_json::from_value(built["chunks"].clone()).unwrap_or_default();
        let output = spec.output.clone().unwrap_or_default();
        if storage.get::<bool>("announced").await? != Some(true) {
            let mut lines = Lines::default();
            if *verbose {
                for input in &spec.inputs {
                    lines.out(if *format == terminal_worker_core::archive::Format::Zip {
                        format!("  adding: {}", input.name)
                    } else {
                        input.name.clone()
                    });
                }
            }
            self.update(id, lines, Some(json!({"done": 0, "total": hashes.len()})))
                .await?;
            storage.put("announced", true).await?;
        }
        // Resume the upload begun before, if what is stored of it is what was built now.
        let mut upload = storage.get::<String>("upload").await?;
        let mut committed = Vec::new();
        if let Some(previous) = &upload {
            let stored = self
                .ask(
                    Method::Get,
                    &format!("/internal/job/chunks?id={id}&upload={previous}"),
                    None,
                )
                .await?;
            let same = stored["exists"] == true
                && storage.get::<u64>("upload_size").await? == Some(size)
                && stored["chunks"].as_array().into_iter().flatten().all(|c| {
                    let index = c[0].as_u64().unwrap_or(u64::MAX) as usize;
                    hashes.get(index).map(String::as_str) == c[1].as_str()
                });
            if same {
                committed = stored["chunks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c[0].as_u64())
                    .collect();
            } else {
                self.abort_upload(id).await?;
                upload = None;
            }
        }
        let upload = match upload {
            Some(upload) => upload,
            None => {
                let begin = self
                    .ask(
                        Method::Post,
                        &format!("/internal/job/begin?id={id}"),
                        Some(json!({"path": output, "size": size, "replace": spec.replace})),
                    )
                    .await?;
                if begin["skipped"] == true {
                    return Err(Error::RustError(format!(
                        "{output} appeared while the archive was made"
                    )));
                }
                let upload = begin["id"].as_str().unwrap_or_default().to_string();
                storage.put("upload", &upload).await?;
                storage.put("upload_size", size).await?;
                upload
            }
        };
        let count = size.div_ceil(CHUNK_SIZE);
        for index in 0..count {
            if committed.contains(&index) {
                continue;
            }
            let offset = index * CHUNK_SIZE;
            let length = CHUNK_SIZE.min(size - offset);
            let sha = hashes.get(index as usize).cloned();
            let key = chunk_key(&upload, index);
            self.store(
                &fetcher,
                &format!("/output?job={id}&offset={offset}&length={length}"),
                &key,
                length,
                sha.as_deref(),
            )
            .await?;
            self.ask(
                Method::Post,
                &format!("/internal/job/commit?id={id}"),
                Some(json!({"upload": upload, "index": index, "key": key, "sha256": sha})),
            )
            .await?;
            let state = self
                .update(
                    id,
                    Lines::default(),
                    Some(json!({"done": index + 1, "total": count})),
                )
                .await?;
            if state == "cancelling" || Date::now().as_millis() as f64 > deadline {
                return Ok(Step::More);
            }
        }
        self.ask(
            Method::Post,
            &format!("/internal/job/commit?id={id}"),
            Some(json!({"upload": upload, "complete": true})),
        )
        .await?;
        storage.delete("upload").await?;
        Ok(Step::Done)
    }
}
