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
    let mut results = Vec::with_capacity(batch.calls.len());
    for call in batch.calls {
        let key = key.clone();
        let result = transact(state, move |db| {
            let result = run(db, key.as_deref(), sudo, umask, call);
            let commit = result.is_ok();
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

/// The call `call`, as the session `key`'s process, under sudo if `sudo`.
fn run(
    db: &mut Db,
    key: Option<&str>,
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
    match call {
        Call::Getcreds => Ok(json!(creds(db, key, &cred))),
        call => kernel::call(db, &Process { cred, umask }, call),
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
