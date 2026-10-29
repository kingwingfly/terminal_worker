//! The authority's requests: each decides and writes in one synchronous transaction, after any
//! awaiting (reading the body, hashing a password) is done. It never touches file bytes; the
//! Worker asks it whether it may, and tells it what it wrote.
use crate::db::{Db, File, etag, transact};
use crate::{auth, commands, devices, jobs, sound};
use serde::Deserialize;
use serde_json::{Value, json};
use terminal_worker_core::devices::{PRESENT, SOUND};
use terminal_worker_core::{
    CHUNK_SIZE, Listing, MAX_FILE, MAX_UPLOAD, mime, permissions::*, resolve,
};
use worker::*;

/// How long an upload may go without a new chunk before it is cancelled, in seconds.
pub const UPLOAD_TTL: u64 = 24 * 60 * 60;
/// The chunks of a recording (`stream`): small, as each is sent once it is whole, and lost if
/// its page goes first (about half a minute of speech at 64 kbit/s); not too small, as reading a
/// file takes an R2 request a chunk, of the Worker's subrequests (1000 a request).
pub const RECORDING_CHUNK: u64 = 256 * 1024;
/// The most a recording may be: as many chunks as one read can take, with room for what else it
/// asks (240 MiB, about eight hours at 64 kbit/s).
pub const RECORDING_MAX: u64 = RECORDING_CHUNK * 960;
/// How long a recording's page may go unheard (chunks, its hold) before the recording is
/// completed with what arrived, in seconds.
pub const RECORDING_IDLE: u64 = 10 * 60;
/// What a recording that reached the most it may be is told: it takes no more.
const RECORDING_FULL: &str =
    "The recording reached the most a recording may be (240 MB, about eight hours)";

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
        || message.contains("File exists")
        || message.contains("Device or resource busy")
    {
        409
    } else {
        400
    };
    Reply::error(message, status)
}

pub fn run(state: &State, work: impl FnOnce(&mut Db) -> Reply + 'static) -> Result<Reply> {
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
/// `path` physically for `who`, a link at its end followed if `follow`, or the error reply.
macro_rules! real {
    ($db:expr, $who:expr, $path:expr, $follow:expr) => {
        acl!($db.physical($who, $path, $follow), $who)
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
            let real = db.physical(&who, &path, true).unwrap_or_default();
            view["can_write"] = json!(
                db.access(&who, &real, WRITE | EXEC)
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
        (Method::Post, "/api/register") => register(req, env, state, key, https).await,
        (Method::Post, "/api/unregister") => unregister(req, env, state, key, https).await,
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
                let real = real!(db, &who, &path, true);
                let n = acl!(db.access(&who, &real, if enter { EXEC } else { 0 }), &who);
                if enter && !n.directory {
                    return Reply::error("Not a directory", 400);
                }
                let mut value = json!(commands::entry(db, &who, &path, &real, &n));
                value["writable"] = json!(who.allows(&n, WRITE));
                value["readable"] = json!(who.allows(&n, READ));
                Reply::ok(value)
            })
        }
        (Method::Get, "/api/ls") => run(state, move |db| {
            let who = auth::identity(db, key.as_deref());
            let real = real!(db, &who, &path, true);
            let n = acl!(db.access(&who, &real, READ | EXEC), &who);
            if !n.directory {
                return Reply::error("Not a directory", 400);
            }
            let entries = commands::entries(db, &who, &real, &path, &db.children(&real));
            Reply::ok(json!(Listing { path, entries }))
        }),
        (Method::Post, "/api/mkdir") => run(state, move |db| {
            let who = auth::identity(db, key.as_deref());
            let real = real!(db, &who, &path, false);
            // Keep the original API's mkdir -p contract; CLI mkdir supports explicit -p.
            acl!(db.mkdir(&who, &real, true), &who);
            Reply::ok(json!({"path":path}))
        }),
        // Presentations (devices.rs): start one of the file at `path`, as `name`; stop one; and
        // what one at `path` (/dev/present/NAME) shows, to follow it.
        (Method::Post, "/api/present") => {
            let name = param("name");
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                let name = (!name.is_empty()).then_some(name.as_str());
                Reply::ok(acl!(devices::start(db, &who, &path, name), &who))
            })
        }
        (Method::Delete, "/api/present") => {
            let name = param("name");
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                Reply::ok(acl!(devices::stop(db, &who, PRESENT, &name, None), &who))
            })
        }
        // Live sound (sound.rs): send it, as the recording at `path` is made; stop it; hear the
        // device at `path` (/dev/snd/NAME); answer what Realtime offered, to hear it.
        (Method::Post, "/api/sound") => {
            let name = Some(param("name")).filter(|name| !name.is_empty());
            let entries = Some(param("acl")).filter(|entries| !entries.is_empty());
            // Of a recording, or kept nowhere.
            let recording = (!param("path").is_empty()).then_some(path);
            sound::speak(req, env, state, key, recording, name, entries).await
        }
        // The page sending a live sound still does: it lasts (devices::SOUND_IDLE).
        (Method::Post, "/api/sound/hold") => {
            let (name, version) = (param("name"), param("version"));
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                Reply::ok(acl!(devices::keep(db, &who, &name, &version), &who))
            })
        }
        (Method::Delete, "/api/sound") => {
            let name = param("name");
            let version = Some(param("version")).filter(|version| !version.is_empty());
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                let stopped = devices::stop(db, &who, SOUND, &name, version.as_deref());
                Reply::ok(acl!(stopped, &who))
            })
        }
        (Method::Post, "/api/sound/listen") => sound::listen(req, env, state, key, path).await,
        (Method::Put, "/api/sound/answer") => {
            sound::answer(req, env, state, param("session")).await
        }
        (Method::Get, "/api/device") => run(state, move |db| {
            let who = auth::identity(db, key.as_deref());
            Reply::ok(acl!(devices::attend(db, &who, &path), &who))
        }),
        (Method::Delete, "/api/file") => {
            let recursive = flag("recursive");
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                if path == "/" {
                    return Reply::error("Cannot remove the root directory", 403);
                }
                let real = real!(db, &who, &path, false);
                acl!(db.remove(&who, &real, recursive), &who);
                Reply::ok(json!({"path":path}))
            })
        }
        (Method::Get, "/internal/read") => run(state, move |db| {
            let who = auth::identity(db, key.as_deref());
            let real = real!(db, &who, &path, true);
            let node = acl!(db.access(&who, &real, READ), &who);
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
                "state": file.state, "etag": file.etag(), "mime": mime(&real), "chunks": chunks,
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
                        hold: false,
                    },
                )
            })
        }
        (Method::Post, "/api/uploads") => {
            #[derive(Deserialize)]
            struct Start {
                /// What it will be; a recording has none yet.
                size: Option<u64>,
                sha256: Option<String>,
                #[serde(default)]
                overwrite: bool,
                if_match: Option<String>,
                /// A recording (`stream`): a new file of no size yet, which grows.
                #[serde(default)]
                record: bool,
                /// A recording's ACL entries, as setfacl -m takes them (stream -m).
                #[serde(default)]
                acl: Option<String>,
            }
            if !body_size(req, 4096) {
                return Ok(Reply::error("Invalid upload request", 400));
            }
            let Ok(start) = req.json::<Start>().await.map_err(|_| ()).and_then(|start| {
                if start.record || start.size.is_some() {
                    Ok(start)
                } else {
                    Err(())
                }
            }) else {
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
            if start.record && !matches!(mime(&path), "audio/webm" | "video/webm") {
                return Ok(Reply::error(
                    "A recording is Opus in WebM: name it NAME.weba",
                    400,
                ));
            }
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                let reply = begin_upload(
                    db,
                    &who,
                    &path,
                    Begun {
                        size: start.size.filter(|_| !start.record).unwrap_or(0),
                        chunk_size: if start.record {
                            RECORDING_CHUNK
                        } else {
                            CHUNK_SIZE
                        },
                        limit: MAX_FILE,
                        sha256: start.sha256.filter(|_| !start.record),
                        overwrite: start.overwrite && !start.record,
                        if_match: start.if_match,
                        if_none_match: None,
                        mode: None,
                        hold: true,
                    },
                );
                if start.record
                    && reply.status < 400
                    && let Some(id) = reply.body["id"].as_str().map(str::to_string)
                {
                    db.record_file(&id);
                    db.set_expiry("upload", &id, auth::now() + RECORDING_IDLE);
                    // Who may hear it, as it is made (stream -m), or it is not made.
                    if let (Some(entries), Some(made)) = (&start.acl, db.showing(&id).pop())
                        && let Err(message) = devices::grant(db, &who, &made, entries)
                    {
                        return denied(&message, &who);
                    }
                }
                reply
            })
        }
        // The page sending an upload is still there: it keeps its hold.
        (Method::Post, "/api/uploads/hold") => {
            let id = param("id");
            let hold = param("hold");
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
                // A hold let go of is not taken back: a heartbeat sent before the release may
                // arrive after it. A page that goes on sending without one is refused once
                // another resumes the upload, holding it.
                match held(db, &id, Some(&hold)) {
                    Ok(()) => {
                        // A recording lasts while its page is heard; its live sound, as that page
                        // keeps it (/api/sound/hold).
                        if file.recording() {
                            db.set_expiry("upload", &id, auth::now() + RECORDING_IDLE);
                        }
                        Reply::ok(json!({"held": true}))
                    }
                    Err(reply) => reply,
                }
            })
        }
        // The page stopped sending the upload, which another may now resume at once.
        (Method::Post, "/api/uploads/release") => {
            let id = param("id");
            let hold = param("hold");
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                if db.file(&id).is_some_and(|f| f.uploader == who.id) {
                    db.release_hold(&id, &hold);
                }
                Reply::ok(json!({"released": true}))
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
            let hold = param("hold");
            // A recording's, which nobody knew before it ended.
            let sha256 = Some(param("sha256"))
                .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()));
            run(state, move |db| {
                let who = auth::identity(db, key.as_deref());
                if let Some(sha256) = &sha256
                    && let Some(file) = db
                        .file(&id)
                        .filter(|f| f.recording() && f.uploader == who.id)
                {
                    db.grow_file(&id, file.size, Some(sha256));
                }
                finish_upload(db, &who, &id, Some(&hold), true)
            })
        }
        (Method::Delete, "/api/uploads") => {
            let id = param("id");
            let hold = param("hold");
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
                // A page's Ctrl+C cancels the upload it holds, not one another page took over;
                // jobs cancel ID, which holds nothing, cancels it wherever it goes on.
                if !hold.is_empty() && db.hold(&id).is_some_and(|(token, _)| token != hold) {
                    return Reply::error(ELSEWHERE, 409);
                }
                // A recording is stopped, not cancelled: it keeps what arrived, as when its page
                // goes, and its live sound ends.
                if file.recording() {
                    finish_recording(db, &file);
                    return Reply::ok(json!({"stopped":true}));
                }
                cancel(db, &file);
                Reply::ok(json!({"cancelled":true}))
            })
        }
        (Method::Get, "/internal/upload/chunk") => {
            let id = param("id");
            let index = param("index").parse::<u64>().ok();
            let length = param("length").parse::<u64>().ok();
            let hold = param("hold");
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
                if let Err(reply) = held(db, &id, Some(&hold)) {
                    return reply;
                }
                // A recording takes its next chunk, whole or, last, short; or one sent again.
                if file.recording()
                    && index == Some(file.chunk_count())
                    && file.size.is_multiple_of(file.chunk_size)
                {
                    if !length.is_some_and(|n| n > 0 && n <= file.chunk_size) {
                        return Reply::error(
                            &format!("A recording's chunk is at most {} bytes", file.chunk_size),
                            400,
                        );
                    }
                    if file.size + length.unwrap_or_default() > RECORDING_MAX {
                        return Reply::error(RECORDING_FULL, 413);
                    }
                    let (path, _) = match destination(db, &who, &file) {
                        Ok(found) => found,
                        Err(reply) => return reply,
                    };
                    let index = file.chunk_count();
                    return Reply::ok(json!({"key": chunk_key(&id, index), "mime": mime(&path)}));
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
                hold: Option<String>,
                /// The chunk's length, which a recording grows by.
                length: Option<u64>,
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
                    commit.hold.as_deref(),
                    commit.complete,
                    commit.length,
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
            .map(|f| {
                json!({
                    "id": f.id, "path": f.target, "size": f.size, "received": db.received(f),
                    "recording": f.recording(),
                })
            })
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
                db.set_expiry("job", id, auth::now() + jobs::WAITING);
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
            if state == "question" {
                db.set_expiry("job", id, auth::now() + jobs::UNANSWERED);
            }
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
            // What each path is, and its mode, the link itself for a link, as lstat(2).
            let mut existing = serde_json::Map::new();
            let mut modes = serde_json::Map::new();
            for path in body["paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                // What lies where the job may not look is never told, not even whether it is
                // there: writing it fails, as for a path that is not.
                if let Ok(real) = db.physical(&actor, path, false)
                    && db.traverse(&actor, &real).is_ok()
                    && let Some(node) = db.node(&real)
                {
                    existing.insert(
                        path.into(),
                        json!(if node.directory { "directory" } else { "file" }),
                    );
                    modes.insert(path.into(), json!(node.mode));
                }
            }
            Reply::ok(json!({"existing": existing, "modes": modes}))
        }
        (Method::Post, "/internal/job/mkdir") => {
            let path = body["path"].as_str().unwrap_or_default();
            let real = real!(db, &actor, path, false);
            acl!(db.mkdir(&actor, &real, true), &actor);
            Reply::ok(json!({"path": path}))
        }
        (Method::Post, "/internal/job/begin") => {
            let path = body["path"].as_str().unwrap_or_default().to_string();
            let replace = body["replace"] == true;
            // Extraction removes a file in the way and makes it anew, as tar and unzip do, so
            // that it is the extractor's, with the archive's mode, and other names of the old
            // file keep it; a new archive (tar -c) writes over one, as open(2) does.
            let unlink = body["unlink"] == true;
            let real = real!(db, &actor, &path, false);
            // Only where the job may look is it told what is there.
            acl!(db.traverse(&actor, &real), &actor);
            let mut overwrite = replace;
            if let Some(node) = db.node(&real) {
                if node.directory {
                    return Reply::error(
                        &format!("{path}: a directory with this name already exists"),
                        409,
                    );
                }
                if !replace {
                    return Reply::ok(json!({"skipped": true}));
                }
                if unlink || node.is_link() {
                    acl!(db.remove(&actor, &real, false), &actor);
                    overwrite = false;
                }
            }
            begin_upload(
                db,
                &actor,
                &real,
                Begun {
                    size: body["size"].as_u64().unwrap_or(0),
                    chunk_size: CHUNK_SIZE,
                    limit: MAX_FILE,
                    sha256: None,
                    overwrite,
                    if_match: None,
                    if_none_match: None,
                    mode: body["mode"].as_u64().map(|m| (m & 0o7777) as u16),
                    hold: false,
                },
            )
        }
        (Method::Post, "/internal/job/link") | (Method::Post, "/internal/job/symlink") => {
            let path = body["path"].as_str().unwrap_or_default();
            let target = body["target"].as_str().unwrap_or_default();
            let real = real!(db, &actor, path, false);
            acl!(db.traverse(&actor, &real), &actor);
            if let Some(node) = db.node(&real) {
                if node.directory {
                    return Reply::error(
                        &format!("{path}: a directory with this name already exists"),
                        409,
                    );
                }
                if body["replace"] != true {
                    return Reply::ok(json!({"skipped": true}));
                }
            }
            // What is there gives way only once the link can be made.
            if endpoint == "/internal/job/symlink" {
                acl!(db.symlink(&actor, target, &real, true), &actor);
            } else {
                let source = real!(db, &actor, target, false);
                acl!(db.link(&actor, &source, &real, true), &actor);
            }
            Reply::ok(json!({"path": path}))
        }
        (Method::Post, "/internal/job/commit") => commit_upload(
            db,
            &actor,
            body["upload"].as_str().unwrap_or_default(),
            body["index"]
                .as_u64()
                .zip(body["key"].as_str().map(String::from)),
            body["sha256"].as_str().map(String::from),
            None,
            body["complete"] == true,
            // Jobs write whole files, never recordings.
            None,
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
            let real = real!(db, &actor, path, true);
            // Five digits: exactly this mode, a directory's setgid bit too.
            let mode = format!("{:05o}", body["mode"].as_u64().unwrap_or(0o644) & 0o7777);
            acl!(db.chmod(&actor, &real, &mode, false), &actor);
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
    // zip, unzip, tar and faststart start a job, whose owner is who is signed in.
    match commands::execute(db, &actor, who.id, command) {
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
        env.durable_object("JOBS")?
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
    /// Whether the page beginning it sends it in chunks, holding it (`HOLD`).
    hold: bool,
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
        hold,
    } = begin;
    // A link is written through, as open(2) does: one leading nowhere makes what it names.
    let real = real!(db, who, path, true);
    let path = real.as_str();
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
    let now = auth::now();
    // The same file, to the same path, by the same account: resume its upload, from this page
    // or another, unless a page is sending it now.
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
        if db.hold(&file.id).is_some_and(|(_, held)| now < held + HOLD) {
            return Reply::error(
                &format!(
                    "{path} is being uploaded from another tab or device; wait for it to finish, or stop it there"
                ),
                409,
            );
        }
        let token = hold.then(auth::random_token);
        if let Some(token) = &token {
            db.set_hold(&file.id, token, now);
        }
        let committed: Vec<u64> = db.chunks(&file.id).into_iter().map(|c| c.idx).collect();
        return Reply::ok(json!({
            "id": file.id, "created": true, "resumed": true, "size": size,
            "chunk_size": file.chunk_size, "committed": committed,
            "mode": old.as_ref().map(|n| n.mode), "hold": token,
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
            let uploading = old
                .file
                .as_deref()
                .and_then(|id| db.file(id))
                .is_some_and(|f| !f.complete());
            return Reply::error(
                if uploading {
                    "A file of this name is still being uploaded; wait for it to finish, or upload under another name"
                } else {
                    "File already exists; rename it before uploading"
                },
                409,
            );
        }
        old.file.clone()
    } else {
        acl!(db.writable_parent(who, path), who);
        None
    };
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
            let Some(above) = db.node(terminal_worker_core::parent(path)) else {
                return Reply::error("Parent directory does not exist", 404);
            };
            // Asked for rw-rw-rw-, less the umask; a mode from an archive is as it is.
            let mut node = match mode {
                // `created` takes permission bits only; an archive's setuid, setgid and sticky
                // bits (root's tar -x) are kept as given.
                Some(mode) => {
                    let mut node = created(who, &above, mode, false, 0);
                    node.mode |= mode & 0o7000;
                    node
                }
                None => created(who, &above, 0o666, false, UMASK),
            };
            node.file = Some(id.clone());
            node.size = size;
            db.set_node(path, &node);
            node.mode
        }
    };
    let token = hold.then(auth::random_token);
    if let Some(token) = &token {
        db.set_hold(&id, token, now);
    }
    Reply::ok(json!({
        "id": id, "created": old.is_none(), "size": size, "chunk_size": chunk_size.max(1),
        "mode": mode, "hold": token,
    }))
}

/// How long an upload stays with the page sending it after it was last heard from, which is
/// every few seconds while it is there (upload.mjs): until then, no other page resumes it.
const HOLD: u64 = 20;
/// Why a page may not go on with an upload.
const ELSEWHERE: &str = "the upload goes on from another tab or device";

/// Whether the page that sent `hold` may go on with upload `file`: the page holding it, which
/// is heard from again, or any, for an upload no page holds (one request, or a job's).
fn held(db: &mut Db, file: &str, hold: Option<&str>) -> std::result::Result<(), Reply> {
    let Some((token, _)) = db.hold(file) else {
        return Ok(());
    };
    if hold != Some(token.as_str()) {
        return Err(Reply::error(ELSEWHERE, 409));
    }
    db.set_hold(file, &token, auth::now());
    Ok(())
}

/// Record `chunk`, an index and the object written for it, as part of upload `id`, and complete
/// the upload if asked. A chunk is committed once: what readers have seen of it never changes,
/// and an object written for nothing goes to the garbage.
#[allow(clippy::too_many_arguments)]
fn commit_upload(
    db: &mut Db,
    who: &Identity,
    id: &str,
    chunk: Option<(u64, String)>,
    sha256: Option<String>,
    hold: Option<&str>,
    complete: bool,
    length: Option<u64>,
) -> Reply {
    let Some((index, key)) = chunk else {
        return finish_upload(db, who, id, hold, complete);
    };
    if !chunk_of(&key, id, index) {
        return Reply::error("Chunk key does not belong to this upload", 400);
    }
    // A recording's next chunk, which it grows by.
    let growing = db.file(id).filter(|file| {
        file.recording()
            && file.uploader == who.id
            && index == file.chunk_count()
            && file.size.is_multiple_of(file.chunk_size)
    });
    if let Some(file) = growing {
        let length = length.filter(|n| *n > 0 && *n <= file.chunk_size);
        let refused = match (
            length,
            held(db, id, hold).and_then(|()| destination(db, who, &file)),
        ) {
            (None, _) => Some(Reply::error("A recording's chunk must say its length", 400)),
            (Some(length), _) if file.size + length > RECORDING_MAX => {
                Some(Reply::error(RECORDING_FULL, 413))
            }
            (_, Err(reply)) => Some(reply),
            _ => None,
        };
        if let (Some(length), None) = (length, &refused) {
            let now = auth::now();
            db.add_chunk(id, index, &key, sha256.as_deref(), now);
            db.grow_file(id, index * file.chunk_size + length, None);
            db.set_expiry("upload", id, now + RECORDING_IDLE);
            return Reply::ok(json!({"committed":index}));
        }
        db.discard(&key);
        return refused.expect("refused").keep();
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
        Some(file) => match held(db, id, hold).and_then(|()| destination(db, who, &file)) {
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
    finish_upload(db, who, id, hold, true)
}
/// Complete upload `id` if asked and every chunk is committed, by the page holding it.
fn finish_upload(
    db: &mut Db,
    who: &Identity,
    id: &str,
    hold: Option<&str>,
    complete: bool,
) -> Reply {
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
    if let Err(reply) = held(db, id, hold) {
        return reply;
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
    if file.recording() {
        end_live(db, &file, &path);
    }
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

/// A recording whose page went away: it is complete with what arrived, in order; one with
/// nothing is abandoned. Its live sound ends.
pub fn finish_recording(db: &mut Db, file: &File) {
    if let Some(path) = db.showing(&file.id).pop() {
        end_live(db, file, &path);
    }
    // A recording takes only its next chunk, so what arrived is all there, in order.
    if file.size == 0 {
        return cancel(db, file);
    }
    db.finish_file(&file.id);
}

/// End the live sound of the recording `file`, which shows at `path`: a recording that ended is
/// live no more.
fn end_live(db: &mut Db, file: &File, path: &str) {
    for device in db.presenting(file.uploader, path) {
        devices::end(db, &device.name);
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
    if target.id == GUEST {
        return Ok(Reply::error(
            "guest has no password: it is who is signed out",
            400,
        ));
    }
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

/// The most accounts guests may make, all told.
const REGISTERED_MAX: usize = 1000;

/// An account of group guest's deleting itself (userdel NAME, signed in as NAME), as its
/// password confirms, and with `remove` its home: its sessions end, this one too.
async fn unregister(
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
    if who.id == 0 || !who.groups.contains(&GUEST) {
        return Ok(Reply::error(
            "Permission denied: an account deletes itself only if it is in group guest; root deletes others with sudo userdel",
            403,
        ));
    }
    if auth::limited(req, env, "unregister").await? {
        return Ok(Reply::error(
            "Too many attempts; please try again later",
            429,
        ));
    }
    if !body_size(req, 2048) {
        return Ok(Reply::error("Invalid account request", 400));
    }
    #[derive(Deserialize)]
    struct Unregister {
        password: String,
        #[serde(default)]
        remove: bool,
    }
    let Ok(wanted) = req.json::<Unregister>().await else {
        return Ok(Reply::error("Invalid account request", 400));
    };
    let (name, user) = read(state, {
        let id = who.id;
        move |db| {
            let name = db.username(id);
            let user = db.user(&name);
            (name, user)
        }
    })?;
    let Some(user) = user.filter(|user| user.id == who.id) else {
        return Ok(Reply::error("User does not exist", 404));
    };
    if !auth::verify(&wanted.password, &user, env).await? {
        return Ok(Reply::error("Incorrect password", 401));
    }
    run(state, move |db| {
        if auth::identity(db, key.as_deref()).id != who.id {
            return Reply::error("Signed out meanwhile; please try again", 409);
        }
        let kept = match commands::accounts::delete_account(db, &name, wanted.remove) {
            Ok(kept) => kept,
            Err(message) => return denied(&message, &who),
        };
        Reply::ok(json!({ "username": "guest", "deleted": name, "home": "/", "kept": kept }))
            .cookie(auth::cookie(https, "", 0))
    })
}

/// useradd NAME, signed out: an account of one's own, in its own group and in guest's, its home
/// /guest/NAME (made, only it may enter, with -m), and the password given; signed in as it at
/// once.
async fn register(
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
    if who.id != GUEST {
        return Ok(Reply::error(
            "useradd NAME makes an account for one who is signed out; signed in, use sudo useradd",
            409,
        ));
    }
    if auth::limited(req, env, "register").await? {
        return Ok(Reply::error(
            "Too many attempts; please try again later",
            429,
        ));
    }
    if !body_size(req, 2048) {
        return Ok(Reply::error("Invalid account request", 400));
    }
    #[derive(Deserialize)]
    struct Register {
        username: String,
        password: String,
        /// One shorter than 12 characters is meant (useradd --short-password).
        #[serde(default)]
        short_password: bool,
        /// Its home, /guest/NAME, is made too (useradd -m).
        #[serde(default)]
        create_home: bool,
    }
    let Ok(wanted) = req.json::<Register>().await else {
        return Ok(Reply::error("Invalid account request", 400));
    };
    let name = wanted.username;
    if !valid_name(&name) {
        return Ok(Reply::error(
            "Username is invalid (start with a lowercase letter; up to 32 lowercase letters, digits, underscores, or hyphens)",
            400,
        ));
    }
    if wanted.password.is_empty() || wanted.password.len() > 512 {
        return Ok(Reply::error(
            "The password cannot be empty and must not exceed 512 bytes",
            400,
        ));
    }
    if !wanted.short_password && !auth::password_valid(&wanted.password) {
        return Ok(Reply::error(
            "Use at least 12 characters; useradd --short-password allows a short password",
            400,
        ));
    }
    let create_home = wanted.create_home;
    // Refused before the slow hash, and again in the transaction that makes it.
    let taken = |db: &Db, name: &str| {
        if db.user(name).is_some() || db.group(name).is_some() {
            Some(Reply::error("Username already exists", 409))
        } else if db.node(GUEST_HOME).is_none_or(|n| !n.directory) {
            Some(Reply::error(
                "There is no /guest for homes; ask root to make it",
                503,
            ))
        } else if db
            .users()
            .iter()
            .filter(|(_, u)| u.home.starts_with(&format!("{GUEST_HOME}/")))
            .count()
            >= REGISTERED_MAX
        {
            Some(Reply::error(
                "Guests have made as many accounts as they may",
                503,
            ))
        } else {
            db.node(&format!("{GUEST_HOME}/{name}"))
                .map(|_| Reply::error("Something with that name is already in /guest", 409))
        }
    };
    if let Some(reply) = read(state, {
        let name = name.clone();
        move |db| taken(db, &name)
    })? {
        return Ok(reply);
    }
    let salt = auth::random_token();
    let hash = auth::hash(&wanted.password, &salt, env).await?;
    run(state, move |db| {
        if auth::identity(db, key.as_deref()).id != GUEST {
            return Reply::error("Signed in meanwhile; please try again", 409);
        }
        if let Some(reply) = taken(db, &name) {
            return reply;
        }
        let home = format!("{GUEST_HOME}/{name}");
        let gid = db.next_id(Id::Group);
        db.set_group(&name, gid);
        let uid = db.next_id(Id::User);
        db.set_user(
            &name,
            &User {
                id: uid,
                group: gid,
                groups: [GUEST].into(),
                home: home.clone(),
                locked: false,
                salt,
                hash,
            },
        );
        // Its home is /guest/NAME, made as useradd -m makes one, or not at all.
        if create_home {
            db.set_node(&home, &Node::directory(uid, gid, 0o700));
        }
        let (mut body, token) = auth::issue(db, key.as_deref(), uid);
        body["home"] = json!(home);
        body["created_home"] = json!(create_home);
        Reply::ok(body)
            .status(201)
            .cookie(auth::cookie(https, &token, auth::TTL))
    })
}
