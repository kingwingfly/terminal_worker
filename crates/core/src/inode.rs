//! The rules Linux applies to one inode: who may open, create, remove, link and change it, and
//! what else changes with that, as `fs/namei.c` and `fs/attr.c` decide. The server's kernel and
//! the VFS drivers (`os-vfs`) both apply these, so their answers agree.
//!
//! Each `may_*` function answers one question with the errno Linux gives; none changes
//! anything. The functions that change a [`Node`] change only what the rule says.

use crate::permissions::{
    EXEC, Identity, LINK_TARGET_MAX, Node, READ, SETGID, SETUID, STICKY, WRITE,
};
use abi::{Errno, Time};

/// Whether `cred` may `mask` (READ, WRITE, EXEC) the node, as Linux's generic_permission
/// decides: its mode and ACL say ([`Identity::allows`]); root may anything, but execute only
/// what someone may.
pub fn permission(cred: &Identity, node: &Node, mask: u16) -> bool {
    if cred.id == 0 {
        return mask & EXEC == 0 || node.directory || node.mode & 0o111 != 0;
    }
    cred.allows(node, mask)
}

/// [`permission`], as an errno: EACCES.
pub fn require(cred: &Identity, node: &Node, mask: u16) -> Result<(), Errno> {
    if permission(cred, node, mask) {
        Ok(())
    } else {
        Err(Errno::EACCES)
    }
}

/// may_open: whether `cred` may open `node` for `mask` (READ, WRITE, EXEC; 0 for a path-only
/// open, `O_PATH`, which asks nothing of the node itself). A symbolic link is never opened
/// (ELOOP); a directory opens for reading only (EISDIR to write, EACCES to execute); only a
/// regular file runs (EACCES).
pub fn may_open(cred: &Identity, node: &Node, mask: u16) -> Result<(), Errno> {
    if mask == 0 {
        return Ok(());
    }
    if node.is_link() {
        return Err(Errno::ELOOP);
    }
    if node.directory && mask & WRITE != 0 {
        return Err(Errno::EISDIR);
    }
    if mask & EXEC != 0 && !node.is_file() {
        return Err(Errno::EACCES);
    }
    require(cred, node, mask)
}

/// may_create: whether `cred` may add a name to `dir`. A directory removed while still in use
/// takes none (ENOENT, Linux's IS_DEADDIR); otherwise it needs write and search permission
/// (EACCES). Whether the name is free (EEXIST) is asked first, by the caller.
pub fn may_create(cred: &Identity, dir: &Node, removed: bool) -> Result<(), Errno> {
    if removed {
        return Err(Errno::ENOENT);
    }
    require(cred, dir, WRITE | EXEC)
}

/// The sticky rule: in a `+t` directory only the entry's owner, the directory's owner and root
/// may remove or rename an entry.
pub fn sticky_allows(cred: &Identity, dir: &Node, victim: &Node) -> bool {
    dir.mode & STICKY == 0 || cred.id == 0 || cred.id == dir.owner || cred.id == victim.owner
}

/// may_delete: whether `cred` may take the name of `victim` out of `dir`, before what kind it
/// is: write and search permission on the directory (EACCES), then the sticky rule (EPERM).
pub fn may_delete(cred: &Identity, dir: &Node, victim: &Node) -> Result<(), Errno> {
    require(cred, dir, WRITE | EXEC)?;
    if !sticky_allows(cred, dir, victim) {
        return Err(Errno::EPERM);
    }
    Ok(())
}

/// protected_symlinks: whether `cred` may follow `link`, found in `dir`. In a sticky directory
/// everyone may write, only links owned by the follower or by the directory's owner are
/// followed.
pub fn may_follow_link(cred: &Identity, dir: &Node, link: &Node) -> bool {
    dir.mode & STICKY == 0
        || dir.mode & 0o002 == 0
        || cred.id == 0
        || cred.id == link.owner
        || dir.owner == link.owner
}

/// A symbolic link's target, as symlink(2) takes it: not empty (ENOENT), without NUL (EINVAL),
/// and shorter than PATH_MAX with its NUL (ENAMETOOLONG).
pub fn check_link_target(target: &str) -> Result<(), Errno> {
    if target.is_empty() {
        return Err(Errno::ENOENT);
    }
    if target.contains('\0') {
        return Err(Errno::EINVAL);
    }
    if target.len() > LINK_TARGET_MAX {
        return Err(Errno::ENAMETOOLONG);
    }
    Ok(())
}

/// The group a node `who` makes in `dir` gets: the directory's if it is setgid, else `who`'s.
pub fn new_group(who: &Identity, dir: &Node) -> u32 {
    if dir.mode & SETGID != 0 {
        dir.group
    } else {
        who.group
    }
}

/// The symbolic link `who` makes in `dir`, to `target`, which [`check_link_target`] allows.
pub fn new_symlink(who: &Identity, dir: &Node, target: &str) -> Node {
    Node::symlink(who.id, new_group(who, dir), target)
}

/// chmod(2): its owner's or root's (EPERM); a symbolic link's mode never changes
/// (EOPNOTSUPP, as fchmodat with AT_SYMLINK_NOFOLLOW). Without root, setgid needs the node's
/// group to be one of the caller's: it is cleared otherwise, on any kind of node, without an
/// error, as Linux's setattr_copy does.
pub fn chmod(cred: &Identity, node: &mut Node, mode: u16) -> Result<(), Errno> {
    if node.is_link() {
        return Err(Errno::EOPNOTSUPP);
    }
    if cred.id != 0 && cred.id != node.owner {
        return Err(Errno::EPERM);
    }
    node.mode = mode & 0o7777;
    if cred.id != 0 && !cred.groups.contains(&node.group) {
        node.mode &= !SETGID;
    }
    Ok(())
}

/// chown(2). Naming neither owner nor group asks nothing and changes nothing. Otherwise it is
/// root's; its owner may name itself as the owner, and as the group one of its own or the
/// group it has, as Linux's chown_ok and chgrp_ok allow. Anything else is EPERM, even naming
/// the owner it already has. Changing who owns a regular file, or its group, clears its
/// privileges ([`kill_suid`]).
pub fn chown(
    cred: &Identity,
    node: &mut Node,
    uid: Option<u32>,
    gid: Option<u32>,
) -> Result<(), Errno> {
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    if cred.id != 0
        && (cred.id != node.owner
            || uid.is_some_and(|id| id != node.owner)
            || gid.is_some_and(|g| g != node.group && !cred.groups.contains(&g)))
    {
        return Err(Errno::EPERM);
    }
    if let Some(id) = uid {
        node.owner = id;
    }
    if let Some(id) = gid {
        node.group = id;
    }
    if node.is_file() {
        kill_suid(node);
    }
    Ok(())
}

/// Clear a node's setuid bit, and its setgid bit if its group may execute it: what changing
/// its owner does, and what [`remove_privs`] does on a write.
pub fn kill_suid(node: &mut Node) {
    node.mode &= !SETUID;
    if node.mode & 0o010 != 0 {
        node.mode &= !SETGID;
    }
}

/// file_remove_privs: a write or truncate by anyone but root takes a file's privileges away
/// ([`kill_suid`]).
pub fn remove_privs(cred: &Identity, node: &mut Node) {
    if cred.id != 0 {
        kill_suid(node);
    }
}

/// utimensat(2): whether `cred` may set these times. Both omitted, nothing is asked. A time
/// given must have its nanoseconds below 1,000,000,000 (EINVAL). Both set to now, they need the
/// owner, root, or write permission (EACCES); anything else, the owner or root (EPERM), as
/// Linux's ATTR_TIMES_SET asks, even one set to now and the other omitted.
pub fn may_set_times(cred: &Identity, node: &Node, atime: Time, mtime: Time) -> Result<(), Errno> {
    if atime == Time::Omit && mtime == Time::Omit {
        return Ok(());
    }
    let invalid = |time| matches!(time, Time::At { nsec, .. } if nsec > 999_999_999);
    if invalid(atime) || invalid(mtime) {
        return Err(Errno::EINVAL);
    }
    if cred.id == 0 || cred.id == node.owner {
        return Ok(());
    }
    if atime == Time::Now && mtime == Time::Now {
        return require(cred, node, WRITE);
    }
    Err(Errno::EPERM)
}

/// The time a [`Time`] sets, in seconds and nanoseconds: `now`, none, or the one given, which
/// [`may_set_times`] has checked.
pub fn time(time: Time, now: (u64, u32)) -> Option<(u64, u32)> {
    match time {
        Time::Now => Some(now),
        Time::Omit => None,
        Time::At { sec, nsec } => Some((sec, nsec)),
    }
}

/// link(2): another name for the node, never a directory (EPERM). Without owning it, as
/// Linux's protected_hardlinks has it, only a regular file one may read and write, and neither
/// setuid nor setgid and group-executable (EPERM).
pub fn may_link(cred: &Identity, node: &Node) -> Result<(), Errno> {
    if node.directory {
        return Err(Errno::EPERM);
    }
    if cred.id != 0 && cred.id != node.owner {
        let safe = node.is_file()
            && node.mode & SETUID == 0
            && !(node.mode & SETGID != 0 && node.mode & 0o010 != 0)
            && permission(cred, node, READ | WRITE);
        if !safe {
            return Err(Errno::EPERM);
        }
    }
    Ok(())
}
