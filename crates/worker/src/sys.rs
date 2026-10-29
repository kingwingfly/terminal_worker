//! The system-call entry (owner's design, 2026-10-29): `POST /api/sys` takes a batch of calls
//! (abi::Batch) from user space, the page, and runs each as its session's account (root under
//! sudo), in order, in a transaction of its own, as separate system calls are: one that fails
//! changes nothing and stops nothing. What the calls do is the kernel's (crates/kernel), over
//! the authority's SQLite.
use crate::api::Reply;
use crate::auth;
use crate::db::{Db, transact};
use abi::{BATCH_MAX, Batch, Call, Creds, Errno, Error, Results, Return};
use kernel::Process;
use serde_json::json;
use terminal_worker_core::permissions::*;
use worker::*;

pub async fn handle(req: &mut Request, state: &State, key: Option<String>) -> Result<Reply> {
    let Ok(batch) = req.json::<Batch>().await else {
        return Ok(Reply::error("Invalid system call batch", 400));
    };
    if batch.calls.len() > BATCH_MAX {
        return Ok(Reply::error(
            &format!("A batch takes at most {BATCH_MAX} calls"),
            400,
        ));
    }
    let (sudo, umask) = (batch.sudo, batch.umask & 0o777);
    let guest = auth::guest_key(req);
    let mut results = Vec::with_capacity(batch.calls.len());
    for call in batch.calls {
        let (key, guest) = (key.clone(), guest.clone());
        let result = transact(state, move |db| {
            // A descriptor call stands as it ends, failed or not: a close that fails closes.
            let descriptors = matches!(
                call,
                Call::Open { .. } | Call::Close { .. } | Call::Lease { .. } | Call::Ioctl { .. }
            );
            let sid = auth::sid(db, key.as_deref(), guest.as_deref());
            let result = run(db, key.as_deref(), sid.as_deref(), sudo, umask, call);
            let commit = result.is_ok() || descriptors;
            let result = match result {
                Ok(value) => Return::Ok(value),
                Err(error) => Return::Err(error),
            };
            (result, commit)
        })?;
        results.push(result);
    }
    Ok(Reply::ok(json!(Results { results })))
}

/// The call `call`, as the session `key`'s process (`sid`, signed in or not), under sudo if
/// `sudo`.
fn run(
    db: &mut Db,
    key: Option<&str>,
    sid: Option<&str>,
    sudo: bool,
    umask: u16,
    call: Call,
) -> std::result::Result<serde_json::Value, Error> {
    let cred = auth::elevated(db, key, sudo).map_err(|why| {
        // Not a wheel member: never. One that has not authenticated lately: not yet.
        let errno = if db.can_sudo(&auth::identity(db, key)) {
            Errno::EACCES
        } else {
            Errno::EPERM
        };
        Error::because(errno, why)
    })?;
    let process = Process { cred, umask };
    match call {
        Call::Getcreds => Ok(json!(creds(db, key, &process.cred))),
        call @ (Call::Open { .. }
        | Call::Close { .. }
        | Call::Lease { .. }
        | Call::Fds
        | Call::Ioctl { .. }) => crate::fds::call(db, sid, &process, call),
        call => kernel::call(db, &process, call),
    }
}

/// The files the kernel writes as they are read, as /proc's are: /etc/passwd and /etc/group,
/// what libc's getpwnam and getgrnam read, which user space's programs do to name accounts.
pub fn generated(db: &Db, path: &str) -> Option<String> {
    let mut users = db.users();
    users.sort_by_key(|(_, user)| user.id);
    match path {
        "/etc/passwd" => Some(
            users
                .iter()
                .map(|(name, user)| {
                    format!(
                        "{name}:x:{}:{}::{}:/bin/sh\n",
                        user.id, user.group, user.home
                    )
                })
                .collect(),
        ),
        "/etc/group" => {
            let mut groups = db.groups();
            groups.sort_by_key(|(_, id)| *id);
            Some(
                groups
                    .iter()
                    .map(|(name, id)| {
                        let members: Vec<&str> = users
                            .iter()
                            .filter(|(_, user)| user.groups.contains(id))
                            .map(|(name, _)| name.as_str())
                            .collect();
                        format!("{name}:x:{id}:{}\n", members.join(","))
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

/// Make /etc and the files the kernel writes in it, root's, where they are not.
pub fn ensure(db: &mut Db) {
    if db.node("/etc").is_none() {
        db.set_node("/etc", &Node::directory(0, 0, 0o755));
    }
    for path in ["/etc/passwd", "/etc/group"] {
        if db.node(path).is_none() {
            let file = kernel::Fs::empty(db, 0, path);
            db.set_node(path, &Node::file(0, 0, 0o644, file, 0));
        }
    }
}

/// Who the process is.
fn creds(db: &Db, key: Option<&str>, cred: &Identity) -> Creds {
    let view = auth::session_view(db, key);
    let home = db
        .user_by_id(cred.id)
        .map(|(_, user)| user.home)
        .unwrap_or_else(|| "/".into());
    Creds {
        uid: cred.id,
        gid: cred.group,
        groups: cred.groups.iter().copied().collect(),
        user: db.username(cred.id),
        group: db.groupname(cred.group),
        group_names: cred.groups.iter().map(|id| db.groupname(*id)).collect(),
        home,
        signed_in: cred.id != GUEST,
        can_sudo: db.can_sudo(cred),
        sudo_active: view["sudo_active"].as_bool().unwrap_or(false),
    }
}
