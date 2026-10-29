//! The filesystem's metadata as tables in the Durable Object's SQLite, and the transactions that
//! change them. File bytes are not here: chunks of files are R2 objects, which these tables name.
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use terminal_worker_core::permissions::{Id, Identity, Node, Session, Store, User};
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

/// The version of the tables below, which local state made by an older one does not have.
pub const SCHEMA_VERSION: &str = "3";

const SCHEMA: [&str; 23] = [
    "CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS users(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, gid INTEGER NOT NULL, groups TEXT NOT NULL, home TEXT NOT NULL, locked INTEGER NOT NULL, salt TEXT NOT NULL, hash TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS unix_groups(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE)",
    "CREATE TABLE IF NOT EXISTS sessions(key TEXT PRIMARY KEY, uid INTEGER NOT NULL, expires INTEGER NOT NULL, sudo_until INTEGER NOT NULL)",
    // Names, and the inodes they name: hard links are names of one inode. The root's parent is
    // the empty string, so that it is nobody's child.
    "CREATE TABLE IF NOT EXISTS nodes(path TEXT PRIMARY KEY, parent TEXT NOT NULL, ino INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS nodes_parent ON nodes(parent)",
    "CREATE INDEX IF NOT EXISTS nodes_ino ON nodes(ino)",
    // An inode: a directory, a file showing content, or a symbolic link to `link`; its ACLs as
    // JSON (crate::acl); when its content (a directory's entries) last changed, and it at all.
    "CREATE TABLE IF NOT EXISTS inodes(id INTEGER PRIMARY KEY, owner INTEGER NOT NULL, grp INTEGER NOT NULL, mode INTEGER NOT NULL, directory INTEGER NOT NULL, file TEXT, size INTEGER NOT NULL, link TEXT, acl TEXT, default_acl TEXT, mtime INTEGER NOT NULL DEFAULT 0, ctime INTEGER NOT NULL DEFAULT 0)",
    "CREATE INDEX IF NOT EXISTS inodes_file ON inodes(file)",
    // Extended attributes, their values in hexadecimal.
    "CREATE TABLE IF NOT EXISTS xattrs(ino INTEGER NOT NULL, name TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(ino, name))",
    // A file's content: `size` bytes in chunks of `chunk_size`, chunk i at i × chunk_size, the
    // last one short. An upload names its target path, and the file it replaces there, if any.
    "CREATE TABLE IF NOT EXISTS files(id TEXT PRIMARY KEY, size INTEGER NOT NULL, chunk_size INTEGER NOT NULL, state TEXT NOT NULL, sha256 TEXT, uploader INTEGER NOT NULL, target TEXT NOT NULL, base TEXT, touched INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS chunks(file TEXT NOT NULL, idx INTEGER NOT NULL, key TEXT NOT NULL, sha256 TEXT, PRIMARY KEY(file, idx))",
    // The page sending an upload in chunks, by the token it was given, and when it was last
    // heard from: one page at a time (api::HOLD). A table of its own, made where it is missing.
    "CREATE TABLE IF NOT EXISTS upload_holds(file TEXT PRIMARY KEY, token TEXT NOT NULL, held INTEGER NOT NULL)",
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
    // Where each account last showed a deck or a presented PDF (store.rs relay), which its
    // pages open at, on any device. Tables of their own, made where they are missing.
    "CREATE TABLE IF NOT EXISTS places(uid INTEGER NOT NULL, path TEXT NOT NULL, col INTEGER NOT NULL, row INTEGER NOT NULL, PRIMARY KEY(uid, path))",
    // What /dev holds (devices.rs): presentations others follow, whose they are, and the file
    // presented; `media`, what a live sound has to be heard by.
    "CREATE TABLE IF NOT EXISTS devices(name TEXT PRIMARY KEY, owner INTEGER NOT NULL, path TEXT NOT NULL, media TEXT, started INTEGER NOT NULL)",
    // A listener a presenter let speak (devices::allow): by the token its page was given, in
    // which presentation, as whom, for which ask, and the voice it sends, once it does. A
    // table of its own, made where it is missing.
    "CREATE TABLE IF NOT EXISTS grants(token TEXT PRIMARY KEY, presentation TEXT NOT NULL, speaker TEXT NOT NULL, ask TEXT NOT NULL, device TEXT)",
];

thread_local! {
    /// The first query of this transaction that failed; reads only borrow the store.
    static FAILED: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Whether a transaction left R2 objects to delete or a deadline to keep, for the alarm.
    static ALARM: Cell<bool> = const { Cell::new(false) };
    /// The directories whose listings the running transaction changed, ...
    static CHANGING: RefCell<BTreeSet<String>> = const { RefCell::new(BTreeSet::new()) };
    /// ... and those committed transactions changed, for the pages that show them.
    static CHANGED: RefCell<BTreeSet<String>> = const { RefCell::new(BTreeSet::new()) };
    /// Whether a transaction ended sessions, which the pages holding them hear of.
    static ENDED: Cell<bool> = const { Cell::new(false) };
    /// The devices the running transaction removed, or replaced with one of the same name that
    /// shows something else, ...
    static REMOVING: RefCell<Vec<Removed>> = const { RefCell::new(Vec::new()) };
    /// ... and those committed transactions removed or replaced, which the pages following them,
    /// and their account's, hear of.
    static REMOVED: RefCell<Vec<Removed>> = const { RefCell::new(Vec::new()) };
    /// The live sounds the running transaction started, ...
    static STARTING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// ... and those committed transactions started, which the pages following a presentation
    /// of their account hear of, to hear them (owner's request, 2026-10-28).
    static STARTED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// A device removed, or replaced (`replaced`), as it was: whose, and which (devices::version).
#[derive(Clone, Debug)]
pub struct Removed {
    pub name: String,
    pub replaced: bool,
    pub owner: u32,
    pub version: String,
}

/// The directories whose listings committed transactions changed since this was last asked.
pub fn take_changes() -> BTreeSet<String> {
    CHANGED.take()
}

/// Whether sessions ended since this was last asked: signed out, renewed, revoked or expired.
pub fn take_ended() -> bool {
    ENDED.replace(false)
}

/// The devices committed transactions removed, or replaced, since this was last asked.
pub fn take_removed() -> Vec<Removed> {
    REMOVED.take()
}

/// The live sounds committed transactions started since this was last asked.
pub fn take_started() -> Vec<String> {
    STARTED.take()
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

/// Create the tables, if they do not exist yet; refuse tables an older version made, which
/// nothing migrates, as nothing is deployed.
pub fn create(sql: &SqlStorage) -> Result<()> {
    #[derive(Deserialize)]
    struct Name {
        name: String,
    }
    let tables: Vec<Name> = sql
        .exec(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('users', 'meta')",
            None,
        )?
        .to_array()?;
    if tables.iter().any(|t| t.name == "users") {
        let version: Vec<Value> = if tables.iter().any(|t| t.name == "meta") {
            sql.exec("SELECT v FROM meta WHERE k = 'schema'", None)?
                .to_array()?
        } else {
            Vec::new()
        };
        if version.first().map(|v| v.v.as_str()) != Some(SCHEMA_VERSION) {
            return Err(Error::RustError(
                "This storage was made by an older version of the terminal, whose tables are not migrated; delete the local state (.wrangler/state) and start again".into(),
            ));
        }
    }
    for statement in SCHEMA {
        sql.exec(statement, None)?;
    }
    // Inodes made before they had times get them, as of 1970.
    if sql.exec("SELECT mtime FROM inodes LIMIT 0", None).is_err() {
        for column in ["mtime", "ctime"] {
            sql.exec(
                &format!("ALTER TABLE inodes ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"),
                None,
            )?;
        }
    }
    // Tables made just now are this version's, said at once: initialization may fail (a
    // missing ADMIN_PASSWORD), and the next start must not take them for an older version's.
    if !tables.iter().any(|t| t.name == "users") {
        sql.exec(
            &format!("INSERT OR REPLACE INTO meta(k, v) VALUES('schema', '{SCHEMA_VERSION}')"),
            None,
        )?;
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
        CHANGING.take();
        REMOVING.take();
        STARTING.take();
        let mut db = Db { sql: sql.clone() };
        let (value, commit) = work(&mut db);
        *slot.borrow_mut() = Some(value);
        let changing = CHANGING.take();
        let removing = REMOVING.take();
        let starting = STARTING.take();
        match FAILED.with(|f| f.borrow_mut().take()) {
            Some(error) => JsValue::from_str(&error),
            None if !commit => JsValue::from_str(ROLLBACK),
            None => {
                CHANGED.with(|changed| changed.borrow_mut().extend(changing));
                REMOVED.with(|removed| removed.borrow_mut().extend(removing));
                STARTED.with(|started| started.borrow_mut().extend(starting));
                JsValue::UNDEFINED
            }
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
    ino: u64,
    owner: u32,
    grp: u32,
    mode: u16,
    directory: u8,
    file: Option<String>,
    size: u64,
    link: Option<String>,
    acl: Option<String>,
    default_acl: Option<String>,
    links: u32,
}
impl NodeRow {
    fn pair(self) -> (String, Node) {
        (
            self.path,
            Node {
                ino: self.ino,
                owner: self.owner,
                group: self.grp,
                mode: self.mode,
                directory: self.directory != 0,
                file: self.file,
                size: self.size,
                link: self.link,
                links: self.links,
                acl: self.acl.and_then(|a| serde_json::from_str(&a).ok()),
                default_acl: self.default_acl.and_then(|a| serde_json::from_str(&a).ok()),
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
#[derive(Deserialize)]
struct Ino {
    ino: u64,
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
    /// Whether it is a recording still being made (`stream`): it has as many bytes as have
    /// arrived, in order, and grows by a chunk at a time until it completes.
    pub fn recording(&self) -> bool {
        self.state == "recording"
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
/// A device of /dev: its path there, whose it is, the file it presents, and what its live sound
/// is heard by, if it has one.
#[derive(Clone, Debug, Deserialize)]
pub struct Device {
    pub name: String,
    pub owner: u32,
    pub path: String,
    pub media: Option<String>,
}
/// A listener let speak in a presentation (devices::allow).
#[derive(Clone, Debug, Deserialize)]
pub struct Grant {
    pub token: String,
    pub presentation: String,
    pub speaker: String,
    pub ask: String,
    pub device: Option<String>,
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
/// Note that the listing of the directory holding `path` changes.
fn changes(path: &str) {
    if path != "/" {
        CHANGING.with(|changing| {
            changing
                .borrow_mut()
                .insert(terminal_worker_core::parent(path).to_string())
        });
    }
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
    /// How many bytes of `file` have arrived: its committed chunks, wherever they are, as an
    /// upload's progress counts them.
    pub fn received(&self, file: &File) -> u64 {
        if file.complete() {
            return file.size;
        }
        self.chunks(&file.id)
            .iter()
            .map(|chunk| file.chunk_len(chunk.idx))
            .sum()
    }
    /// The token of the page sending upload `file`, and when it was last heard from.
    pub fn hold(&self, file: &str) -> Option<(String, u64)> {
        #[derive(Deserialize)]
        struct Hold {
            token: String,
            held: u64,
        }
        self.rows::<Hold>(
            "SELECT token, held FROM upload_holds WHERE file = ?",
            vec![text(file)],
        )
        .pop()
        .map(|hold| (hold.token, hold.held))
    }
    /// Let go of upload `file`, if `token` holds it.
    pub fn release_hold(&mut self, file: &str, token: &str) {
        self.run(
            "DELETE FROM upload_holds WHERE file = ? AND token = ?",
            vec![text(file), text(token)],
        );
    }
    /// Delete the sessions `condition` selects, and their expiries; the pages holding them hear
    /// that they ended, if any did.
    fn end_sessions(&mut self, condition: &str, args: Vec<SqlStorageValue>) {
        #[derive(Deserialize)]
        struct Ended {
            key: String,
        }
        let ended = self.rows::<Ended>(
            &format!("DELETE FROM sessions WHERE {condition} RETURNING key"),
            args,
        );
        for session in &ended {
            self.clear_expiry("session", &session.key);
        }
        if !ended.is_empty() {
            ENDED.set(true);
        }
    }
    pub fn set_hold(&mut self, file: &str, token: &str, now: u64) {
        self.run(
            "INSERT OR REPLACE INTO upload_holds(file, token, held) VALUES(?, ?, ?)",
            vec![text(file), text(token), int(now)],
        );
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
        // How much of it can be read shows where it is listed.
        for path in self.showing(file) {
            changes(&path);
        }
        self.run(
            "INSERT INTO chunks(file, idx, key, sha256) VALUES(?, ?, ?, ?)",
            vec![text(file), int(index), text(key), sha256.into()],
        );
        self.run(
            "UPDATE files SET touched = ? WHERE id = ?",
            vec![int(now), text(file)],
        );
    }
    /// Make the new upload `file` a recording, which grows as it is heard.
    pub fn record_file(&mut self, file: &str) {
        self.run(
            "UPDATE files SET state = 'recording' WHERE id = ?",
            vec![text(file)],
        );
    }
    /// Set the size of the recording `file`, and of the nodes showing it, as it grows; and its
    /// SHA-256 once it is whole, if known.
    pub fn grow_file(&mut self, file: &str, size: u64, sha256: Option<&str>) {
        self.run(
            "UPDATE files SET size = ?, sha256 = COALESCE(?, sha256) WHERE id = ?",
            vec![
                int(size),
                sha256.map_or(SqlStorageValue::Null, text),
                text(file),
            ],
        );
        for path in self.showing(file) {
            if let Some(node) = self.node(&path) {
                self.set_node(&path, &Node { size, ..node });
            }
        }
    }
    pub fn finish_file(&mut self, file: &str) {
        for path in self.showing(file) {
            changes(&path);
        }
        self.run(
            "UPDATE files SET state = 'complete' WHERE id = ?",
            vec![text(file)],
        );
        self.run("DELETE FROM upload_holds WHERE file = ?", vec![text(file)]);
        self.clear_expiry("upload", file);
    }
    /// Delete `file` and send its chunks to the garbage, whatever refers to it: every object
    /// under its prefix, also those written but never committed.
    pub fn drop_file(&mut self, file: &str) {
        self.discard(&format!("chunks/{file}/"));
        self.run("DELETE FROM chunks WHERE file = ?", vec![text(file)]);
        self.run("DELETE FROM files WHERE id = ?", vec![text(file)]);
        self.run("DELETE FROM upload_holds WHERE file = ?", vec![text(file)]);
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
                "SELECT CAST(id AS TEXT) AS key FROM inodes WHERE file = ?1 UNION ALL SELECT job AS key FROM job_files WHERE file = ?1 LIMIT 1",
                vec![text(&file)],
            )
            .is_empty();
        if !referenced {
            self.drop_file(&file);
        }
    }
    /// The names of the inode `ino`.
    fn names(&self, ino: u64) -> Vec<String> {
        self.rows::<Key>(
            "SELECT path AS key FROM nodes WHERE ino = ?",
            vec![int(ino)],
        )
        .into_iter()
        .map(|k| k.key)
        .collect()
    }
    /// Let the inode `ino` go, and what it shows, if nothing names it.
    fn forget(&mut self, ino: u64) {
        if !self.names(ino).is_empty() {
            return;
        }
        #[derive(Deserialize)]
        struct Shown {
            file: Option<String>,
        }
        let file = self
            .rows::<Shown>("SELECT file FROM inodes WHERE id = ?", vec![int(ino)])
            .pop()
            .and_then(|s| s.file);
        self.run("DELETE FROM inodes WHERE id = ?", vec![int(ino)]);
        self.run("DELETE FROM xattrs WHERE ino = ?", vec![int(ino)]);
        self.release(file);
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
    /// The uploads `uploader` has not finished.
    pub fn uploads_of(&self, uploader: u32) -> Vec<File> {
        self.rows(
            "SELECT id, size, chunk_size, state, sha256, uploader, target, base FROM files WHERE uploader = ? AND state IN ('uploading', 'recording') ORDER BY touched DESC LIMIT 50",
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
            "SELECT n.path AS key FROM nodes n JOIN inodes i ON i.id = n.ino WHERE i.file = ?",
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
    /// Where `uid` last showed the deck or presented PDF at `path`: its column and row (a PDF's
    /// page and 0).
    pub fn place(&self, uid: u32, path: &str) -> Option<(u32, u32)> {
        #[derive(Deserialize)]
        struct Place {
            col: u32,
            row: u32,
        }
        self.rows::<Place>(
            "SELECT col, row FROM places WHERE uid = ? AND path = ?",
            vec![int(uid), text(path)],
        )
        .pop()
        .map(|place| (place.col, place.row))
    }
    pub fn set_place(&mut self, uid: u32, path: &str, (col, row): (u32, u32)) {
        self.run(
            "INSERT OR REPLACE INTO places(uid, path, col, row) VALUES(?, ?, ?, ?)",
            vec![int(uid), text(path), int(col), int(row)],
        );
    }
    /// Forget where `uid` showed anything: its account is gone, and a new one may take its uid.
    pub fn delete_places(&mut self, uid: u32) {
        self.run("DELETE FROM places WHERE uid = ?", vec![int(uid)]);
    }
    pub fn device(&self, name: &str) -> Option<Device> {
        self.rows::<Device>(
            "SELECT name, owner, path, media FROM devices WHERE name = ?",
            vec![text(name)],
        )
        .pop()
    }
    /// The devices of `owner` presenting the file at `path`, or sending it live; not the voices
    /// of listeners speaking in its presentations, which link to it too.
    pub fn presenting(&self, owner: u32, path: &str) -> Vec<Device> {
        self.rows::<Device>(
            "SELECT name, owner, path, media FROM devices WHERE owner = ? AND path = ? AND json_extract(media, '$.for') IS NULL",
            vec![int(owner), text(path)],
        )
    }
    /// The names of `owner`'s live sounds, not the voices of listeners it let speak.
    pub fn sounds_of(&self, owner: u32) -> Vec<String> {
        self.sounds_by(owner)
            .into_iter()
            .map(|device| device.name)
            .collect()
    }
    /// `owner`'s live sounds, not the voices of listeners it let speak, the latest first.
    pub fn sounds_by(&self, owner: u32) -> Vec<Device> {
        self.rows::<Device>(
            "SELECT name, owner, path, media FROM devices WHERE owner = ? AND media IS NOT NULL AND json_extract(media, '$.for') IS NULL ORDER BY started DESC, name",
            vec![int(owner)],
        )
    }
    /// The voices of listeners let speak in the presentation `presentation`, the first first.
    pub fn voices_for(&self, presentation: &str) -> Vec<Device> {
        self.rows::<Device>(
            "SELECT name, owner, path, media FROM devices WHERE json_extract(media, '$.for') = ? ORDER BY started, name",
            vec![text(presentation)],
        )
    }
    /// The names of `owner`'s devices.
    pub fn devices_of(&self, owner: u32) -> Vec<String> {
        self.rows::<Device>(
            "SELECT name, owner, path, media FROM devices WHERE owner = ?",
            vec![int(owner)],
        )
        .into_iter()
        .map(|device| device.name)
        .collect()
    }
    pub fn set_device(&mut self, device: &Device, started: u64) {
        self.run(
            "INSERT OR REPLACE INTO devices(name, owner, path, media, started) VALUES(?, ?, ?, ?, ?)",
            vec![
                text(&device.name),
                int(device.owner),
                text(&device.path),
                device.media.as_deref().map_or(SqlStorageValue::Null, text),
                int(started),
            ],
        );
    }
    /// Remove the device `name`, whose followers hear so once this commits.
    pub fn delete_device(&mut self, name: &str) {
        self.removing(name, false);
        self.run("DELETE FROM devices WHERE name = ?", vec![text(name)]);
    }
    /// Note that the device `name` now shows something else, which its followers hear once this
    /// commits, to follow it again, if they still may.
    pub fn replaced_device(&mut self, name: &str) {
        self.removing(name, true);
    }
    fn removing(&self, name: &str, replaced: bool) {
        let Some(device) = self.device(name) else {
            return;
        };
        let removed = Removed {
            name: name.to_string(),
            replaced,
            owner: device.owner,
            version: crate::devices::version(&device),
        };
        REMOVING.with(|removing| removing.borrow_mut().push(removed));
    }
    /// Note that the live sound `name` started, which the pages following a presentation of its
    /// account hear once this commits.
    pub fn started_sound(&mut self, name: &str) {
        STARTING.with(|starting| starting.borrow_mut().push(name.to_string()));
    }
    pub fn grant(&self, token: &str) -> Option<Grant> {
        self.rows::<Grant>(
            "SELECT token, presentation, speaker, ask, device FROM grants WHERE token = ?",
            vec![text(token)],
        )
        .pop()
    }
    pub fn set_grant(&mut self, grant: &Grant) {
        self.run(
            "INSERT OR REPLACE INTO grants(token, presentation, speaker, ask, device) VALUES(?, ?, ?, ?, ?)",
            vec![
                text(&grant.token),
                text(&grant.presentation),
                text(&grant.speaker),
                text(&grant.ask),
                grant.device.as_deref().map_or(SqlStorageValue::Null, text),
            ],
        );
    }
    pub fn delete_grant(&mut self, token: &str) {
        self.run("DELETE FROM grants WHERE token = ?", vec![text(token)]);
    }
    /// Forget the grants of the presentation, or the voice, `name`, which ended.
    pub fn delete_grants_of(&mut self, name: &str) {
        self.run(
            "DELETE FROM grants WHERE presentation = ? OR device = ?",
            vec![text(name), text(name)],
        );
    }
    pub fn set_expiry(&mut self, kind: &str, id: &str, due: u64) {
        self.run(
            "INSERT INTO expiries(kind, id, due) VALUES(?, ?, ?) ON CONFLICT(kind, id) DO UPDATE SET due = excluded.due",
            vec![text(kind), text(id), int(due)],
        );
        ALARM.set(true);
    }
    /// When the deadline of `kind` `id` falls, if it has one.
    pub fn expiry(&self, kind: &str, id: &str) -> Option<u64> {
        #[derive(Deserialize)]
        struct Due {
            due: u64,
        }
        self.rows::<Due>(
            "SELECT due FROM expiries WHERE kind = ? AND id = ?",
            vec![text(kind), text(id)],
        )
        .pop()
        .map(|d| d.due)
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

/// Names and their inodes, with how many names each has: a directory, 2 and one for each
/// directory in it.
const NODE: &str = "SELECT n.path, i.id AS ino, i.owner, i.grp, i.mode, i.directory, i.file, i.size, i.link, i.acl, i.default_acl, CASE WHEN i.directory = 1 THEN 2 + (SELECT COUNT(*) FROM nodes c JOIN inodes ci ON ci.id = c.ino WHERE c.parent = n.path AND ci.directory = 1) ELSE (SELECT COUNT(*) FROM nodes m WHERE m.ino = i.id) END AS links FROM nodes n JOIN inodes i ON i.id = n.ino";
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

impl Db {
    /// The entries of the directory holding `path` changed: its times are now.
    fn entries_changed(&mut self, path: &str, now: u64) {
        if path == "/" {
            return;
        }
        self.run(
            "UPDATE inodes SET mtime = ?1, ctime = ?1 WHERE id = (SELECT ino FROM nodes WHERE path = ?2)",
            vec![int(now), text(terminal_worker_core::parent(path))],
        );
    }
}

/// What the kernel's file system keeps beyond names: the time, inodes' times, empty content,
/// and what has arrived of content still being written.
impl kernel::Fs for Db {
    fn now(&self) -> u64 {
        crate::auth::now()
    }
    fn times(&self, ino: u64) -> (u64, u64) {
        #[derive(Deserialize)]
        struct Times {
            mtime: u64,
            ctime: u64,
        }
        self.rows::<Times>(
            "SELECT mtime, ctime FROM inodes WHERE id = ?",
            vec![int(ino)],
        )
        .pop()
        .map_or((0, 0), |times| (times.mtime, times.ctime))
    }
    fn touch(&mut self, ino: u64) {
        let now = crate::auth::now();
        self.run(
            "UPDATE inodes SET mtime = ?1, ctime = ?1 WHERE id = ?2",
            vec![int(now), int(ino)],
        );
        for name in self.names(ino) {
            changes(&name);
        }
    }
    fn empty(&mut self, owner: u32, path: &str) -> String {
        let file = self.create_file(0, 1, None, owner, path, None, crate::auth::now());
        self.finish_file(&file);
        file
    }
    fn writing(&self, file: &str) -> Option<(u64, bool)> {
        let file = self.file(file).filter(|file| !file.complete())?;
        Some((self.received(&file), file.recording()))
    }
    fn unlink_device(
        &mut self,
        cred: &Identity,
        path: &str,
    ) -> Option<std::result::Result<(), abi::Error>> {
        let stopped = crate::devices::remove(self, cred, path)?;
        Some(stopped.map(|_| ()).map_err(|why| {
            let errno = if crate::commands::is_permission_error(&why) {
                abi::Errno::EACCES
            } else {
                abi::Errno::EPERM
            };
            abi::Error::because(errno, why)
        }))
    }
}

impl Store for Db {
    fn node(&self, path: &str) -> Option<Node> {
        self.rows::<NodeRow>(&format!("{NODE} WHERE n.path = ?"), vec![text(path)])
            .pop()
            .map(|r| r.pair().1)
    }
    fn children(&self, path: &str) -> Vec<(String, Node)> {
        self.rows::<NodeRow>(
            &format!("{NODE} WHERE n.parent = ? ORDER BY n.path"),
            vec![text(path)],
        )
        .into_iter()
        .map(NodeRow::pair)
        .collect()
    }
    fn subtree(&self, path: &str) -> Vec<(String, Node)> {
        let rows = if path == "/" {
            self.rows::<NodeRow>(&format!("{NODE} ORDER BY n.path"), vec![])
        } else {
            // Every path starting with `path/`: '0' is the character after '/'.
            self.rows::<NodeRow>(
                &format!("{NODE} WHERE n.path = ? OR (n.path >= ? AND n.path < ?) ORDER BY n.path"),
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
        #[derive(Deserialize)]
        struct Id {
            id: u64,
        }
        #[derive(Deserialize)]
        struct Shown {
            file: Option<String>,
            mtime: u64,
        }
        changes(path);
        let now = crate::auth::now();
        let json = |value: Option<String>| -> SqlStorageValue { value.into() };
        let values = vec![
            int(node.owner),
            int(node.group),
            int(node.mode),
            int(node.directory as u8),
            node.file.as_deref().into(),
            int(node.size),
            node.link.as_deref().into(),
            json(node.acl.as_ref().map(|a| serde_json::json!(a).to_string())),
            json(
                node.default_acl
                    .as_ref()
                    .map(|a| serde_json::json!(a).to_string()),
            ),
        ];
        let ino = if node.ino == 0 {
            let mut values = values;
            values.extend([int(now), int(now)]);
            self.rows::<Id>(
                "INSERT INTO inodes(owner, grp, mode, directory, file, size, link, acl, default_acl, mtime, ctime) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
                values,
            )
            .pop()
            .map_or(0, |i| i.id)
        } else {
            let shown = self
                .rows::<Shown>(
                    "SELECT file, mtime FROM inodes WHERE id = ?",
                    vec![int(node.ino)],
                )
                .pop();
            let (before, mtime) = shown.map_or((None, 0), |s| (s.file, s.mtime));
            // Its content changed (an upload completed over it): its mtime too; anything, its
            // ctime.
            let mtime = if before != node.file { now } else { mtime };
            let mut values = values;
            values.extend([int(mtime), int(now), int(node.ino)]);
            self.run(
                "UPDATE inodes SET owner = ?, grp = ?, mode = ?, directory = ?, file = ?, size = ?, link = ?, acl = ?, default_acl = ?, mtime = ?, ctime = ? WHERE id = ?",
                values,
            );
            // Every name of it shows the change.
            for name in self.names(node.ino) {
                changes(&name);
            }
            if before.is_some() && before != node.file {
                self.release(before);
            }
            node.ino
        };
        let old = self
            .rows::<Ino>("SELECT ino FROM nodes WHERE path = ?", vec![text(path)])
            .pop()
            .map(|r| r.ino);
        self.run(
            "INSERT INTO nodes(path, parent, ino) VALUES(?, ?, ?) ON CONFLICT(path) DO UPDATE SET ino = excluded.ino",
            vec![text(path), text(parent_of(path)), int(ino)],
        );
        if old != Some(ino) {
            self.entries_changed(path, now);
        }
        if let Some(old) = old
            && old != ino
        {
            self.forget(old);
        }
    }
    fn delete_node(&mut self, path: &str) {
        changes(path);
        let old = self
            .rows::<Ino>("SELECT ino FROM nodes WHERE path = ?", vec![text(path)])
            .pop()
            .map(|r| r.ino);
        self.run("DELETE FROM nodes WHERE path = ?", vec![text(path)]);
        self.entries_changed(path, crate::auth::now());
        if let Some(old) = old {
            self.forget(old);
        }
    }
    fn xattrs(&self, ino: u64) -> BTreeMap<String, Vec<u8>> {
        #[derive(Deserialize)]
        struct Attr {
            name: String,
            value: String,
        }
        self.rows::<Attr>(
            "SELECT name, value FROM xattrs WHERE ino = ? ORDER BY name",
            vec![int(ino)],
        )
        .into_iter()
        .filter_map(|a| Some((a.name, crate::files::decode_hex(&a.value)?)))
        .collect()
    }
    fn set_xattrs(&mut self, ino: u64, attrs: &BTreeMap<String, Vec<u8>>) {
        self.run("DELETE FROM xattrs WHERE ino = ?", vec![int(ino)]);
        for (name, value) in attrs {
            let hex: String = value.iter().map(|b| format!("{b:02x}")).collect();
            self.run(
                "INSERT INTO xattrs(ino, name, value) VALUES(?, ?, ?)",
                vec![int(ino), text(name), text(&hex)],
            );
        }
        for name in self.names(ino) {
            changes(&name);
        }
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
    fn home_of(&self, path: &str) -> Option<u32> {
        #[derive(Deserialize)]
        struct Id {
            id: u32,
        }
        self.rows::<Id>("SELECT id FROM users WHERE home = ?", vec![text(path)])
            .pop()
            .map(|user| user.id)
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
        self.set_expiry("session", key, session.expires);
    }
    fn delete_session(&mut self, key: &str) {
        self.end_sessions("key = ?", vec![text(key)]);
    }
    fn delete_sessions(&mut self, user: u32) {
        self.end_sessions("uid = ?", vec![int(user)]);
    }
    fn delete_expired_sessions(&mut self, now: u64) {
        self.end_sessions("expires <= ?", vec![int(now)]);
    }

    fn uploading(&self, file: &str) -> bool {
        self.file(file).is_some_and(|f| !f.complete())
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
