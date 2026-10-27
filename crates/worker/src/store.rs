use crate::db::{self, transact};
use crate::{api, auth, jobs};
use std::cell::Cell;
use std::collections::BTreeSet;
use terminal_worker_core::permissions::{EXEC, GUEST, GUEST_HOME, Node, READ, Store, User};
use worker::*;

/// The R2 object that marks the bucket as managed by an authority, so that a lost authority is
/// noticed instead of silently starting over with an empty filesystem.
const MARKER: &str = "system/terminal-authority-v2";

/// The one authority over accounts, sessions and the filesystem tree. Every decision is a
/// synchronous SQLite transaction, so requests never wait on each other's network I/O; file
/// bytes go to and from R2 in the Worker, never through here.
#[durable_object]
pub struct TerminalAuthority {
    state: State,
    env: Env,
    ready: Cell<bool>,
}
impl DurableObject for TerminalAuthority {
    fn new(state: State, env: Env) -> Self {
        Self {
            state,
            env,
            ready: Cell::new(false),
        }
    }
    async fn fetch(&self, mut req: Request) -> Result<Response> {
        self.ready().await?;
        if req.path() == "/api/events" {
            return self.listen(&req);
        }
        let reply = api::handle(&mut req, &self.env, &self.state).await?;
        self.schedule().await?;
        self.broadcast();
        reply.response()
    }
    async fn websocket_message(&self, _: WebSocket, _: WebSocketIncomingMessage) -> Result<()> {
        // Pages only listen; their pings are answered without waking this object.
        Ok(())
    }
    async fn websocket_close(&self, ws: WebSocket, _: usize, _: String, _: bool) -> Result<()> {
        let _ = ws.close(Some(1000), Some("closed"));
        Ok(())
    }
    async fn websocket_error(&self, _: WebSocket, _: Error) -> Result<()> {
        Ok(())
    }
    async fn alarm(&self) -> Result<Response> {
        self.ready().await?;
        let now = auth::now();
        transact(&self.state, move |db| {
            for (kind, id) in db.due(now) {
                match (kind.as_str(), db.file(&id), db.job(&id)) {
                    ("upload", Some(file), _) => api::cancel(db, &file),
                    ("job", _, Some(job)) if !job.finished() => {
                        db.add_lines(&id, &[(true, "Cancelled after waiting a week".into())]);
                        jobs::finish(db, &id, "cancelled");
                    }
                    _ => db.clear_expiry(&kind, &id),
                }
            }
            ((), true)
        })?;
        let bucket = self.env.bucket("FILES")?;
        loop {
            let mut keys = transact(&self.state, |db| (db.garbage(1000), false))?;
            if keys.is_empty() {
                break;
            }
            // A prefix is forgotten once nothing is listed under it.
            let (prefixes, objects): (Vec<String>, Vec<String>) =
                keys.iter().cloned().partition(|key| key.ends_with('/'));
            let mut doomed = objects;
            for prefix in prefixes {
                let listed = bucket.list().prefix(&prefix).limit(1000).execute().await?;
                if listed.truncated() {
                    keys.retain(|key| *key != prefix);
                }
                doomed.extend(listed.objects().iter().map(|o| o.key()));
            }
            if !doomed.is_empty() {
                bucket.delete_multiple(doomed).await?;
            }
            transact(&self.state, move |db| {
                db.forget_garbage(&keys);
                ((), true)
            })?;
        }
        // Uploads it let expire leave their directories.
        self.broadcast();
        db::take_alarm();
        if let Some(due) = transact(&self.state, |db| (db.next_due(), false))? {
            let delay = (due.saturating_sub(auth::now()) * 1000).max(1000);
            self.state
                .storage()
                .set_alarm(delay.min(i64::MAX as u64) as i64)
                .await?;
        }
        Response::ok("")
    }
}
impl TerminalAuthority {
    /// A page that listens for changes: a WebSocket, which keeps the session that opened it, so
    /// that it hears only of directories its account may list. Hibernation keeps idle ones free.
    fn listen(&self, req: &Request) -> Result<Response> {
        if !req
            .headers()
            .get("Upgrade")?
            .is_some_and(|upgrade| upgrade.eq_ignore_ascii_case("websocket"))
        {
            return Response::error("Expected a WebSocket", 426);
        }
        let pair = WebSocketPair::new()?;
        self.state.accept_web_socket(&pair.server);
        pair.server.serialize_attachment(auth::token_key(req))?;
        if self.state.get_websocket_auto_response().is_none()
            && let Ok(ping) = worker::worker_sys::WebSocketRequestResponsePair::new("ping", "pong")
        {
            self.state.set_websocket_auto_response(&ping);
        }
        Response::from_websocket(pair.client)
    }
    /// Tell the listening pages which directories changed, each only those its account may
    /// list, so that they show what others did as it happens.
    fn broadcast(&self) {
        let changed = db::take_changes();
        if changed.is_empty() {
            return;
        }
        let sockets = self.state.get_websockets();
        if sockets.is_empty() {
            return;
        }
        let keys: Vec<Option<String>> = sockets
            .iter()
            .map(|ws| {
                ws.deserialize_attachment::<Option<String>>()
                    .ok()
                    .flatten()
                    .flatten()
            })
            .collect();
        let visible = transact(&self.state, move |db| {
            let visible: Vec<Vec<String>> = keys
                .iter()
                .map(|key| {
                    let who = auth::identity(db, key.as_deref());
                    changed
                        .iter()
                        .filter(|dir| db.access(&who, dir, READ | EXEC).is_ok())
                        .cloned()
                        .collect()
                })
                .collect();
            (visible, false)
        });
        let Ok(visible) = visible else {
            return;
        };
        for (ws, dirs) in sockets.iter().zip(visible) {
            if !dirs.is_empty() {
                let _ = ws.send_with_str(serde_json::json!({ "changed": dirs }).to_string());
            }
        }
    }
    /// Run the alarm soon if a transaction left garbage or a deadline; it reschedules itself.
    async fn schedule(&self) -> Result<()> {
        if db::take_alarm() {
            self.state.storage().set_alarm(1000).await?;
        }
        Ok(())
    }
    async fn ready(&self) -> Result<()> {
        if self.ready.get() {
            return Ok(());
        }
        db::create(&self.state.storage().sql())?;
        if transact(&self.state, |db| (db.is_empty(), false))? {
            self.initialize().await?;
        }
        self.ready.set(true);
        Ok(())
    }
    /// Create root, the system groups and the top directories, the first time only.
    async fn initialize(&self) -> Result<()> {
        let bucket = self.env.bucket("FILES")?;
        if bucket.head(MARKER).await?.is_some() {
            return Err(Error::RustError("Permission catalog is missing for an initialized bucket; restore Durable Object storage instead of starting over".into()));
        }
        let password = self.env.secret("ADMIN_PASSWORD")?.to_string();
        let salt = auth::random_token();
        let hash = auth::hash(&password, &salt, &self.env).await?;
        transact(&self.state, move |db| {
            if !db.is_empty() {
                return ((), false);
            }
            db.set_user(
                "root",
                &User {
                    id: 0,
                    group: 0,
                    groups: BTreeSet::from([0]),
                    home: "/root".into(),
                    locked: false,
                    salt,
                    hash,
                },
            );
            // Who is signed out is guest, a real account that cannot sign in, as nobody is on
            // Linux; the accounts guests make are in its group, with homes in /guest, which
            // is shared, sticky as /tmp is.
            db.set_user(
                "guest",
                &User {
                    id: GUEST,
                    group: GUEST,
                    groups: BTreeSet::new(),
                    home: GUEST_HOME.into(),
                    locked: true,
                    salt: String::new(),
                    hash: String::new(),
                },
            );
            for (name, id) in [("root", 0), ("wheel", 27), ("guest", GUEST)] {
                db.set_group(name, id);
            }
            for (path, group, mode) in [
                ("/", 0, 0o755),
                ("/home", 0, 0o755),
                ("/root", 0, 0o700),
                (GUEST_HOME, GUEST, 0o1777),
            ] {
                db.set_node(path, &Node::directory(0, group, mode));
            }
            db.set_meta("schema", db::SCHEMA_VERSION);
            ((), true)
        })?;
        bucket
            .put(
                MARKER,
                "v2: the filesystem's metadata is managed by AUTHORITY; restore its storage if missing"
                    .to_string(),
            )
            .execute()
            .await?;
        Ok(())
    }
}
