use crate::db::{self, transact};
use crate::{api, auth, devices, jobs};
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
    async fn websocket_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        // Pings are answered without waking this object; what else a page says is about the
        // decks it shows.
        if let WebSocketIncomingMessage::String(text) = message {
            self.ready().await?;
            self.heard(&ws, &text);
            self.schedule().await?;
        }
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
                    ("upload", Some(file), _) if file.recording() => {
                        api::finish_recording(db, &file)
                    }
                    ("upload", Some(file), _) => api::cancel(db, &file),
                    ("job", _, Some(job)) if !job.finished() => {
                        let why = if job.state == "question" {
                            "Cancelled: its question went unanswered for an hour"
                        } else {
                            "Cancelled after waiting a week"
                        };
                        db.add_lines(&id, &[(true, why.into())]);
                        jobs::finish(db, &id, "cancelled");
                    }
                    // A session ends when it expires, and the pages holding it hear so.
                    ("session", _, _) => {
                        db.delete_session(&id);
                        db.clear_expiry(&kind, &id);
                    }
                    // So does a presentation, unturned for long; its followers hear so.
                    ("device", _, _) => devices::end(db, &id),
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
        // A cookie whose session is gone is a guest's, which has nothing to end.
        let key = auth::token_key(req);
        let live = transact(&self.state, {
            let key = key.clone();
            move |db| (auth::identity(db, key.as_deref()).id != GUEST, false)
        })?;
        pair.server.serialize_attachment(Line {
            session: key.filter(|_| live),
            follows: Vec::new(),
        })?;
        if self.state.get_websocket_auto_response().is_none()
            && let Ok(ping) = worker::worker_sys::WebSocketRequestResponsePair::new("ping", "pong")
        {
            self.state.set_websocket_auto_response(&ping);
        }
        Response::from_websocket(pair.client)
    }
    /// Tell the listening pages which directories changed, each only those its account may
    /// list, so that they show what others did as it happens; those whose session ended (signed
    /// out elsewhere, a password changed, the account locked or removed, expired) that it did,
    /// once, as their account is a guest's from then on; and those following a presentation
    /// that ended, that it did.
    fn broadcast(&self) {
        let changed = db::take_changes();
        let ended = db::take_ended();
        let removed = db::take_removed();
        if changed.is_empty() && !ended && removed.is_empty() {
            return;
        }
        let sockets = self.state.get_websockets();
        if sockets.is_empty() {
            return;
        }
        let lines: Vec<Line> = sockets.iter().map(Line::of).collect();
        let keys: Vec<Option<String>> = lines.iter().map(|line| line.session.clone()).collect();
        let visible = transact(&self.state, move |db| {
            let visible: Vec<(bool, Vec<String>)> = keys
                .iter()
                .map(|key| {
                    let who = auth::identity(db, key.as_deref());
                    let gone = ended && key.is_some() && who.id == GUEST;
                    let dirs = changed
                        .iter()
                        .filter(|dir| db.access(&who, dir, READ | EXEC).is_ok())
                        .cloned()
                        .collect();
                    (gone, dirs)
                })
                .collect();
            (visible, false)
        });
        let Ok(visible) = visible else {
            return;
        };
        for ((ws, mut line), (gone, dirs)) in sockets.iter().zip(lines).zip(visible) {
            if gone {
                line.session = None;
                line.keep(ws);
                let _ = ws.send_with_str(serde_json::json!({ "session": "ended" }).to_string());
            }
            let (ended, following): (Vec<String>, Vec<String>) = std::mem::take(&mut line.follows)
                .into_iter()
                .partition(|device| removed.iter().any(|(name, _)| name == device));
            line.follows = following;
            if !ended.is_empty() {
                line.keep(ws);
            }
            // Ended, or replaced by one that shows something else, which the page follows
            // again, asking whether it may.
            for device in ended {
                let replaced = removed
                    .iter()
                    .any(|(name, again)| *name == device && *again);
                let mut said = serde_json::json!({ "name": device });
                said[if replaced { "replaced" } else { "ended" }] = true.into();
                let _ = ws.send_with_str(serde_json::json!({ "device": said }).to_string());
            }
            if !dirs.is_empty() {
                let _ = ws.send_with_str(serde_json::json!({ "changed": dirs }).to_string());
            }
        }
    }
    /// What a page says on its line, about the deck or the presented PDF it shows:
    ///
    /// - `{"present": {"path", "at": [column, row]}}`: it shows that place (a PDF's page and 0).
    ///   Kept as where its account last showed that file, it goes to the account's pages signed
    ///   in elsewhere, in other browsers (this browser's other windows hear it from the page),
    ///   and to whoever follows a presentation of that file by the account (devices.rs), guests
    ///   included. Guests, who are anyone signed out, say nothing to anyone.
    /// - `{"where": path}`: where its account last showed that file, which it opens at.
    /// - `{"follow": [name...]}`: it follows these devices, a presentation /dev/present/NAME and
    ///   a sound /dev/snd/NAME, and no others; it is told where a presentation is, and hears when
    ///   one ends.
    fn heard(&self, from: &WebSocket, text: &str) {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum Said {
            Present(Place),
            Where(String),
            Follow(Vec<String>),
        }
        #[derive(serde::Deserialize, serde::Serialize)]
        struct Place {
            path: String,
            at: [u32; 2],
        }
        let reply = |ws: &WebSocket, message: serde_json::Value| {
            let _ = ws.send_with_str(message.to_string());
        };
        if text.len() > 8192 {
            return;
        }
        let Ok(said) = serde_json::from_str::<Said>(text) else {
            return;
        };
        let mut line = Line::of(from);
        match said {
            Said::Present(place) => {
                let Some(key) = line.session else {
                    return;
                };
                if terminal_worker_core::resolve("/", &place.path)
                    .ok()
                    .as_ref()
                    != Some(&place.path)
                {
                    return;
                }
                let sockets = self.state.get_websockets();
                let lines: Vec<Line> = sockets.iter().map(Line::of).collect();
                let path = place.path.clone();
                let at = (place.at[0], place.at[1]);
                let told = transact(&self.state, move |db| {
                    let who = auth::identity(db, Some(&key));
                    // Only a file the account may read is kept as shown, or said to anyone.
                    let readable = db
                        .physical(&who, &path, true)
                        .and_then(|real| db.access(&who, &real, READ));
                    if who.id == GUEST || readable.is_err() {
                        return (Vec::new(), false);
                    }
                    db.set_place(who.id, &path, at);
                    let presented: Vec<String> = db
                        .presenting(who.id, &path)
                        .into_iter()
                        .map(|device| device.name)
                        .collect();
                    for device in &presented {
                        db.set_expiry("device", device, auth::now() + devices::PRESENT_TTL);
                    }
                    // Whether each account may follow each device, asked once per turn: an
                    // audience of guests is one account.
                    let mut may = std::collections::BTreeMap::<(u32, &str), bool>::new();
                    let told = lines
                        .iter()
                        .map(|line| {
                            let other = line.session.as_deref().filter(|other| *other != key);
                            let watching: Vec<&String> = line
                                .follows
                                .iter()
                                .filter(|device| presented.contains(device))
                                .collect();
                            if other.is_none() && watching.is_empty() {
                                return false;
                            }
                            let reader = auth::identity(db, line.session.as_deref());
                            (other.is_some() && reader.id == who.id)
                                || watching.into_iter().any(|device| {
                                    *may.entry((reader.id, device.as_str())).or_insert_with(|| {
                                        devices::followed(db, &reader, device).is_ok()
                                    })
                                })
                        })
                        .collect();
                    (told, true)
                });
                let message = serde_json::json!({ "present": place });
                for (ws, told) in sockets.iter().zip(told.unwrap_or_default()) {
                    if told {
                        reply(ws, message.clone());
                    }
                }
            }
            Said::Where(path) => {
                let Some(key) = line.session else {
                    return;
                };
                let asked = path.clone();
                let place = transact(&self.state, move |db| {
                    let who = auth::identity(db, Some(&key));
                    let place = (who.id != GUEST)
                        .then(|| db.place(who.id, &asked))
                        .flatten();
                    (place, false)
                });
                if let Ok(Some((column, row))) = place {
                    reply(
                        from,
                        serde_json::json!({ "present": { "path": path, "at": [column, row] } }),
                    );
                }
            }
            Said::Follow(names) => {
                let key = line.session.clone();
                let asked: Vec<String> = names.into_iter().take(4).collect();
                let followed = transact(&self.state, move |db| {
                    let who = auth::identity(db, key.as_deref());
                    let followed: Vec<_> = asked
                        .into_iter()
                        .map(|name| {
                            let device = devices::followed(db, &who, &name)
                                .map(|device| (db.place(device.owner, &device.path), device.path));
                            (name, device)
                        })
                        .collect();
                    (followed, false)
                });
                let Ok(followed) = followed else {
                    return;
                };
                line.follows.clear();
                for (name, device) in followed {
                    match device {
                        Ok((place, path)) => {
                            line.follows.push(name);
                            if let Some((column, row)) = place {
                                reply(
                                    from,
                                    serde_json::json!({ "present": { "path": path, "at": [column, row] } }),
                                );
                            }
                        }
                        Err(error) => reply(
                            from,
                            serde_json::json!({ "device": { "name": name, "error": error } }),
                        ),
                    }
                }
                line.keep(from);
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

/// What a page's line keeps: the session it was opened with (none for a guest's, or once that
/// session ended), and the devices it follows: a presentation, a sound.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Line {
    session: Option<String>,
    #[serde(default)]
    follows: Vec<String>,
}
impl Line {
    fn of(ws: &WebSocket) -> Self {
        ws.deserialize_attachment::<Line>()
            .ok()
            .flatten()
            // A line opened before lines kept more than their session.
            .or_else(|| {
                let session = ws.deserialize_attachment::<Option<String>>().ok()??;
                Some(Line {
                    session,
                    follows: Vec::new(),
                })
            })
            .unwrap_or_default()
    }
    fn keep(&self, ws: &WebSocket) {
        let _ = ws.serialize_attachment(self);
    }
}
