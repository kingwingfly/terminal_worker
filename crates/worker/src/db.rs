//! The filesystem's metadata as tables in the Durable Object's SQLite, and the transactions that
//! change them. File bytes are not here: chunks of files are R2 objects, which these tables name.
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;
use terminal_worker_core::permissions::{Id, Node, Session, Store, User};
use wasm_bindgen::prelude::*;
use worker::{Error, Result, SqlStorage, SqlStorageValue, State};

#[wasm_bindgen(inline_js = "
export function transactionSync(storage, run) {
  let failure;
  try {
    storage.transactionSync(() => { failure = run(); if (failure !== undefined) throw new Error(failure); });
  } catch (error) {
    if (failure !== undefined) return failure;
    throw error;
  }
}
")]
extern "C" {
    #[wasm_bindgen(catch, js_name = transactionSync)]
    fn transaction_sync(
        storage: &JsValue,
        run: &js_sys::Function,
    ) -> std::result::Result<JsValue, JsValue>;
}

/// The rollback a transaction asks for when its reply is an error, which is not a failure.
const ROLLBACK: &str = "rollback";

const SCHEMA: [&str; 16] = [
    "CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS users(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, gid INTEGER NOT NULL, groups TEXT NOT NULL, home TEXT NOT NULL, locked INTEGER NOT NULL, salt TEXT NOT NULL, hash TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS unix_groups(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE)",
    "CREATE TABLE IF NOT EXISTS sessions(key TEXT PRIMARY KEY, uid INTEGER NOT NULL, expires INTEGER NOT NULL, sudo_until INTEGER NOT NULL)",
    // The root's parent is the empty string, so that it is nobody's child.
    "CREATE TABLE IF NOT EXISTS nodes(path TEXT PRIMARY KEY, parent TEXT NOT NULL, owner INTEGER NOT NULL, grp INTEGER NOT NULL, mode INTEGER NOT NULL, directory INTEGER NOT NULL, file TEXT, size INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS nodes_parent ON nodes(parent)",
    "CREATE INDEX IF NOT EXISTS nodes_file ON nodes(file)",
    // A file's content: `size` bytes in chunks of `chunk_size`, chunk i at i × chunk_size, the
    // last one short. An upload names its target path, and the file it replaces there, if any.
    "CREATE TABLE IF NOT EXISTS files(id TEXT PRIMARY KEY, size INTEGER NOT NULL, chunk_size INTEGER NOT NULL, state TEXT NOT NULL, sha256 TEXT, uploader INTEGER NOT NULL, target TEXT NOT NULL, base TEXT, touched INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS chunks(file TEXT NOT NULL, idx INTEGER NOT NULL, key TEXT NOT NULL, sha256 TEXT, PRIMARY KEY(file, idx))",
    // R2 objects nothing refers to any more, which the alarm deletes.
    "CREATE TABLE IF NOT EXISTS garbage(key TEXT PRIMARY KEY)",
    "CREATE TABLE IF NOT EXISTS expiries(kind TEXT NOT NULL, id TEXT NOT NULL, due INTEGER NOT NULL, PRIMARY KEY(kind, id))",
    "CREATE INDEX IF NOT EXISTS expiries_due ON expiries(due)",
    // Archive jobs: who started them and as whom they act (0 under sudo), what they do, and
    // how far they are; their output, line by line; and the files they read, which stay
    // until they finish, whatever happens to the paths.
    "CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY, owner INTEGER NOT NULL, actor INTEGER NOT NULL, kind TEXT NOT NULL, spec TEXT NOT NULL, state TEXT NOT NULL, progress TEXT NOT NULL, question TEXT, created INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS job_lines(job TEXT NOT NULL, seq INTEGER NOT NULL, err INTEGER NOT NULL, text TEXT NOT NULL, PRIMARY KEY(job, seq))",
    "CREATE TABLE IF NOT EXISTS job_files(job TEXT NOT NULL, file TEXT NOT NULL, PRIMARY KEY(job, file))",
    "CREATE INDEX IF NOT EXISTS job_files_file ON job_files(file)",
];

thread_local! {
    /// The first query of this transaction that failed; reads only borrow the store.
    static FAILED: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Whether a transaction left R2 objects to delete or a deadline to keep, for the alarm.
    static ALARM: Cell<bool> = const { Cell::new(false) };
}

/// Whether the alarm should run soon, clearing the request.
pub fn take_alarm() -> bool {
    ALARM.replace(false)
}
fn fail(error: Error) {
    FAILED.with(|f| {
        f.borrow_mut().get_or_insert_with(|| error.to_string());
    });
}

/// Create the tables, if they do not exist yet.
pub fn create(sql: &SqlStorage) -> Result<()> {
    for statement in SCHEMA {
        sql.exec(statement, None)?;
    }
    Ok(())
}

/// Run `work` in one SQLite transaction, with no await inside it: either all its writes commit,
/// or none do. It commits when `work` says so, alongside its value; a failed query rolls back
/// and is an error, as the value may come from a failed read.
pub fn transact<T: 'static>(
    state: &State,
    work: impl FnOnce(&mut Db) -> (T, bool) + 'static,
) -> Result<T> {
    let storage = state.storage();
    let sql = storage.sql();
    let out = Rc::new(RefCell::new(None));
    let slot = out.clone();
    let mut work = Some(work);
    let run = Closure::<dyn FnMut() -> JsValue>::new(move || {
        let Some(work) = work.take() else {
            return JsValue::from_str("transaction ran twice");
        };
        FAILED.with(|f| f.borrow_mut().take());
        let mut db = Db { sql: sql.clone() };
        let (value, commit) = work(&mut db);
        *slot.borrow_mut() = Some(value);
        match FAILED.with(|f| f.borrow_mut().take()) {
            Some(error) => JsValue::from_str(&error),
            None if !commit => JsValue::from_str(ROLLBACK),
            None => JsValue::UNDEFINED,
        }
    });
    let storage: &JsValue = storage.as_raw().as_ref();
    let result = transaction_sync(storage, run.as_ref().unchecked_ref()).map_err(Error::from)?;
    if let Some(error) = result.as_string()
        && error != ROLLBACK
    {
        return Err(Error::RustError(format!(
            "storage transaction failed: {error}"
        )));
    }
    out.take()
        .ok_or_else(|| Error::RustError("storage transaction did not run".into()))
}

#[derive(Deserialize)]
struct NodeRow {
    path: String,
    owner: u32,
    grp: u32,
    mode: u16,
    directory: u8,
    file: Option<String>,
    size: u64,
}
impl NodeRow {
    fn pair(self) -> (String, Node) {
        (
            self.path,
            Node {
                owner: self.owner,
                group: self.grp,
                mode: self.mode,
                directory: self.directory != 0,
                file: self.file,
                size: self.size,
            },
        )
    }
}
#[derive(Deserialize)]
struct UserRow {
    id: u32,
    name: String,
    gid: u32,
    groups: String,
    home: String,
    locked: u8,
    salt: String,
    hash: String,
}
impl UserRow {
    fn pair(self) -> (String, User) {
        (
            self.name,
            User {
                id: self.id,
                group: self.gid,
                groups: serde_json::from_str(&self.groups).unwrap_or_default(),
                home: self.home,
                locked: self.locked != 0,
                salt: self.salt,
                hash: self.hash,
            },
        )
    }
}
#[derive(Deserialize)]
struct GroupRow {
    id: u32,
    name: String,
}
#[derive(Deserialize)]
struct SessionRow {
    uid: u32,
    expires: u64,
    sudo_until: u64,
}
#[derive(Deserialize)]
struct Value {
    v: String,
}
#[derive(Deserialize)]
struct Key {
    key: String,
}

/// A file's content, and the upload that is writing it.
#[derive(Clone, Debug, Deserialize)]
pub struct File {
    pub id: String,
    pub size: u64,
    pub chunk_size: u64,
    pub state: String,
    pub sha256: Option<String>,
    pub uploader: u32,
    /// The path the upload writes ...
    pub target: String,
    /// ... and the file it replaces there, if it overwrites one.
    pub base: Option<String>,
}
impl File {
    pub fn complete(&self) -> bool {
        self.state == "complete"
    }
    /// How many chunks the whole file has.
    pub fn chunk_count(&self) -> u64 {
        self.size.div_ceil(self.chunk_size)
    }
    /// The size of chunk `index`: the chunk size, except for the last chunk.
    pub fn chunk_len(&self, index: u64) -> u64 {
        self.chunk_size
            .min(self.size.saturating_sub(index * self.chunk_size))
    }
    pub fn etag(&self) -> String {
        etag(&self.id)
    }
}
pub fn etag(file: &str) -> String {
    format!("\"{file}\"")
}
#[derive(Clone, Debug, Deserialize)]
pub struct Chunk {
    pub idx: u64,
    pub key: String,
}

fn int(value: impl TryInto<i64>) -> SqlStorageValue {
    SqlStorageValue::Integer(value.try_into().unwrap_or(i64::MAX))
}
fn text(value: &str) -> SqlStorageValue {
    value.into()
}
fn parent_of(path: &str) -> &str {
    if path == "/" {
        ""
    } else {
        terminal_worker_core::parent(path)
    }
}

/// The tables, read and written within one [`transact`]. A failed query reads as nothing, and
/// fails the transaction.
pub struct Db {
    sql: SqlStorage,
}
impl Db {
    fn rows<T: DeserializeOwned>(&self, query: &str, args: Vec<SqlStorageValue>) -> Vec<T> {
        self.sql
            .exec(query, args)
            .and_then(|c| c.to_array())
            .unwrap_or_else(|error| {
                fail(error);
                Vec::new()
            })
    }
    fn run(&mut self, query: &str, args: Vec<SqlStorageValue>) {
        if let Err(error) = self
            .sql
            .exec(query, args)
            .and_then(|c| c.to_array::<IgnoredAny>())
        {
            fail(error);
        }
    }

    pub fn meta(&self, key: &str) -> Option<String> {
        self.rows::<Value>("SELECT v FROM meta WHERE k = ?", vec![text(key)])
            .pop()
            .map(|r| r.v)
    }
    pub fn set_meta(&mut self, key: &str, value: &str) {
        self.run(
            "INSERT INTO meta(k, v) VALUES(?, ?) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            vec![text(key), text(value)],
        );
    }
    /// Whether no account exists yet, as before the first start.
    pub fn is_empty(&self) -> bool {
        self.rows::<Key>("SELECT name AS key FROM users LIMIT 1", vec![])
            .is_empty()
    }

    /// A new file of `size` bytes, being uploaded to `target` by `uploader`, with the SHA-256
    /// its uploader declares, if any.
    #[allow(clippy::too_many_arguments)]
    pub fn create_file(
        &mut self,
        size: u64,
        chunk_size: u64,
        sha256: Option<&str>,
        uploader: u32,
        target: &str,
        base: Option<&str>,
        now: u64,
    ) -> String {
        let id = crate::auth::random_token();
        self.run(
            "INSERT INTO files(id, size, chunk_size, state, sha256, uploader, target, base, touched) VALUES(?, ?, ?, 'uploading', ?, ?, ?, ?, ?)",
            vec![
                text(&id),
                int(size),
                int(chunk_size.max(1)),
                sha256.into(),
                int(uploader),
                text(target),
                base.into(),
                int(now),
            ],
        );
        id
    }
    /// How many bytes of `file` can be read from its start: its committed chunks, up to the
    /// first one missing.
    pub fn readable(&self, file: &File) -> u64 {
        if file.complete() {
            return file.size;
        }
        let mut next = 0;
        for chunk in self.chunks(&file.id) {
            if chunk.idx != next {
                break;
            }
            next += 1;
        }
        (next * file.chunk_size).min(file.size)
    }
    pub fn file(&self, id: &str) -> Option<File> {
        self.rows(
            "SELECT id, size, chunk_size, state, sha256, uploader, target, base FROM files WHERE id = ?",
            vec![text(id)],
        )
        .pop()
    }
    pub fn chunks(&self, file: &str) -> Vec<Chunk> {
        self.rows(
            "SELECT idx, key FROM chunks WHERE file = ? ORDER BY idx",
            vec![text(file)],
        )
    }
    pub fn add_chunk(&mut self, file: &str, index: u64, key: &str, sha256: Option<&str>, now: u64) {
        self.run(
            "INSERT INTO chunks(file, idx, key, sha256) VALUES(?, ?, ?, ?)",
            vec![text(file), int(index), text(key), sha256.into()],
        );
        self.run(
            "UPDATE files SET touched = ? WHERE id = ?",
            vec![int(now), text(file)],
        );
    }
    pub fn finish_file(&mut self, file: &str) {
        self.run(
            "UPDATE files SET state = 'complete' WHERE id = ?",
            vec![text(file)],
        );
        self.clear_expiry("upload", file);
    }
    /// Delete `file` and send its chunks to the garbage, whatever refers to it: every object
    /// under its prefix, also those written but never committed.
    pub fn drop_file(&mut self, file: &str) {
        self.discard(&format!("chunks/{file}/"));
        self.run("DELETE FROM chunks WHERE file = ?", vec![text(file)]);
        self.run("DELETE FROM files WHERE id = ?", vec![text(file)]);
        self.clear_expiry("upload", file);
        ALARM.set(true);
    }
    /// An R2 object written for nothing, such as a chunk of a cancelled upload, or, ending in
    /// `/`, every object under that prefix.
    pub fn discard(&mut self, key: &str) {
        self.run(
            "INSERT OR IGNORE INTO garbage(key) VALUES(?)",
            vec![text(key)],
        );
        ALARM.set(true);
    }
    fn release(&mut self, file: Option<String>) {
        let Some(file) = file else { return };
        // A job still reading it keeps it too.
        let referenced = !self
            .rows::<Key>(
                "SELECT path AS key FROM nodes WHERE file = ?1 UNION ALL SELECT job AS key FROM job_files WHERE file = ?1 LIMIT 1",
                vec![text(&file)],
            )
            .is_empty();
        if !referenced {
            self.drop_file(&file);
        }
    }

    pub fn create_job(
        &mut self,
        owner: u32,
        actor: u32,
        kind: &str,
        spec: &serde_json::Value,
        now: u64,
    ) -> String {
        let id = crate::auth::random_token()[..32].to_string();
        self.run(
            "INSERT INTO jobs(id, owner, actor, kind, spec, state, progress, question, created) VALUES(?, ?, ?, ?, ?, 'queued', '{}', NULL, ?)",
            vec![text(&id), int(owner), int(actor), text(kind), text(&spec.to_string()), int(now)],
        );
        id
    }
    pub fn job(&self, id: &str) -> Option<Job> {
        self.rows(&format!("{JOB} WHERE id = ?"), vec![text(id)])
            .pop()
    }
    /// The jobs `owner` started, newest first.
    pub fn jobs_of(&self, owner: u32, limit: u32) -> Vec<Job> {
        self.rows(
            &format!("{JOB} WHERE owner = ? ORDER BY created DESC LIMIT ?"),
            vec![int(owner), int(limit)],
        )
    }
    pub fn set_job(&mut self, id: &str, column: JobColumn, value: Option<&str>) {
        let column = match column {
            JobColumn::State => "state",
            JobColumn::Progress => "progress",
            JobColumn::Question => "question",
            JobColumn::Spec => "spec",
        };
        self.run(
            &format!("UPDATE jobs SET {column} = ? WHERE id = ?"),
            vec![value.into(), text(id)],
        );
    }
    /// Add lines to a job's output: `err` for what goes to standard error.
    pub fn add_lines(&mut self, id: &str, lines: &[(bool, String)]) {
        #[derive(Deserialize)]
        struct Last {
            seq: Option<u64>,
        }
        let mut seq = self
            .rows::<Last>(
                "SELECT MAX(seq) AS seq FROM job_lines WHERE job = ?",
                vec![text(id)],
            )
            .pop()
            .and_then(|l| l.seq)
            .unwrap_or(0);
        for (err, line) in lines {
            seq += 1;
            self.run(
                "INSERT INTO job_lines(job, seq, err, text) VALUES(?, ?, ?, ?)",
                vec![text(id), int(seq), int(*err as u8), text(line)],
            );
        }
    }
    /// A job's lines after `since`, as (seq, err, text).
    pub fn lines(&self, id: &str, since: u64, limit: u32) -> Vec<(u64, bool, String)> {
        #[derive(Deserialize)]
        struct Line {
            seq: u64,
            err: u8,
            text: String,
        }
        self.rows::<Line>(
            "SELECT seq, err, text FROM job_lines WHERE job = ? AND seq > ? ORDER BY seq LIMIT ?",
            vec![text(id), int(since), int(limit)],
        )
        .into_iter()
        .map(|l| (l.seq, l.err != 0, l.text))
        .collect()
    }
    /// Keep `file` while `job` reads it.
    pub fn pin(&mut self, job: &str, file: &str) {
        self.run(
            "INSERT OR IGNORE INTO job_files(job, file) VALUES(?, ?)",
            vec![text(job), text(file)],
        );
    }
    pub fn pinned(&self, job: &str, file: &str) -> bool {
        !self
            .rows::<Key>(
                "SELECT file AS key FROM job_files WHERE job = ? AND file = ?",
                vec![text(job), text(file)],
            )
            .is_empty()
    }
    /// Let go of the files `job` read, freeing those nothing else shows.
    pub fn unpin(&mut self, job: &str) {
        let files: Vec<String> = self
            .rows::<Key>(
                "SELECT file AS key FROM job_files WHERE job = ?",
                vec![text(job)],
            )
            .into_iter()
            .map(|k| k.key)
            .collect();
        self.run("DELETE FROM job_files WHERE job = ?", vec![text(job)]);
        for file in files {
            self.release(Some(file));
        }
    }
    /// The unfinished uploads that nodes in directory `dir` show.
    pub fn uploading_in(&self, dir: &str) -> Vec<File> {
        self.rows(
            "SELECT DISTINCT f.id, f.size, f.chunk_size, f.state, f.sha256, f.uploader, f.target, f.base FROM files f JOIN nodes n ON n.file = f.id WHERE n.parent = ? AND f.state = 'uploading'",
            vec![text(dir)],
        )
    }
    /// The uploads `uploader` has not finished.
    pub fn uploads_of(&self, uploader: u32) -> Vec<File> {
        self.rows(
            "SELECT id, size, chunk_size, state, sha256, uploader, target, base FROM files WHERE uploader = ? AND state = 'uploading' ORDER BY touched DESC LIMIT 50",
            vec![int(uploader)],
        )
    }
    /// The SHA-256 chunk `index` of `file` was committed with, or `None` if it is not committed.
    pub fn committed_hash(&self, file: &str, index: u64) -> Option<Option<String>> {
        #[derive(Deserialize)]
        struct Hash {
            sha256: Option<String>,
        }
        self.rows::<Hash>(
            "SELECT sha256 FROM chunks WHERE file = ? AND idx = ?",
            vec![text(file), int(index)],
        )
        .pop()
        .map(|h| h.sha256)
    }
    /// The committed chunks of `file`, as (index, SHA-256).
    pub fn chunk_hashes(&self, file: &str) -> Vec<(u64, Option<String>)> {
        #[derive(Deserialize)]
        struct Hash {
            idx: u64,
            sha256: Option<String>,
        }
        self.rows::<Hash>(
            "SELECT idx, sha256 FROM chunks WHERE file = ? ORDER BY idx",
            vec![text(file)],
        )
        .into_iter()
        .map(|h| (h.idx, h.sha256))
        .collect()
    }

    /// The paths of the nodes showing `file`.
    pub fn showing(&self, file: &str) -> Vec<String> {
        self.rows::<Key>(
            "SELECT path AS key FROM nodes WHERE file = ?",
            vec![text(file)],
        )
        .into_iter()
        .map(|k| k.key)
        .collect()
    }
    pub fn garbage(&self, limit: u32) -> Vec<String> {
        self.rows::<Key>("SELECT key FROM garbage LIMIT ?", vec![int(limit)])
            .into_iter()
            .map(|k| k.key)
            .collect()
    }
    pub fn forget_garbage(&mut self, keys: &[String]) {
        for key in keys {
            self.run("DELETE FROM garbage WHERE key = ?", vec![text(key)]);
        }
    }
    pub fn set_expiry(&mut self, kind: &str, id: &str, due: u64) {
        self.run(
            "INSERT INTO expiries(kind, id, due) VALUES(?, ?, ?) ON CONFLICT(kind, id) DO UPDATE SET due = excluded.due",
            vec![text(kind), text(id), int(due)],
        );
        ALARM.set(true);
    }
    pub fn clear_expiry(&mut self, kind: &str, id: &str) {
        self.run(
            "DELETE FROM expiries WHERE kind = ? AND id = ?",
            vec![text(kind), text(id)],
        );
    }
    /// The deadlines that have passed, as (kind, id).
    pub fn due(&self, now: u64) -> Vec<(String, String)> {
        #[derive(Deserialize)]
        struct Due {
            kind: String,
            id: String,
        }
        self.rows::<Due>(
            "SELECT kind, id FROM expiries WHERE due <= ? ORDER BY due LIMIT 100",
            vec![int(now)],
        )
        .into_iter()
        .map(|d| (d.kind, d.id))
        .collect()
    }
    /// The next deadline, in seconds.
    pub fn next_due(&self) -> Option<u64> {
        #[derive(Deserialize)]
        struct Next {
            due: u64,
        }
        self.rows::<Next>("SELECT due FROM expiries ORDER BY due LIMIT 1", vec![])
            .pop()
            .map(|n| n.due)
    }
}

const NODE: &str = "SELECT path, owner, grp, mode, directory, file, size FROM nodes";
const JOB: &str =
    "SELECT id, owner, actor, kind, spec, state, progress, question, created FROM jobs";

/// An archive job, as the authority keeps it.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct Job {
    pub id: String,
    pub owner: u32,
    pub actor: u32,
    pub kind: String,
    /// What it does, as JSON: see crate::jobs::Spec.
    pub spec: String,
    /// queued, running, question, cancelling, done, failed or cancelled.
    pub state: String,
    pub progress: String,
    pub question: Option<String>,
    pub created: u64,
}
impl Job {
    pub fn finished(&self) -> bool {
        matches!(self.state.as_str(), "done" | "failed" | "cancelled")
    }
}
/// The columns of a job that change.
pub enum JobColumn {
    State,
    Progress,
    Question,
    Spec,
}
const USER: &str = "SELECT id, name, gid, groups, home, locked, salt, hash FROM users";

impl Store for Db {
    fn node(&self, path: &str) -> Option<Node> {
        self.rows::<NodeRow>(&format!("{NODE} WHERE path = ?"), vec![text(path)])
            .pop()
            .map(|r| r.pair().1)
    }
    fn children(&self, path: &str) -> Vec<(String, Node)> {
        self.rows::<NodeRow>(
            &format!("{NODE} WHERE parent = ? ORDER BY path"),
            vec![text(path)],
        )
        .into_iter()
        .map(NodeRow::pair)
        .collect()
    }
    fn subtree(&self, path: &str) -> Vec<(String, Node)> {
        let rows = if path == "/" {
            self.rows::<NodeRow>(&format!("{NODE} ORDER BY path"), vec![])
        } else {
            // Every path starting with `path/`: '0' is the character after '/'.
            self.rows::<NodeRow>(
                &format!("{NODE} WHERE path = ? OR (path >= ? AND path < ?) ORDER BY path"),
                vec![
                    text(path),
                    text(&format!("{path}/")),
                    text(&format!("{path}0")),
                ],
            )
        };
        rows.into_iter().map(NodeRow::pair).collect()
    }
    fn set_node(&mut self, path: &str, node: &Node) {
        let old = self
            .rows::<NodeRow>(&format!("{NODE} WHERE path = ?"), vec![text(path)])
            .pop()
            .and_then(|r| r.file);
        self.run(
            "INSERT INTO nodes(path, parent, owner, grp, mode, directory, file, size) VALUES(?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(path) DO UPDATE SET owner = excluded.owner, grp = excluded.grp, mode = excluded.mode, directory = excluded.directory, file = excluded.file, size = excluded.size",
            vec![
                text(path),
                text(parent_of(path)),
                int(node.owner),
                int(node.group),
                int(node.mode),
                int(node.directory as u8),
                node.file.as_deref().into(),
                int(node.size),
            ],
        );
        if old.is_some() && old != node.file {
            self.release(old);
        }
    }
    fn delete_node(&mut self, path: &str) {
        let old = self
            .rows::<NodeRow>(&format!("{NODE} WHERE path = ?"), vec![text(path)])
            .pop()
            .and_then(|r| r.file);
        self.run("DELETE FROM nodes WHERE path = ?", vec![text(path)]);
        self.release(old);
    }

    fn user(&self, name: &str) -> Option<User> {
        self.rows::<UserRow>(&format!("{USER} WHERE name = ?"), vec![text(name)])
            .pop()
            .map(|r| r.pair().1)
    }
    fn user_by_id(&self, id: u32) -> Option<(String, User)> {
        self.rows::<UserRow>(&format!("{USER} WHERE id = ?"), vec![int(id)])
            .pop()
            .map(UserRow::pair)
    }
    fn users(&self) -> Vec<(String, User)> {
        self.rows::<UserRow>(&format!("{USER} ORDER BY name"), vec![])
            .into_iter()
            .map(UserRow::pair)
            .collect()
    }
    fn set_user(&mut self, name: &str, user: &User) {
        let groups: BTreeSet<u32> = user.groups.clone();
        self.run(
            "INSERT INTO users(id, name, gid, groups, home, locked, salt, hash) VALUES(?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET name = excluded.name, gid = excluded.gid, groups = excluded.groups, home = excluded.home, locked = excluded.locked, salt = excluded.salt, hash = excluded.hash",
            vec![
                int(user.id),
                text(name),
                int(user.group),
                text(&serde_json::to_string(&groups).unwrap_or_else(|_| "[]".into())),
                text(&user.home),
                int(user.locked as u8),
                text(&user.salt),
                text(&user.hash),
            ],
        );
    }
    fn delete_user(&mut self, name: &str) {
        self.run("DELETE FROM users WHERE name = ?", vec![text(name)]);
    }

    fn group(&self, name: &str) -> Option<u32> {
        self.rows::<GroupRow>(
            "SELECT id, name FROM unix_groups WHERE name = ?",
            vec![text(name)],
        )
        .pop()
        .map(|g| g.id)
    }
    fn group_name(&self, id: u32) -> Option<String> {
        self.rows::<GroupRow>(
            "SELECT id, name FROM unix_groups WHERE id = ?",
            vec![int(id)],
        )
        .pop()
        .map(|g| g.name)
    }
    fn groups(&self) -> Vec<(String, u32)> {
        self.rows::<GroupRow>("SELECT id, name FROM unix_groups ORDER BY name", vec![])
            .into_iter()
            .map(|g| (g.name, g.id))
            .collect()
    }
    fn set_group(&mut self, name: &str, id: u32) {
        self.run(
            "INSERT INTO unix_groups(id, name) VALUES(?, ?) ON CONFLICT(id) DO UPDATE SET name = excluded.name",
            vec![int(id), text(name)],
        );
    }
    fn delete_group(&mut self, name: &str) {
        self.run("DELETE FROM unix_groups WHERE name = ?", vec![text(name)]);
    }

    fn session(&self, key: &str) -> Option<Session> {
        self.rows::<SessionRow>(
            "SELECT uid, expires, sudo_until FROM sessions WHERE key = ?",
            vec![text(key)],
        )
        .pop()
        .map(|s| Session {
            user: s.uid,
            expires: s.expires,
            sudo_until: s.sudo_until,
        })
    }
    fn set_session(&mut self, key: &str, session: &Session) {
        self.run(
            "INSERT INTO sessions(key, uid, expires, sudo_until) VALUES(?, ?, ?, ?) ON CONFLICT(key) DO UPDATE SET uid = excluded.uid, expires = excluded.expires, sudo_until = excluded.sudo_until",
            vec![
                text(key),
                int(session.user),
                int(session.expires),
                int(session.sudo_until),
            ],
        );
    }
    fn delete_session(&mut self, key: &str) {
        self.run("DELETE FROM sessions WHERE key = ?", vec![text(key)]);
    }
    fn delete_sessions(&mut self, user: u32) {
        self.run("DELETE FROM sessions WHERE uid = ?", vec![int(user)]);
    }
    fn delete_expired_sessions(&mut self, now: u64) {
        self.run("DELETE FROM sessions WHERE expires <= ?", vec![int(now)]);
    }

    fn next_id(&mut self, kind: Id) -> u32 {
        let key = match kind {
            Id::User => "next_user",
            Id::Group => "next_group",
        };
        let next = self
            .meta(key)
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1000);
        self.set_meta(key, &(next + 1).to_string());
        next
    }
}
