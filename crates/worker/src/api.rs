//! The authority's requests: each decides and writes in one synchronous transaction, after any
//! awaiting (reading the body, hashing a password) is done. It never touches file bytes; the
//! Worker asks it whether it may, and tells it what it wrote.
use crate::db::{Db, File, etag, transact};
use crate::{auth, commands, jobs};
use serde::Deserialize;
use serde_json::{Value, json};
use terminal_worker_core::{
    CHUNK_SIZE, Listing, MAX_FILE, MAX_UPLOAD, mime, permissions::*, resolve,
};
use worker::*;

/// How long an upload may go without a new chunk before it is cancelled, in seconds.
pub const UPLOAD_TTL: u64 = 24 * 60 * 60;

pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub cookie: Option<String>,
    /// Whether the transaction's writes stand: by default, only when the reply is a success.
    pub commit: bool,
}
impl Reply {
    pub fn ok(body: Value) -> Self {
        Self {
            status: 200,
            body,
            cookie: None,
            commit: true,
        }
    }
    pub fn error(message: &str, status: u16) -> Self {
        Self {
            status,
            body: json!({"error":message}),
            cookie: None,
            commit: false,
        }
    }
    fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
    fn cookie(mut self, cookie: String) -> Self {
        self.cookie = Some(cookie);
        self
    }
    /// Keep the writes of an error reply, such as a cleanup.
    fn keep(mut self) -> Self {
        self.commit = true;
        self
    }
    pub fn response(self) -> Result<Response> {
        let mut response = Response::from_json(&self.body)?.with_status(self.status);
        if let Some(cookie) = self.cookie {
            response.headers_mut().set("Set-Cookie", &cookie)?;
        }
        Ok(response)
    }
}

/// An error reply whose status follows the message: guests are asked to sign in.
pub fn denied(message: &str, who: &Identity) -> Reply {
    let status = if commands::is_permission_error(message) {
        if who.id == GUEST { 401 } else { 403 }
    } else if message.contains("does not exist") {
        404
    } else if message.contains("already exists")
        || message.contains("with this name")
        || message.contains("This is a directory")
    {
        409
    } else {
        400
    };
    Reply::error(message, status)
}

fn run(state: &State, work: impl FnOnce(&mut Db) -> Reply + 'static) -> Result<Reply> {
    transact(state, move |db| {
        let reply = work(db);
        let commit = reply.commit;
        (reply, commit)
    })
}
fn read<T: 'static>(state: &State, work: impl FnOnce(&mut Db) -> T + 'static) -> Result<T> {
    transact(state, move |db| (work(db), false))
}

fn body_size(req: &Request, max: u64) -> bool {
    req.headers()
        .get("Content-Length")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<u64>().ok())
        .is_some_and(|n| n > 0 && n <= max)
}
macro_rules! acl {
    ($result:expr, $who:expr) => {
        match $result {
            Ok(value) => value,
            Err(message) => return denied(&message, $who),
        }
    };
}

pub async fn handle(req: &mut Request, env: &Env, state: &State) -> Result<Reply> {
    let url = req.url()?;
    let endpoint = url.path().to_string();
    let method = req.method();
    let key = auth::token_key(req);
    let https = url.scheme() == "https";
    let raw_path = url
        .query_pairs()
        .find(|(k, _)| k == "path")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| "/".into());
    let path = match resolve("/", &raw_path) {
        Ok(p) => p,
        Err(e) => return Ok(Reply::error(&e, 400)),
    };
    let flag = |name: &str| url.query_pairs().any(|(k, v)| k == name && v == "true");
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    };
    match (method.clone(), endpoint.as_str()) {
        (Method::Get, "/api/session") => run(state, move |db| {
            let mut view = auth::session_view(db, key.as_deref());
            let who = auth::identity(db, key.as_deref());
            view["can_write"] = json!(
                db.access(&who, &path, WRITE | EXEC)
                    .is_ok_and(|n| n.directory)
            );
            Reply::ok(view)
        }),
        (Method::Post, "/api/login") => login(req, env, state, key, https).await,
        (Method::Post, "/api/logout") => run(state, move |db| {
            if let Some(key) = &key {
                db.delete_session(key);
            }
            Reply::ok(json!({"writable":false,"username":"guest"}))
                .cookie(auth::cookie(https, "", 0))
        }),
        (Method::Post, "/api/sudo") => sudo(req, env, state, key).await,
        (Method::Post, "/api/passwd") => passwd(req, env, state, key, https).await,
        (Method::Post, "/api/command") => {
            if !body_size(req, 1 << 20) {
                return Ok(Reply::error(
                    "Command is too long or has an unknown length",
                    400,
                ));
            }
            #[derive(Deserialize)]
            #[serde(untagged)]
            enum Body {
                /// Commands run one after another, each in its own transaction, as xargs runs
                /// them: a failure does not stop the next.
                Batch {
                    batch: Vec<commands::Command>,
                },
                One(commands::Command),
            }
            let commands = match req.json::<Body>().await {
                Ok(Body::One(command)) => {
                    let reply = run(state, move |db| command_reply(db, key.as_deref(), command))?;
                    kick(env, &reply.body).await;
                    return Ok(reply);
                }
                Ok(Body::Batch { batch }) if batch.len() <= BATCH_MAX => batch,
                Ok(Body::Batch { .. }) => {
                    return Ok(Reply::error(
                        &format!("A batch runs at most {BATCH_MAX} commands"),
                        400,
                    ));
                }
                Err(_) => return Ok(Reply::error("Invalid command request", 400)),
            };
            let mut results = Vec::new();
            for command in commands {
                let key = key.clone();
                let reply = run(state, move |db| command_reply(db, key.as_deref(), command))?;
                kick(env, &reply.body).await;
                let mut result = reply.body;
                result["status"] = json!(reply.status);
                results.push(result);
            }
            Ok(Reply::ok(json!({ "results": results })))
        }
        (Method::Get, "/api/stat") => {
            let enter = flag("enter");
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                let n = acl!(db.access(&who, &path, if enter { EXEC } else { 0 }), &who);
                if enter && !n.directory {
                    return Reply::error("Not a directory", 400);
                }
                let mut value = json!(commands::entry(db, &path, &n));
                value["writable"] = json!(who.allows(&n, WRITE));
                value["readable"] = json!(who.allows(&n, READ));
                Reply::ok(value)
            })
        }
        (Method::Get, "/api/ls") => run(state, move |db| {
            let who = auth::identity(db, key.as_deref());
            let n = acl!(db.access(&who, &path, READ | EXEC), &who);
            if !n.directory {
                return Reply::error("Not a directory", 400);
            }
            let entries = commands::entries(db, &path, &db.children(&path));
            Reply::ok(json!(Listing { path, entries }))
        }),
        (Method::Post, "/api/mkdir") => run(state, move |db| {
            let who = auth::identity(db, key.as_deref());
            // Keep the original API's mkdir -p contract; CLI mkdir supports explicit -p.
            acl!(db.mkdir(&who, &path, true), &who);
            Reply::ok(json!({"path":path}))
        }),
        (Method::Delete, "/api/file") => {
            let recursive = flag("recursive");
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                if path == "/" {
                    return Reply::error("Cannot remove the root directory", 403);
                }
                acl!(db.remove(&who, &path, recursive), &who);
                Reply::ok(json!({"path":path}))
            })
        }
        (Method::Get, "/internal/read") => run(state, move |db| {
            let who = auth::identity(db, key.as_deref());
            let node = acl!(db.access(&who, &path, READ), &who);
            let Some(id) = node.file else {
                return Reply::error("This is a directory", 400);
            };
            let Some(file) = db.file(&id) else {
                return Reply::error("File content does not exist", 404);
            };
            let chunks: Vec<_> = db
                .chunks(&id)
                .into_iter()
                .map(|c| json!([c.idx, c.key]))
                .collect();
            Reply::ok(json!({
                "file": id, "size": file.size, "chunk_size": file.chunk_size,
                "state": file.state, "etag": file.etag(), "mime": mime(&path), "chunks": chunks,
                "readable": db.readable(&file),
            }))
        }),
        (Method::Post, "/internal/upload/begin") => {
            #[derive(Deserialize)]
            struct Begin {
                size: u64,
                chunk_size: u64,
                #[serde(default)]
                overwrite: bool,
                if_match: Option<String>,
                if_none_match: Option<String>,
            }
            let Ok(begin) = req.json::<Begin>().await else {
                return Ok(Reply::error("Invalid upload request", 400));
            };
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                begin_upload(
                    db,
                    &who,
                    &path,
                    Begun {
                        size: begin.size,
                        chunk_size: begin.chunk_size,
                        limit: MAX_UPLOAD,
                        sha256: None,
                        overwrite: begin.overwrite,
                        if_match: begin.if_match,
                        if_none_match: begin.if_none_match,
                        mode: None,
                    },
                )
            })
        }
        (Method::Post, "/api/uploads") => {
            #[derive(Deserialize)]
            struct Start {
                size: u64,
                sha256: Option<String>,
                #[serde(default)]
                overwrite: bool,
                if_match: Option<String>,
            }
            if !body_size(req, 4096) {
                return Ok(Reply::error("Invalid upload request", 400));
            }
            let Ok(start) = req.json::<Start>().await else {
                return Ok(Reply::error("Invalid upload request", 400));
            };
            if start
                .sha256
                .as_ref()
                .is_some_and(|h| h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return Ok(Reply::error(
                    "The SHA-256 must be 64 hexadecimal digits",
                    400,
                ));
            }
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                begin_upload(
                    db,
                    &who,
                    &path,
                    Begun {
                        size: start.size,
                        chunk_size: CHUNK_SIZE,
                        limit: MAX_FILE,
                        sha256: start.sha256,
                        overwrite: start.overwrite,
                        if_match: start.if_match,
                        if_none_match: None,
                        mode: None,
                    },
                )
            })
        }
        (Method::Get, "/api/uploads") => {
            let id = param("id");
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                let Some(file) = db.file(&id).filter(|f| !f.complete()) else {
                    return Reply::error(
                        "Upload does not exist; it was completed, cancelled or has expired",
                        404,
                    );
                };
                if file.uploader != who.id {
                    return denied(
                        "Permission denied: this upload belongs to another account",
                        &who,
                    );
                }
                let committed: Vec<u64> = db.chunks(&id).into_iter().map(|c| c.idx).collect();
                Reply::ok(json!({
                    "id": id, "path": file.target, "size": file.size, "chunk_size": file.chunk_size,
                    "sha256": file.sha256, "committed": committed,
                }))
            })
        }
        (Method::Post, "/api/uploads/complete") => {
            let id = param("id");
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                finish_upload(db, &who, &id, true)
            })
        }
        (Method::Delete, "/api/uploads") => {
            let id = param("id");
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                let Some(file) = db.file(&id).filter(|f| !f.complete()) else {
                    return Reply::ok(json!({"cancelled":true}));
                };
                if file.uploader != who.id && who.id != 0 {
                    return denied(
                        "Permission denied: this upload belongs to another account",
                        &who,
                    );
                }
                cancel(db, &file);
                Reply::ok(json!({"cancelled":true}))
            })
        }
        (Method::Get, "/internal/upload/chunk") => {
            let id = param("id");
            let index = param("index").parse::<u64>().ok();
            let length = param("length").parse::<u64>().ok();
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                let Some(file) = db.file(&id).filter(|f| !f.complete()) else {
                    return Reply::error("Upload was cancelled or has expired", 409);
                };
                if file.uploader != who.id {
                    return denied(
                        "Permission denied: this upload belongs to another account",
                        &who,
                    );
                }
                let Some(index) = index.filter(|i| *i < file.chunk_count()) else {
                    return Reply::error("Chunk index is out of range", 400);
                };
                if length != Some(file.chunk_len(index)) {
                    return Reply::error(
                        &format!("Chunk {index} must be {} bytes", file.chunk_len(index)),
                        400,
                    );
                }
                let (path, _) = match destination(db, &who, &file) {
                    Ok(found) => found,
                    Err(reply) => return reply,
                };
                Reply::ok(json!({"key": chunk_key(&id, index), "mime": mime(&path)}))
            })
        }
        (Method::Post, "/internal/upload/commit") => {
            #[derive(Deserialize)]
            struct Commit {
                id: String,
                index: Option<u64>,
                key: Option<String>,
                sha256: Option<String>,
                #[serde(default)]
                complete: bool,
            }
            let Ok(commit) = req.json::<Commit>().await else {
                return Ok(Reply::error("Invalid upload request", 400));
            };
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                commit_upload(
                    db,
                    &who,
                    &commit.id,
                    commit.index.zip(commit.key),
                    commit.sha256,
                    commit.complete,
                )
            })
        }
        (Method::Post, "/internal/upload/abort") => {
            #[derive(Deserialize)]
            struct Abort {
                id: String,
            }
            let Ok(abort) = req.json::<Abort>().await else {
                return Ok(Reply::error("Invalid upload request", 400));
            };
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                let Some(file) = db.file(&abort.id) else {
                    return Reply::ok(json!({"cancelled":true}));
                };
                if file.uploader != who.id && who.id != 0 {
                    return denied(
                        "Permission denied: this upload belongs to another account",
                        &who,
                    );
                }
                cancel(db, &file);
                Reply::ok(json!({"cancelled":true}))
            })
        }
        (_, endpoint)
            if endpoint.starts_with("/api/jobs") || endpoint.starts_with("/internal/job") =>
        {
            let id = param("id");
            let since = param("since").parse::<u64>().unwrap_or(0);
            let body: Value = if matches!(method, Method::Post) {
                req.json().await.unwrap_or_default()
            } else {
                Value::Null
            };
            let reply = run(state, {
                let endpoint = endpoint.to_string();
                let method = method.clone();
                let upload = param("upload");
                let file = param("file");
                move |db| {
                    let who = auth::identity(db, key.as_deref());
                    job_reply(
                        db, &who, &method, &endpoint, &id, since, &upload, &file, body,
                    )
                }
            })?;
            kick(env, &reply.body).await;
            Ok(reply)
        }
        _ => Ok(Reply::error(
            "Unknown endpoint or unsupported request method",
            404,
        )),
    }
}

/// The jobs' endpoints: `/api/jobs` for their owners, `/internal/job` for their runners, which
/// act as the job's account.
#[allow(clippy::too_many_arguments)]
fn job_reply(
    db: &mut Db,
    who: &Identity,
    method: &Method,
    endpoint: &str,
    id: &str,
    since: u64,
    upload: &str,
    file: &str,
    body: Value,
) -> Reply {
    use crate::db::JobColumn;
    if (method, endpoint) == (&Method::Get, "/api/jobs") && id.is_empty() {
        let jobs: Vec<Value> = db
            .jobs_of(who.id, 20)
            .iter()
            .map(|job| {
                let mut view = jobs::view(db, job, u64::MAX);
                view["command"] = jobs::Spec::of(job)
                    .map(|s| json!(s.command))
                    .unwrap_or_default();
                view
            })
            .collect();
        let uploads: Vec<Value> = db
            .uploads_of(who.id)
            .iter()
            .map(|f| json!({"id": f.id, "path": f.target, "size": f.size, "readable": db.readable(f)}))
            .collect();
        return Reply::ok(json!({"jobs": jobs, "uploads": uploads}));
    }
    let Some(job) = db.job(id) else {
        return Reply::error("Job does not exist", 404);
    };
    if endpoint.starts_with("/api/jobs") {
        if who.id != job.owner && who.id != 0 {
            return denied(
                "Permission denied: this job belongs to another account",
                who,
            );
        }
        return match (method, endpoint) {
            (Method::Get, "/api/jobs") => Reply::ok(jobs::view(db, &job, since)),
            (Method::Delete, "/api/jobs") if job.finished() => {
                Reply::ok(json!({"state": job.state}))
            }
            (Method::Delete, "/api/jobs") => {
                db.set_job(id, JobColumn::State, Some("cancelling"));
                Reply::ok(json!({"job": id, "state": "cancelling"}))
            }
            (Method::Post, "/api/jobs/answer") if job.state == "question" => {
                let Ok(mut spec) = jobs::Spec::of(&job) else {
                    return Reply::error("The job cannot be read", 500);
                };
                spec.decisions = serde_json::from_value(body["decisions"].clone()).ok();
                if spec.decisions.is_none() {
                    return Reply::error("Answer with {\"decisions\": {PATH: true|false}}", 400);
                }
                db.set_job(id, JobColumn::Spec, Some(&json!(spec).to_string()));
                db.set_job(id, JobColumn::Question, None);
                db.set_job(id, JobColumn::State, Some("queued"));
                Reply::ok(json!({"job": id, "state": "queued"}))
            }
            (Method::Post, "/api/jobs/answer") => {
                Reply::error("The job is not waiting for an answer", 409)
            }
            _ => Reply::error("Unknown endpoint or unsupported request method", 404),
        };
    }
    // The runner's requests, as the job's account.
    if (method, endpoint) == (&Method::Get, "/internal/job") {
        return Reply::ok(json!(job));
    }
    if (method, endpoint) == (&Method::Post, "/internal/job/update") {
        if let Some(lines) = body["lines"].as_array() {
            let lines: Vec<(bool, String)> = lines
                .iter()
                .filter_map(|l| Some((l[0].as_bool()?, l[1].as_str()?.to_string())))
                .collect();
            db.add_lines(id, &lines);
        }
        if let Some(progress) = body.get("progress") {
            db.set_job(id, JobColumn::Progress, Some(&progress.to_string()));
        }
        if let Some(question) = body.get("question") {
            db.set_job(
                id,
                JobColumn::Question,
                (!question.is_null())
                    .then(|| question.to_string())
                    .as_deref(),
            );
        }
        // A job being cancelled stays so; the runner finishes it.
        if let Some(state) = body["state"].as_str()
            && !job.finished()
            && job.state != "cancelling"
        {
            db.set_job(id, JobColumn::State, Some(state));
        }
        return Reply::ok(json!({"state": db.job(id).map(|j| j.state)}));
    }
    if (method, endpoint) == (&Method::Post, "/internal/job/finish") {
        let state = body["state"].as_str().unwrap_or("done");
        jobs::finish(
            db,
            id,
            if job.state == "cancelling" {
                "cancelled"
            } else {
                state
            },
        );
        return Reply::ok(json!({"finished": true}));
    }
    if job.finished() {
        return Reply::error("The job has ended", 409);
    }
    let actor = match jobs::actor(db, &job) {
        Ok(actor) => actor,
        Err(e) => return Reply::error(&e, 403),
    };
    match (method, endpoint) {
        (Method::Get, "/internal/job/file") => {
            if !db.pinned(id, file) {
                return Reply::error("The job does not read that file", 403);
            }
            let Some(f) = db.file(file) else {
                return Reply::error("File content does not exist", 404);
            };
            let chunks: Vec<Value> = db
                .chunks(file)
                .into_iter()
                .map(|c| json!([c.idx, c.key]))
                .collect();
            Reply::ok(json!({"size": f.size, "chunk_size": f.chunk_size, "chunks": chunks}))
        }
        (Method::Post, "/internal/job/exists") => {
            let mut existing = serde_json::Map::new();
            for path in body["paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                // What lies where the job may not look is never told, not even whether it is
                // there: writing it fails, as for a path that is not.
                if db.traverse(&actor, path).is_ok()
                    && let Some(node) = db.node(path)
                {
                    existing.insert(
                        path.into(),
                        json!(if node.directory { "directory" } else { "file" }),
                    );
                }
            }
            Reply::ok(json!({"existing": existing}))
        }
        (Method::Post, "/internal/job/mkdir") => {
            let path = body["path"].as_str().unwrap_or_default();
            acl!(db.mkdir(&actor, path, true), &actor);
            Reply::ok(json!({"path": path}))
        }
        (Method::Post, "/internal/job/begin") => {
            let path = body["path"].as_str().unwrap_or_default().to_string();
            let replace = body["replace"] == true;
            // Only where the job may look is it told what is there.
            acl!(db.traverse(&actor, &path), &actor);
            if let Some(node) = db.node(&path) {
                if node.directory {
                    return Reply::error(
                        &format!("{path}: a directory with this name already exists"),
                        409,
                    );
                }
                if !replace {
                    return Reply::ok(json!({"skipped": true}));
                }
            }
            begin_upload(
                db,
                &actor,
                &path,
                Begun {
                    size: body["size"].as_u64().unwrap_or(0),
                    chunk_size: CHUNK_SIZE,
                    limit: MAX_FILE,
                    sha256: None,
                    overwrite: replace,
                    if_match: None,
                    if_none_match: None,
                    mode: body["mode"].as_u64().map(|m| (m & 0o777) as u16),
                },
            )
        }
        (Method::Post, "/internal/job/commit") => commit_upload(
            db,
            &actor,
            body["upload"].as_str().unwrap_or_default(),
            body["index"]
                .as_u64()
                .zip(body["key"].as_str().map(String::from)),
            body["sha256"].as_str().map(String::from),
            body["complete"] == true,
        ),
        (Method::Post, "/internal/job/abort") => {
            if let Some(file) = db.file(body["upload"].as_str().unwrap_or_default())
                && file.uploader == actor.id
                && !file.complete()
            {
                cancel(db, &file);
            }
            Reply::ok(json!({"cancelled": true}))
        }
        (Method::Get, "/internal/job/chunks") => {
            let hashes: Vec<Value> = db
                .chunk_hashes(upload)
                .into_iter()
                .map(|(i, h)| json!([i, h]))
                .collect();
            Reply::ok(
                json!({"chunks": hashes, "exists": db.file(upload).is_some_and(|f| !f.complete())}),
            )
        }
        (Method::Post, "/internal/job/chmod") => {
            let path = body["path"].as_str().unwrap_or_default();
            let mode = format!("{:03o}", body["mode"].as_u64().unwrap_or(0o644) & 0o777);
            acl!(db.chmod(&actor, path, &mode, false), &actor);
            Reply::ok(json!({"path": path}))
        }
        _ => Reply::error("Unknown endpoint or unsupported request method", 404),
    }
}

/// The most commands one batch runs.
const BATCH_MAX: usize = 100;

/// Run one command as the session `key`, elevated if it asks for sudo.
fn command_reply(db: &mut Db, key: Option<&str>, mut command: commands::Command) -> Reply {
    let who = auth::identity(db, key);
    let actor = acl!(auth::elevated(db, key, command.sudo), &who);
    // `~` is the signed-in account's home, also under sudo, as the shell expands it before sudo.
    command.home = db.user_by_id(who.id).map(|(_, user)| user.home);
    let result = if command
        .args
        .first()
        .is_some_and(|c| jobs::ARCHIVERS.contains(&c.as_str()))
    {
        jobs::start(db, &actor, who.id, &command)
    } else {
        commands::execute(db, &actor, command)
    };
    match result {
        Ok(value) => Reply::ok(value),
        Err(e) => denied(&e, &who),
    }
}

/// Wake the runner of the job a reply started or changed, if any.
pub async fn kick(env: &Env, reply: &Value) {
    let Some(job) = reply["job"].as_str() else {
        return;
    };
    let woken = async {
        env.durable_object("ARCHIVES")?
            .id_from_name(job)?
            .get_stub()?
            .fetch_with_str(&format!("https://runner/start?job={job}"))
            .await
    }
    .await;
    if let Err(e) = woken {
        console_error!("cannot start the runner of job {}: {}", job, e);
    }
}

/// A new R2 object for chunk `index` of `file`. Each write has its own, so a chunk sent again,
/// or late, never overwrites one already committed, which readers and copies may be reading.
pub fn chunk_key(file: &str, index: u64) -> String {
    format!("chunks/{file}/{index}-{}", &auth::random_token()[..16])
}
/// Whether `key` is an object `chunk_key` named for chunk `index` of `file`.
fn chunk_of(key: &str, file: &str, index: u64) -> bool {
    key.strip_prefix(&format!("chunks/{file}/{index}-"))
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// An upload to begin: `size` bytes, in chunks of `chunk_size`.
struct Begun {
    size: u64,
    chunk_size: u64,
    /// The most bytes this way of uploading may write.
    limit: u64,
    sha256: Option<String>,
    overwrite: bool,
    if_match: Option<String>,
    if_none_match: Option<String>,
    /// The mode a new file gets, if not 644, as one extracted from an archive.
    mode: Option<u16>,
}

/// Start writing to `path`: a new file appears at once, still uploading, and can be read as it
/// arrives; an overwritten file keeps showing its old content until the upload completes.
fn begin_upload(db: &mut Db, who: &Identity, path: &str, begin: Begun) -> Reply {
    let Begun {
        size,
        chunk_size,
        limit,
        sha256,
        overwrite,
        if_match,
        if_none_match,
        mode,
    } = begin;
    acl!(db.traverse(who, path), who);
    if path == "/" {
        return Reply::error("Cannot write to the root directory", 400);
    }
    if size > limit {
        return Reply::error(
            &format!("Maximum file size is {} MB", limit / 1_000_000),
            413,
        );
    }
    let old = db.node(path);
    // The same file, to the same path, by the same account: resume its upload.
    if sha256.is_some()
        && let Some(file) = old
            .as_ref()
            .and_then(|n| n.file.as_deref())
            .and_then(|id| db.file(id))
        && !file.complete()
        && file.uploader == who.id
        && file.size == size
        && file.sha256 == sha256
    {
        let committed: Vec<u64> = db.chunks(&file.id).into_iter().map(|c| c.idx).collect();
        return Reply::ok(json!({
            "id": file.id, "created": true, "resumed": true, "size": size,
            "chunk_size": file.chunk_size, "committed": committed,
            "mode": old.as_ref().map(|n| n.mode),
        }));
    }
    if old.is_none() && if_match.is_some() {
        return Reply::error("File was deleted or moved; save it as a new file", 412);
    }
    let base = if let Some(old) = &old {
        acl!(db.access(who, path, WRITE), who);
        if old.directory {
            return Reply::error("A directory with this name already exists", 409);
        }
        if if_none_match.as_deref() == Some("*") {
            return Reply::error("File was created by another session; reopen it", 412);
        }
        if let Some(expected) = &if_match
            && expected != "*"
            && Some(expected.as_str()) != old.file.as_deref().map(etag).as_deref()
        {
            return Reply::error(
                "File changed in another session; reopen it or download your draft to avoid overwriting changes",
                412,
            );
        }
        if !overwrite {
            return Reply::error("File already exists; rename it before uploading", 409);
        }
        old.file.clone()
    } else {
        acl!(db.writable_parent(who, path), who);
        None
    };
    let now = auth::now();
    let id = db.create_file(
        size,
        chunk_size,
        sha256.as_deref(),
        who.id,
        path,
        base.as_deref(),
        now,
    );
    db.set_expiry("upload", &id, now + UPLOAD_TTL);
    // A new file has its mode from the start, so that nobody reads it as it arrives who may not
    // once it is there; an overwritten one keeps its own.
    let mode = match &old {
        Some(old) => old.mode,
        None => {
            let mode = mode.unwrap_or(0o644);
            db.set_node(path, &Node::file(who.id, who.group, mode, id.clone(), size));
            mode
        }
    };
    Reply::ok(json!({
        "id": id, "created": old.is_none(), "size": size, "chunk_size": chunk_size.max(1),
        "mode": mode,
    }))
}

/// Record `chunk`, an index and the object written for it, as part of upload `id`, and complete
/// the upload if asked. A chunk is committed once: what readers have seen of it never changes,
/// and an object written for nothing goes to the garbage.
fn commit_upload(
    db: &mut Db,
    who: &Identity,
    id: &str,
    chunk: Option<(u64, String)>,
    sha256: Option<String>,
    complete: bool,
) -> Reply {
    let Some((index, key)) = chunk else {
        return finish_upload(db, who, id, complete);
    };
    if !chunk_of(&key, id, index) {
        return Reply::error("Chunk key does not belong to this upload", 400);
    }
    let refused = match db.file(id) {
        None => Some(Reply::error("Upload was cancelled or has expired", 409)),
        Some(file) if file.uploader != who.id => Some(denied(
            "Permission denied: this upload belongs to another account",
            who,
        )),
        Some(file) if file.complete() => Some(Reply::error("Upload is already complete", 409)),
        Some(file) if index >= file.chunk_count() => {
            Some(Reply::error("Chunk index is out of range", 400))
        }
        Some(file) => match destination(db, who, &file) {
            Err(reply) => Some(reply),
            Ok(_) => match db.committed_hash(id, index) {
                // Sent again: the chunk committed first stays.
                Some(hash) if hash.is_none() || sha256.is_none() || hash == sha256 => {
                    Some(Reply::ok(json!({"committed":index})))
                }
                Some(_) => Some(Reply::error(
                    &format!("Chunk {index} was already committed with other content"),
                    409,
                )),
                None => None,
            },
        },
    };
    if let Some(reply) = refused {
        db.discard(&key);
        return reply.keep();
    }
    let now = auth::now();
    db.add_chunk(id, index, &key, sha256.as_deref(), now);
    db.set_expiry("upload", id, now + UPLOAD_TTL);
    if !complete {
        return Reply::ok(json!({"committed":index}));
    }
    finish_upload(db, who, id, true)
}
/// Complete upload `id` if asked and every chunk is committed.
fn finish_upload(db: &mut Db, who: &Identity, id: &str, complete: bool) -> Reply {
    let Some(file) = db.file(id) else {
        return Reply::error("Upload was cancelled or has expired", 409);
    };
    if file.uploader != who.id {
        return denied(
            "Permission denied: this upload belongs to another account",
            who,
        );
    }
    if file.complete() {
        return Reply::error("Upload is already complete", 409);
    }
    if !complete {
        return Reply::ok(json!({"committed":null}));
    }
    let (path, node) = match destination(db, who, &file) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    if db.chunks(id).len() as u64 != file.chunk_count() {
        return Reply::error("Upload is missing chunks", 409);
    }
    if file.base.is_some() {
        db.set_node(
            &path,
            &Node {
                file: Some(id.into()),
                size: file.size,
                ..node
            },
        );
    }
    db.finish_file(id);
    Reply::ok(json!({"path":path,"size":file.size,"etag":file.etag()}))
        .status(if file.base.is_none() { 201 } else { 200 })
}

/// Where upload `file` writes, and the node there, if `who` may still write it: every chunk and
/// the completion ask again, as permissions may have changed since the upload began. A new file
/// is written at its node, wherever it was moved, while `who` may reach it, and owns it, as one
/// who created a file goes on writing it, or may write it. An overwrite is written while `who`
/// may write the file at its target, and that file is still the one it replaces; once it is not,
/// the upload can never complete, and is dropped.
fn destination(
    db: &mut Db,
    who: &Identity,
    file: &File,
) -> std::result::Result<(String, Node), Reply> {
    let path = match &file.base {
        Some(_) => file.target.clone(),
        None => db
            .showing(&file.id)
            .pop()
            .ok_or_else(|| Reply::error("Upload was cancelled or has expired", 409))?,
    };
    // What is there is not for those who may not reach it.
    db.traverse(who, &path)
        .map_err(|message| denied(&message, who))?;
    let denial = || denied(&format!("Permission denied: {path}"), who);
    match (&file.base, db.node(&path)) {
        (Some(base), Some(node)) if node.file.as_ref() == Some(base) => {
            if !who.allows(&node, WRITE) {
                return Err(denial());
            }
            Ok((path, node))
        }
        (Some(_), _) => {
            db.drop_file(&file.id);
            Err(Reply::error(
                "File changed in another session during the upload; reopen it",
                412,
            )
            .keep())
        }
        (None, Some(node)) if node.owner == who.id || who.allows(&node, WRITE) => Ok((path, node)),
        (None, _) => Err(denial()),
    }
}

/// Abandon an upload: a new file disappears, an overwritten one keeps its old content.
pub fn cancel(db: &mut Db, file: &File) {
    if file.base.is_none() {
        for path in db.showing(&file.id) {
            db.delete_node(&path);
        }
    }
    // Deleting the last node showing it may have freed it already.
    if db.file(&file.id).is_some() {
        db.drop_file(&file.id);
    }
}

async fn login(
    req: &mut Request,
    env: &Env,
    state: &State,
    key: Option<String>,
    https: bool,
) -> Result<Reply> {
    if auth::limited(req, env, "login").await? {
        return Ok(Reply::error(
            "Too many attempts; please try again later",
            429,
        ));
    }
    #[derive(Deserialize)]
    struct Login {
        #[serde(default = "root_user")]
        username: String,
        #[serde(default)]
        password: String,
        #[serde(default)]
        switch: bool,
        #[serde(default)]
        sudo: bool,
    }
    fn root_user() -> String {
        "root".into()
    }
    if !body_size(req, 2048) {
        return Ok(Reply::error(
            "Login request is too large or has an unknown length",
            400,
        ));
    }
    let Ok(login) = req.json::<Login>().await else {
        return Ok(Reply::error("Invalid login request", 400));
    };
    let (who, user, root, sudo) = read(state, {
        let key = key.clone();
        let name = login.username.clone();
        let sudo = login.sudo;
        move |db| {
            let sudo = if sudo {
                auth::elevated(db, key.as_deref(), true).map(|_| ())
            } else {
                Ok(())
            };
            (
                auth::identity(db, key.as_deref()),
                db.user(&name),
                db.user("root"),
                sudo,
            )
        }
    })?;
    // Explicit sudo -u switches only after the caller has authorized sudo.
    if let Err(message) = sudo {
        return Ok(denied(&message, &who));
    }
    // Run the same KDF for nonexistent users to avoid a username timing oracle.
    let verified = if login.sudo || login.switch && who.id == 0 {
        true
    } else {
        let Some(account) = user.as_ref().or(root.as_ref()) else {
            return Ok(Reply::error("Root account is missing", 500));
        };
        auth::verify(&login.password, account, env).await?
    };
    let Some(user) = user.filter(|u| (!u.locked || who.id == 0 && login.switch) && verified) else {
        return Ok(Reply::error(
            "Incorrect username or password, or the account is locked",
            401,
        ));
    };
    if user.locked {
        return Ok(Reply::error(
            "Account is locked; use usermod -U to unlock it first",
            403,
        ));
    }
    run(state, move |db| {
        // The password verified above must still be the account's, and the account unlocked:
        // a session issued after a lock would come back when the account is unlocked.
        if db
            .user_by_id(user.id)
            .is_none_or(|(_, u)| u.hash != user.hash || u.locked)
        {
            return Reply::error("Account changed during sign-in; please try again", 409);
        }
        let (body, token) = auth::issue(db, key.as_deref(), user.id);
        Reply::ok(body).cookie(auth::cookie(https, &token, auth::TTL))
    })
}

async fn sudo(req: &mut Request, env: &Env, state: &State, key: Option<String>) -> Result<Reply> {
    let (who, can_sudo, account) = read(state, {
        let key = key.clone();
        move |db| {
            let who = auth::identity(db, key.as_deref());
            let can_sudo = db.can_sudo(&who);
            let account = db.user_by_id(who.id).map(|(_, u)| u);
            (who, can_sudo, account)
        }
    })?;
    if who.id == GUEST {
        return Ok(Reply::error("Sign in first", 401));
    }
    if !can_sudo {
        return Ok(Reply::error(
            "Current account is not in the wheel group",
            403,
        ));
    }
    if !body_size(req, 2048) {
        return Ok(Reply::error("Invalid sudo request", 400));
    }
    let Ok(value) = req.json::<Value>().await else {
        return Ok(Reply::error("Invalid sudo request", 400));
    };
    let revoke = value["revoke"] == true;
    if !revoke && who.id != 0 {
        if auth::limited(req, env, "sudo").await? {
            return Ok(Reply::error(
                "Too many attempts; please try again later",
                429,
            ));
        }
        let Some(account) = &account else {
            return Ok(Reply::error("Sign in first", 401));
        };
        if !auth::verify(value["password"].as_str().unwrap_or(""), account, env).await? {
            return Ok(Reply::error("Incorrect account password", 401));
        }
    }
    run(state, move |db| {
        if let Some(key) = &key
            && let Some(mut session) = db.session(key)
        {
            session.sudo_until = if revoke { 0 } else { auth::now() + 300 };
            db.set_session(key, &session);
        }
        Reply::ok(json!({"sudo_active":!revoke}))
    })
}

async fn passwd(
    req: &mut Request,
    env: &Env,
    state: &State,
    key: Option<String>,
    https: bool,
) -> Result<Reply> {
    let who = read(state, {
        let key = key.clone();
        move |db| auth::identity(db, key.as_deref())
    })?;
    if who.id == GUEST {
        return Ok(Reply::error("Sign in first", 401));
    }
    if auth::limited(req, env, "passwd").await? {
        return Ok(Reply::error(
            "Too many attempts; please try again later",
            429,
        ));
    }
    if !body_size(req, 4096) {
        return Ok(Reply::error("Invalid password request", 400));
    }
    #[derive(Deserialize)]
    struct Password {
        #[serde(default)]
        username: Option<String>,
        password: String,
        #[serde(default)]
        current_password: String,
        #[serde(default)]
        sudo: bool,
        #[serde(default)]
        force: bool,
    }
    let Ok(value) = req.json::<Password>().await else {
        return Ok(Reply::error("Invalid password request", 400));
    };
    let checked = read(state, {
        let key = key.clone();
        let name = value.username.clone();
        let sudo = value.sudo;
        let who = who.clone();
        move |db| {
            let actor = auth::elevated(db, key.as_deref(), sudo)?;
            let name = name.unwrap_or_else(|| db.username(who.id));
            let target = db.user(&name).ok_or("User does not exist")?;
            Ok::<_, String>((actor, name, target))
        }
    })?;
    let (actor, name, target) = match checked {
        Ok(checked) => checked,
        Err(message) => return Ok(denied(&message, &who)),
    };
    if actor.id != 0 && target.id != who.id {
        return Ok(Reply::error(
            "Only root can change another account's password",
            403,
        ));
    }
    if actor.id != 0 && !auth::verify(&value.current_password, &target, env).await? {
        return Ok(Reply::error("Incorrect current password", 401));
    }
    if value.password.is_empty() || value.password.len() > 512 {
        return Ok(Reply::error(
            "The new password cannot be empty and must not exceed 512 bytes",
            400,
        ));
    }
    if !value.force && !auth::password_valid(&value.password) {
        return Ok(Reply::error(
            "Use at least 12 characters; explicitly use passwd --force to allow a short password",
            400,
        ));
    }
    let salt = auth::random_token();
    let hash = auth::hash(&value.password, &salt, env).await?;
    let sudo = value.sudo;
    run(state, move |db| {
        let Some(mut account) = db.user(&name).filter(|u| u.id == target.id) else {
            return Reply::error("User does not exist", 404);
        };
        // What was checked before hashing must still hold: the same signed-in account, acting as
        // it did, and, without root, the current password verified still the account's, which
        // is not locked meanwhile.
        let now = auth::identity(db, key.as_deref());
        let still = auth::elevated(db, key.as_deref(), sudo).is_ok_and(|a| a.id == actor.id);
        if now.id != who.id
            || !still
            || actor.id != 0 && (account.hash != target.hash || account.locked)
        {
            return Reply::error(
                "Account changed during the password change; please try again",
                409,
            );
        }
        account.salt = salt;
        account.hash = hash;
        account.locked = false;
        db.set_user(&name, &account);
        db.delete_sessions(account.id);
        if account.id == who.id {
            let (body, token) = auth::issue(db, key.as_deref(), account.id);
            return Reply::ok(body).cookie(auth::cookie(https, &token, auth::TTL));
        }
        Reply::ok(
            json!({"output":format!("Password updated for {name}; previous sessions revoked")}),
        )
    })
}
