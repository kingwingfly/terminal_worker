//! File descriptors (owner's design, 2026-10-29): what open(2) gives a session, each browser
//! being one, all of whose pages may use it, as threads share a process's. A descriptor reads,
//! or writes a file in chunks (`pwrite` on the Worker, files.rs), which shows once complete
//! (`close`); a recording (`O_APPEND`) grows from its end, readable as it grows. Uploads,
//! saves, recordings and copies all go through them.
//!
//! A writer keeps its descriptor (`lease`) every few seconds while its page is there. One not
//! kept for `LEASE` may be taken over: the same account opening the same unfinished file, of
//! the same content, goes on with it, from any page, and the old descriptor writes no more
//! (EBADF). One not kept for `IDLE` is closed.
use crate::api::{
    RECORDING_CHUNK, RECORDING_IDLE, RECORDING_MAX, UPLOAD_TTL, chunk_key, chunk_of,
    finish_recording,
};
use crate::auth;
use crate::db::{Db, File, fd_key};
use abi::{
    Call, Closed, Errno, Error, FICLONE, Fd, O_ACCMODE, O_APPEND, O_RDONLY, O_TRUNC, Opened,
};
use kernel::{Fs, Opening, Process};
use serde_json::{Value, json};
use terminal_worker_core::permissions::{Identity, Node, Store, WRITE};
use terminal_worker_core::{CHUNK_SIZE, MAX_FILE, mime};

/// How long a writer's file stays its own after it last kept its descriptor: every few seconds
/// while its page is there (web/upload.mjs).
pub const LEASE: u64 = 20;
/// How long a descriptor not kept stays open.
pub const IDLE: u64 = 60 * 60;
/// Why a descriptor writes no more: another took its file over.
const ELSEWHERE: &str = "the upload goes on from another tab or device";

type Result<T> = std::result::Result<T, Error>;

/// The descriptor calls of the session `sid`, as `process`.
pub fn call(db: &mut Db, sid: Option<&str>, process: &Process, call: Call) -> Result<Value> {
    let sid = sid.ok_or_else(|| Error::because(Errno::EBADF, "no session to keep descriptors"))?;
    match call {
        Call::Open {
            path,
            flags,
            mode,
            size,
            sha256,
            version,
        } => open(db, sid, process, &path, flags, mode, size, sha256, version).map(|o| json!(o)),
        Call::Close { fd } => close(db, sid, &process.cred, fd).map(|c| json!(c)),
        Call::Lease { fd } => lease(db, sid, fd).map(|()| Value::Null),
        Call::Fds => Ok(json!(fds(db, sid))),
        Call::Ioctl { fd, request, arg } => match crate::dev::request(db, sid, fd)? {
            Some(device) => crate::dev::ioctl(db, process, device, request, arg),
            None if request == FICLONE => {
                clone(db, sid, &process.cred, fd, arg.as_u64().unwrap_or(u64::MAX))
                    .map(|()| Value::Null)
            }
            None => Err(Errno::ENOTTY.into()),
        },
        _ => Err(Errno::ENOSYS.into()),
    }
}

/// The descriptor `fd` of `sid`, and the file it still reads or writes.
fn descriptor(db: &Db, sid: &str, fd: u32) -> Result<(crate::db::Fd, String)> {
    let found = db.fd(sid, fd).ok_or(Errno::EBADF)?;
    let file = found
        .file
        .clone()
        .ok_or_else(|| Error::because(Errno::EBADF, ELSEWHERE))?;
    Ok((found, file))
}

/// The file a writer writes, which must still be unfinished: one cancelled, removed or done is
/// written no more.
fn unfinished(db: &Db, file: &str) -> Result<File> {
    db.file(file)
        .filter(|file| !file.complete())
        .ok_or_else(|| {
            Error::because(
                Errno::EBADF,
                "the file was removed, or its writing ended, elsewhere",
            )
        })
}

#[allow(clippy::too_many_arguments)]
fn open(
    db: &mut Db,
    sid: &str,
    process: &Process,
    path: &str,
    flags: u32,
    mode: u16,
    size: Option<u64>,
    sha256: Option<String>,
    version: Option<String>,
) -> Result<Opened> {
    let cred = &process.cred;
    let now = auth::now();
    let opening = kernel::open(db, process, path, flags, mode)?;
    // A device is its driver's to open.
    if let Opening::Existing { path, node } = &opening
        && node.is_device()
    {
        return crate::dev::open(db, sid, path, flags);
    }
    if flags & O_ACCMODE == O_RDONLY {
        let (path, node) = match opening {
            Opening::Existing { path, node } => (path, node),
            // O_CREAT for reading makes an empty file, as on Linux.
            Opening::New { path, mut node } => {
                node.file = Some(db.empty(cred.id, &path));
                db.set_node(&path, &node);
                (path, node)
            }
        };
        let file = match node.file {
            Some(file) => file,
            None => db.empty(cred.id, &path),
        };
        let fd = db.add_fd(sid, Some(&file), &path, flags, false, now);
        db.set_expiry("fd", &fd_key(sid, fd), now + IDLE);
        return Ok(Opened {
            fd,
            size: node.size,
            ..Opened::default()
        });
    }
    let append = flags & O_APPEND != 0;
    let size = if append {
        0
    } else {
        size.ok_or_else(|| Error::because(Errno::EINVAL, "a file is written whole: give its size"))?
    };
    if size > MAX_FILE {
        return Err(Errno::EFBIG.into());
    }
    if sha256
        .as_ref()
        .is_some_and(|h| h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(Error::because(
            Errno::EINVAL,
            "a SHA-256 is 64 hexadecimal digits",
        ));
    }
    let (path, node, base) = match opening {
        Opening::New { path, node } => (path, node, None),
        Opening::Existing { path, node } => {
            let current = node.file.as_deref().and_then(|id| db.file(id));
            if let Some(file) = current.filter(|file| !file.complete()) {
                // The same unfinished file, by the same account, to the same place: go on with
                // it, from this page or another, unless one writes it now.
                if !append
                    && sha256.is_some()
                    && !file.recording()
                    && file.uploader == cred.id
                    && file.size == size
                    && file.sha256 == sha256
                {
                    return resume(db, sid, &path, flags, &file, now);
                }
                if flags & O_TRUNC == 0 {
                    return Err(Error::because(
                        Errno::EBUSY,
                        "a file of this name is still being written",
                    ));
                }
            }
            // Content is written whole: what is there is replaced, or not written.
            if append || flags & O_TRUNC == 0 {
                return Err(Errno::EEXIST.into());
            }
            if version
                .as_deref()
                .is_some_and(|v| Some(v) != node.file.as_deref().map(crate::db::etag).as_deref())
            {
                return Err(Errno::ESTALE.into());
            }
            let base = node.file.clone();
            (path, node, Some(base))
        }
    };
    let chunk_size = if append { RECORDING_CHUNK } else { CHUNK_SIZE };
    let id = db.create_file(
        size,
        chunk_size,
        sha256.as_deref(),
        cred.id,
        &path,
        base.clone().flatten().as_deref(),
        now,
    );
    let created = base.is_none();
    // A new file is there at once, with its mode, as it arrives; one replaced shows what it
    // had until the new content is whole.
    if created {
        db.set_node(
            &path,
            &Node {
                file: Some(id.clone()),
                size,
                ..node
            },
        );
    }
    if append {
        db.record_file(&id);
        db.set_expiry("upload", &id, now + RECORDING_IDLE);
    } else {
        db.set_expiry("upload", &id, now + UPLOAD_TTL);
    }
    let fd = db.add_fd(sid, Some(&id), &path, flags, created, now);
    db.set_expiry("fd", &fd_key(sid, fd), now + IDLE);
    Ok(Opened {
        fd,
        size,
        chunk_size,
        created,
        ..Opened::default()
    })
}

/// Go on with the unfinished `file`: unless its writer kept it lately, this session's new
/// descriptor writes it, and that one no more.
fn resume(db: &mut Db, sid: &str, path: &str, flags: u32, file: &File, now: u64) -> Result<Opened> {
    if db
        .writer(&file.id)
        .is_some_and(|writer| now < writer.leased + LEASE)
    {
        return Err(Error::because(
            Errno::EBUSY,
            format!(
                "{path} is being uploaded from another tab or device; wait for it to finish, or stop it there"
            ),
        ));
    }
    db.revoke_writers(&file.id);
    let created = file.base.is_none();
    let fd = db.add_fd(sid, Some(&file.id), path, flags, created, now);
    db.set_expiry("fd", &fd_key(sid, fd), now + IDLE);
    db.set_expiry("upload", &file.id, now + UPLOAD_TTL);
    Ok(Opened {
        fd,
        size: file.size,
        chunk_size: file.chunk_size,
        committed: db.chunks(&file.id).into_iter().map(|c| c.idx).collect(),
        created,
        resumed: true,
    })
}

/// Where the unfinished `file` is written, and the node there, if `who` may still write it:
/// every chunk and the close ask again, as permissions may have changed since it was opened. A
/// new file is written at its node, wherever it was moved, while `who` may reach it and owns
/// it, as one who made a file goes on writing it, or may write it. A file replaced is written
/// while `who` may write it at its path, and it is still the one replaced; once it is not, the
/// new content can never show, and goes.
pub fn writable(db: &mut Db, who: &Identity, file: &File) -> Result<(String, Node)> {
    let path = match &file.base {
        Some(_) => file.target.clone(),
        None => db.showing(&file.id).pop().ok_or(Errno::EBADF)?,
    };
    // A name `who` may not reach is not told of.
    kernel::namei(db, who, &path, false)?;
    match (&file.base, db.node(&path)) {
        (Some(base), Some(node)) if node.file.as_ref() == Some(base) => {
            if !kernel::permission(who, &node, WRITE) {
                return Err(Errno::EACCES.into());
            }
            Ok((path, node))
        }
        (Some(_), _) => {
            db.drop_file(&file.id);
            Err(Errno::ESTALE.into())
        }
        (None, Some(node)) if node.owner == who.id || kernel::permission(who, &node, WRITE) => {
            Ok((path, node))
        }
        (None, _) => Err(Errno::EACCES.into()),
    }
}

/// close(2). A writer's file is complete once every chunk is there; if one is missing, the
/// descriptor goes all the same, with EIO, and the file waits for a writer to go on with it. A
/// recording is complete with what arrived.
fn close(db: &mut Db, sid: &str, who: &Identity, fd: u32) -> Result<Closed> {
    let found = db.fd(sid, fd).ok_or(Errno::EBADF)?;
    release(db, sid, fd);
    if found.file.is_none() {
        return match found.device {
            Some(device) => Ok(Closed {
                path: device,
                ..Closed::default()
            }),
            None if found.path.starts_with("/dev/") => {
                Err(Error::because(Errno::EBADF, "the device was hung up"))
            }
            None => Err(Error::because(Errno::EBADF, ELSEWHERE)),
        };
    }
    let closed = closing(db, who, &found);
    // A new file removed while it was written goes with its last descriptor, as an unlinked
    // file does on Linux.
    if let Some(file) = found.file.as_deref().and_then(|id| db.file(id))
        && found.writes()
        && !file.complete()
        && file.base.is_none()
        && db.showing(&file.id).is_empty()
        && db.writer(&file.id).is_none()
    {
        db.drop_file(&file.id);
    }
    closed
}
/// Close the descriptor `fd` of `sid`, however it ends (close, its lease out): a device's
/// driver hears so.
pub fn release(db: &mut Db, sid: &str, fd: u32) {
    if let Some(device) = db.fd(sid, fd).and_then(|found| found.device) {
        crate::dev::closed(db, sid, fd, &device);
    }
    db.close_fd(sid, fd);
}
fn closing(db: &mut Db, who: &Identity, found: &crate::db::Fd) -> Result<Closed> {
    let file = found
        .file
        .clone()
        .ok_or_else(|| Error::because(Errno::EBADF, ELSEWHERE))?;
    let Some(file) = db.file(&file) else {
        return Err(Error::because(
            Errno::EBADF,
            "the file was removed, or its writing ended, elsewhere",
        ));
    };
    let closed = |path: String, file: &File| Closed {
        path,
        size: file.size,
        version: file.etag(),
        created: found.created,
    };
    if !found.writes() || file.complete() {
        return Ok(closed(found.path.clone(), &file));
    }
    if file.recording() {
        let path = db.showing(&file.id).pop().unwrap_or(found.path.clone());
        finish_recording(db, &file);
        let size = db.file(&file.id).map_or(0, |f| f.size);
        return Ok(Closed {
            size,
            ..closed(path, &file)
        });
    }
    let (path, node) = writable(db, who, &file)?;
    if db.chunks(&file.id).len() as u64 != file.chunk_count() {
        return Err(Error::because(
            Errno::EIO,
            "chunks are missing; open the file again to go on with it",
        ));
    }
    if file.base.is_some() {
        db.set_node(
            &path,
            &Node {
                file: Some(file.id.clone()),
                size: file.size,
                ..node
            },
        );
    }
    db.finish_file(&file.id);
    Ok(closed(path, &file))
}

fn lease(db: &mut Db, sid: &str, fd: u32) -> Result<()> {
    let now = auth::now();
    if let Some(device) = crate::dev::request(db, sid, fd)? {
        crate::dev::leased(db, sid, fd, &device.device.name);
        db.lease_fd(sid, fd, now);
        db.set_expiry("fd", &fd_key(sid, fd), now + IDLE);
        return Ok(());
    }
    let (found, file) = descriptor(db, sid, fd)?;
    if found.writes() {
        let file = unfinished(db, &file)?;
        // A recording lasts while its page is heard.
        if file.recording() {
            db.set_expiry("upload", &file.id, now + RECORDING_IDLE);
        }
    }
    db.lease_fd(sid, fd, now);
    db.set_expiry("fd", &fd_key(sid, fd), now + IDLE);
    Ok(())
}

/// The descriptors of `sid` that still read or write something.
fn fds(db: &Db, sid: &str) -> Vec<Fd> {
    db.fds_of(sid)
        .into_iter()
        .filter_map(|found| {
            if let Some(device) = &found.device {
                return Some(Fd {
                    fd: found.fd,
                    path: device.clone(),
                    flags: found.flags,
                    ..Fd::default()
                });
            }
            let file = db.file(found.file.as_deref()?)?;
            Some(Fd {
                fd: found.fd,
                path: if found.writes() {
                    db.showing(&file.id).pop().unwrap_or(file.target.clone())
                } else {
                    found.path.clone()
                },
                flags: found.flags,
                size: file.size,
                received: db.received(&file),
                created: found.created,
            })
        })
        .collect()
}

/// ioctl(fd, FICLONE, source): the file `fd` writes, nothing of it written yet, takes the
/// content `source` reads, shared, at once, as a reflink copy does.
fn clone(db: &mut Db, sid: &str, who: &Identity, fd: u32, source: u64) -> Result<()> {
    let (writer, target) = descriptor(db, sid, fd)?;
    let (reader, content) = descriptor(db, sid, source as u32)?;
    if !writer.writes() || reader.flags & O_ACCMODE == abi::O_WRONLY {
        return Err(Errno::EBADF.into());
    }
    let content = db.file(&content).ok_or(Errno::EBADF)?;
    if !content.complete() {
        return Err(Error::because(
            Errno::EBUSY,
            format!(
                "{} is still uploading; copy it once it completes",
                reader.path
            ),
        ));
    }
    let target = unfinished(db, &target)?;
    if target.recording() || !db.chunks(&target.id).is_empty() {
        return Err(Errno::EINVAL.into());
    }
    let (path, node) = writable(db, who, &target)?;
    db.set_fd_file(sid, fd, &content.id);
    db.set_node(
        &path,
        &Node {
            file: Some(content.id.clone()),
            size: content.size,
            ..node
        },
    );
    // What it was writing, which nothing shows any more, if replacing took it with it.
    if db.file(&target.id).is_some() {
        db.drop_file(&target.id);
    }
    Ok(())
}

/// What pwrite(2) of chunk `index`, `length` bytes, through the descriptor `fd` of `sid`, is
/// written to: the R2 object for it, and the file's type. A file written whole takes each
/// chunk at its index, of its length; a recording, its next, whole or, last, short.
pub fn pwrite(
    db: &mut Db,
    sid: &str,
    who: &Identity,
    fd: u32,
    index: u64,
    length: u64,
) -> Result<(String, String)> {
    let (found, file) = descriptor(db, sid, fd)?;
    if !found.writes() {
        return Err(Errno::EBADF.into());
    }
    let file = unfinished(db, &file)?;
    if file.recording() {
        if index != file.chunk_count() || !file.size.is_multiple_of(file.chunk_size) {
            return Err(Error::because(
                Errno::EINVAL,
                "a recording takes its next chunk",
            ));
        }
        if length == 0 || length > file.chunk_size {
            return Err(Error::because(
                Errno::EINVAL,
                format!("a recording's chunk is at most {} bytes", file.chunk_size),
            ));
        }
        if file.size + length > RECORDING_MAX {
            return Err(Errno::EFBIG.into());
        }
    } else {
        if index >= file.chunk_count() {
            return Err(Error::because(Errno::EINVAL, "chunk index is out of range"));
        }
        if length != file.chunk_len(index) {
            return Err(Error::because(
                Errno::EINVAL,
                format!("chunk {index} must be {} bytes", file.chunk_len(index)),
            ));
        }
    }
    let (path, _) = writable(db, who, &file)?;
    db.lease_fd(sid, fd, auth::now());
    Ok((chunk_key(&file.id, index), mime(&path).to_string()))
}

/// Record chunk `index`, written to the R2 object `key`, as part of the file `fd` writes. A
/// chunk is committed once: what readers have seen of it never changes, and an object written
/// for nothing goes to the garbage.
#[allow(clippy::too_many_arguments)]
pub fn written(
    db: &mut Db,
    sid: &str,
    who: &Identity,
    fd: u32,
    index: u64,
    key: &str,
    sha256: Option<&str>,
    length: u64,
) -> Result<Value> {
    let refused = |db: &mut Db, error: Error| {
        db.discard(key);
        Err(error)
    };
    let (found, file) = match descriptor(db, sid, fd) {
        Ok(found) => found,
        Err(error) => return refused(db, error),
    };
    if !chunk_of(key, &file, index) || !found.writes() {
        return refused(db, Errno::EINVAL.into());
    }
    let file = match unfinished(db, &file) {
        Ok(file) => file,
        Err(error) => return refused(db, error),
    };
    if let Err(error) = writable(db, who, &file) {
        return refused(db, error);
    }
    let now = auth::now();
    if file.recording() {
        if index != file.chunk_count() || file.size + length > RECORDING_MAX {
            return refused(db, Errno::EINVAL.into());
        }
        db.add_chunk(&file.id, index, key, sha256, now);
        db.grow_file(&file.id, index * file.chunk_size + length, None);
        db.set_expiry("upload", &file.id, now + RECORDING_IDLE);
        return Ok(json!({ "committed": index }));
    }
    match db.committed_hash(&file.id, index) {
        // Sent again: the chunk committed first stays.
        Some(hash) if hash.is_none() || sha256.is_none() || hash.as_deref() == sha256 => {
            db.discard(key);
            Ok(json!({ "committed": index }))
        }
        Some(_) => refused(
            db,
            Error::because(
                Errno::EEXIST,
                format!("chunk {index} was already written with other content"),
            ),
        ),
        None => {
            db.add_chunk(&file.id, index, key, sha256, now);
            db.set_expiry("upload", &file.id, now + UPLOAD_TTL);
            Ok(json!({ "committed": index }))
        }
    }
}
