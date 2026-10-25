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
use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use terminal_worker_core::CHUNK_SIZE;
use terminal_worker_core::archive::{Existing, Job as Command, member_path};
use terminal_worker_core::permissions::mode_text;
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
    mode: u32,
    target: String,
    action: String,
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
        if !container.running() {
            container.start(None)?;
        }
        let fetcher = container.get_tcp_port(PORT)?;
        for _ in 0..120 {
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
                    let directory = m["kind"] == "directory";
                    lines.out(format!(
                        "{} {:>9} {name}",
                        mode_text(m["mode"].as_u64().unwrap_or(0) as u16 & 0o777, directory),
                        m["size"].as_u64().unwrap_or(0)
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
            "CREATE TABLE IF NOT EXISTS plan(idx INTEGER PRIMARY KEY, name TEXT NOT NULL, size INTEGER NOT NULL, mode INTEGER NOT NULL, kind TEXT NOT NULL, target TEXT NOT NULL, action TEXT NOT NULL, done INTEGER NOT NULL)",
            None,
        )?;
        let planned = !self
            .rows(
                "SELECT idx, name, size, mode, kind, target, action FROM plan LIMIT 1",
                vec![],
            )?
            .is_empty();
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
        if spec.decisions.is_none()
            && !self.rows("SELECT idx, name, size, mode, kind, target, action FROM plan WHERE action = 'ask' LIMIT 1", vec![])?.is_empty()
        {
            return Ok(Step::Wait);
        }
        let decisions = spec.decisions.clone().unwrap_or_default();
        let total = self
            .rows(
                "SELECT idx, name, size, mode, kind, target, action FROM plan",
                vec![],
            )?
            .len();
        // A member that a restart interrupted starts again.
        self.abort_upload(id).await?;
        let mut lines = Lines::default();
        let mut errors = 0;
        loop {
            let Some(row) = self
                .rows("SELECT idx, name, size, mode, kind, target, action FROM plan WHERE done = 0 ORDER BY idx LIMIT 1", vec![])?
                .pop()
            else {
                break;
            };
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
                    match self.write(&fetcher, id, &row, &directory, replace).await? {
                        Ok(true) => {
                            if *verbose {
                                lines.out(if *zip {
                                    format!("  inflating: {}", row.target)
                                } else {
                                    row.name.clone()
                                });
                            }
                            let mode = row.mode & 0o755;
                            if row.mode != 0 && mode != 0o644 {
                                self.call(
                                    Method::Post,
                                    &format!("/internal/job/chmod?id={id}"),
                                    Some(json!({"path": row.target, "mode": mode})),
                                )
                                .await?;
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
            let done: usize = total - self.rows("SELECT idx, name, size, mode, kind, target, action FROM plan WHERE done = 0", vec![])?.len();
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
        // Directories get their modes last, deepest first, so that none stops its own filling.
        for row in self.rows("SELECT idx, name, size, mode, kind, target, action FROM plan WHERE action = 'mkdir' ORDER BY length(target) DESC", vec![])? {
            let mode = row.mode & 0o755;
            if row.mode != 0 && mode != 0o755 {
                self.call(Method::Post, &format!("/internal/job/chmod?id={id}"), Some(json!({"path": row.target, "mode": mode})))
                    .await?;
            }
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
        let mut rows: Vec<(u64, String, u64, u32, String, String, String)> = Vec::new();
        let mut targets = BTreeMap::new();
        let mut stripped_noted = false;
        for (index, member) in listing["members"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let name = member["name"].as_str().unwrap_or_default().to_string();
            let trimmed = name.trim_end_matches('/');
            if !wanted.is_empty()
                && !wanted.iter().any(|w| {
                    let w = w.trim_end_matches('/');
                    trimmed == w || trimmed.starts_with(&format!("{w}/"))
                })
            {
                continue;
            }
            let kind = member["kind"].as_str().unwrap_or("other").to_string();
            match kind.as_str() {
                "link" => {
                    lines.err(format!(
                        "{cmd}: skipping {name}: links are not supported here"
                    ));
                    continue;
                }
                "other" => {
                    lines.err(format!(
                        "{cmd}: skipping {name}: not a regular file or directory"
                    ));
                    continue;
                }
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
                member["mode"].as_u64().unwrap_or(0) as u32,
                kind,
                target,
                action.to_string(),
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
                "INSERT OR REPLACE INTO plan(idx, name, size, mode, kind, target, action, done) VALUES(?, ?, ?, ?, ?, ?, ?, 0)",
                vec![
                    SqlStorageValue::Integer(row.0 as i64),
                    row.1.as_str().into(),
                    SqlStorageValue::Integer(row.2 as i64),
                    SqlStorageValue::Integer(i64::from(row.3)),
                    row.4.as_str().into(),
                    row.5.as_str().into(),
                    row.6.as_str().into(),
                ],
            )?;
        }
        if rows.is_empty() {
            lines.err(format!("{cmd}: nothing to extract"));
        }
        let mut body = json!({
            "lines": lines.0.iter().map(|(e, l)| json!([e, l])).collect::<Vec<_>>(),
            "progress": {"done": 0, "total": rows.len()},
        });
        if !conflicts.is_empty() && spec.decisions.is_none() {
            body["question"] = json!({"replace": conflicts});
            body["state"] = json!("question");
            self.ask(
                Method::Post,
                &format!("/internal/job/update?id={id}"),
                Some(body),
            )
            .await?;
            return Ok(Step::Wait);
        }
        self.ask(
            Method::Post,
            &format!("/internal/job/update?id={id}"),
            Some(body),
        )
        .await?;
        Ok(Step::More)
    }

    /// Write one member, through an upload of its own: Ok(true) if written, Ok(false) if left
    /// as it was, Err with a message if refused.
    async fn write(
        &self,
        fetcher: &Fetcher,
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
        let (status, begin) = self
            .call(
                Method::Post,
                &format!("/internal/job/begin?id={id}"),
                Some(json!({"path": row.target, "size": row.size, "replace": replace})),
            )
            .await?;
        if status >= 400 {
            return Ok(Err(begin["error"]
                .as_str()
                .unwrap_or("cannot be written")
                .into()));
        }
        if begin["skipped"] == true {
            return Ok(Ok(false));
        }
        let upload = begin["id"].as_str().unwrap_or_default().to_string();
        self.state.storage().put("upload", &upload).await?;
        let count = row.size.div_ceil(CHUNK_SIZE);
        for index in 0..count {
            let offset = index * CHUNK_SIZE;
            let length = CHUNK_SIZE.min(row.size - offset);
            self.store(
                fetcher,
                &format!(
                    "/entry?job={id}&index={}&offset={offset}&length={length}",
                    row.idx
                ),
                &chunk_key(&upload, index),
                length,
                None,
            )
            .await?;
            self.ask(
                Method::Post,
                &format!("/internal/job/commit?id={id}"),
                Some(json!({"upload": upload, "index": index})),
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
        Ok(Ok(true))
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
                })
            })
            .collect();
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
            self.store(
                &fetcher,
                &format!("/output?job={id}&offset={offset}&length={length}"),
                &chunk_key(&upload, index),
                length,
                sha.as_deref(),
            )
            .await?;
            self.ask(
                Method::Post,
                &format!("/internal/job/commit?id={id}"),
                Some(json!({"upload": upload, "index": index, "sha256": sha})),
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
