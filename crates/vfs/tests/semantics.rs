//! The VFS against Linux semantics, through the reference driver (memfs).

use abi::contract::Durability;
use abi::{ACL_ACCESS, ACL_DEFAULT, Errno};
use os_vfs::memfs::MemFs;
use os_vfs::{
    Access, Cred, FILE_MAX, FdTable, FileRef, Filesystem, FsId, Handle, Identity, LockFamily,
    LockKind, LockRange, OpenOptions, OpenRef, ReleaseQueue, RenameFlags, SeekFrom, SetAttr, Stop,
    Time, Timespec, WriteAt, Written, XattrFlags, now_or_never,
};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Wake, Waker};

#[derive(Default)]
struct WakeCount(AtomicUsize);
impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// Helpers.

/// Run a call memfs answers at once.
fn run<F: Future>(future: F) -> F::Output {
    now_or_never(future).expect("memfs never waits")
}
fn memfs(generation: u64) -> MemFs {
    let tick = Cell::new(0u64);
    MemFs::new(
        FsId("mem-test".into()),
        generation,
        0,
        0,
        0o1777,
        Box::new(move || {
            tick.set(tick.get() + 1);
            (tick.get(), 0)
        }),
    )
}
fn fs() -> Rc<MemFs> {
    Rc::new(memfs(7))
}
fn user(id: u32) -> Cred {
    Cred::new(Identity {
        id,
        group: id,
        groups: BTreeSet::from([id]),
    })
}
fn names(path: &str) -> Vec<String> {
    path.split('/').map(String::from).collect()
}
fn read() -> OpenOptions {
    *OpenOptions::new().read(true)
}
fn write() -> OpenOptions {
    *OpenOptions::new().write(true)
}
fn read_write() -> OpenOptions {
    *OpenOptions::new().read(true).write(true)
}
fn path() -> OpenOptions {
    *OpenOptions::new().path(true)
}
fn exec() -> OpenOptions {
    *OpenOptions::new().exec(true)
}
fn create_new(options: OpenOptions) -> OpenOptions {
    let mut options = options;
    *options.create_new(true)
}
fn range(start: u64, end: u64) -> LockRange {
    LockRange::try_from(start..end).unwrap()
}
fn new_file(fs: &MemFs, cred: &Cred, dir: &Handle, name: &str, mode: u16, data: &[u8]) -> Handle {
    let (entry, open) = run(fs.create(cred, dir, name, mode, &create_new(write()))).unwrap();
    run(fs.write_at(&open, data, WriteAt::Offset(0))).unwrap();
    run(fs.release(&open)).unwrap();
    entry.handle
}
fn open(fs: &MemFs, cred: &Cred, handle: &Handle, options: OpenOptions) -> Result<OpenRef, Errno> {
    run(fs.open(cred, handle, &options))
}
/// Up to 64 bytes from `offset`.
fn read_at(fs: &MemFs, open: &OpenRef, offset: u64) -> Result<Vec<u8>, Errno> {
    let mut buf = vec![0; 64];
    let len = run(fs.read_at(open, &mut buf, offset))?;
    buf.truncate(len);
    Ok(buf)
}
fn setattr(fs: &MemFs, cred: &Cred, handle: &Handle, attr: SetAttr) -> Result<u16, Errno> {
    run(fs.setattr(cred, handle, &attr)).map(|metadata| metadata.mode)
}
fn chmod(fs: &MemFs, cred: &Cred, handle: &Handle, mode: u16) -> Result<u16, Errno> {
    setattr(
        fs,
        cred,
        handle,
        SetAttr {
            mode: Some(mode),
            ..SetAttr::default()
        },
    )
}
fn chown(
    fs: &MemFs,
    cred: &Cred,
    handle: &Handle,
    uid: Option<u32>,
    gid: Option<u32>,
) -> Result<(), Errno> {
    setattr(
        fs,
        cred,
        handle,
        SetAttr {
            uid,
            gid,
            ..SetAttr::default()
        },
    )
    .map(|_| ())
}
/// Pages of content the filesystem holds.
fn used(fs: &MemFs) -> u64 {
    let statfs = run(fs.statfs(&Cred::root())).unwrap();
    statfs.blocks - statfs.blocks_free
}
fn file_ref(
    fs: &Rc<MemFs>,
    cred: &Cred,
    handle: &Handle,
    options: OpenOptions,
    releases: &ReleaseQueue,
) -> FileRef {
    let open = open(fs, cred, handle, options).unwrap();
    FileRef::new(
        fs.clone(),
        open,
        handle.clone(),
        cred.clone(),
        &options,
        releases,
    )
}
fn read_file(file: &FileRef) -> Vec<u8> {
    let mut buf = vec![0; 64];
    let len = run(file.read(&mut buf)).unwrap();
    buf.truncate(len);
    buf
}

// Names and paths.

#[test]
fn lookup_walk_and_parent_resolve_within_the_filesystem() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let a = run(fs.mkdir(&alice, &root, "a", 0o755)).unwrap().handle;
    let b = run(fs.mkdir(&alice, &a, "b", 0o755)).unwrap().handle;
    let file = new_file(&fs, &alice, &b, "f", 0o644, b"hello");
    run(fs.symlink(&alice, &a, "link", "b")).unwrap();

    let walked = run(fs.walk(&alice, &root, &names("a/b/f"))).unwrap();
    assert_eq!(walked.stop, None);
    assert_eq!(walked.entries.last().unwrap().handle, file);
    let stop = |path| run(fs.walk(&alice, &root, &names(path))).unwrap().stop;
    assert_eq!(stop("a/nope/f"), Some(Stop::Missing(1)));
    assert_eq!(stop("a/link/f"), Some(Stop::Symlink(1)));
    assert_eq!(stop("a/b/f/x"), Some(Stop::NotDirectory(2)));
    // A link at the end is returned as it is: the client decides whether to follow it.
    let end = run(fs.walk(&alice, &root, &names("a/link"))).unwrap();
    assert_eq!((end.stop, end.entries.len()), (None, 2));
    for bad in [".", "..", "x/y", ""] {
        assert_eq!(
            run(fs.lookup(&alice, &root, bad)).unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(
            run(fs.walk(&alice, &root, &[bad.to_string()])).unwrap_err(),
            Errno::EINVAL
        );
    }
    // Every name is checked, not only those before a stop.
    assert_eq!(
        run(fs.walk(&alice, &root, &names("missing/.."))).unwrap_err(),
        Errno::EINVAL
    );
    let (parent, name) = run(fs.parent(&alice, &b)).unwrap().unwrap();
    assert_eq!((parent.handle, name.as_str()), (a.clone(), "b"));
    assert!(run(fs.parent(&alice, &root)).unwrap().is_none());
    // The parent follows a rename made elsewhere: `..` and getcwd stay right.
    run(fs.rename(&alice, &a, "b", &root, "moved", RenameFlags::empty())).unwrap();
    let (parent, name) = run(fs.parent(&alice, &b)).unwrap().unwrap();
    assert_eq!((parent.handle, name.as_str()), (root.clone(), "moved"));
    // getcwd asks no permission: a directory its owner may not search still has a parent.
    chmod(&fs, &alice, &b, 0).unwrap();
    assert_eq!(run(fs.parent(&alice, &b)).unwrap().unwrap().1, "moved");
}

#[test]
fn names_past_name_max_are_too_long() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    run(fs.mkdir(&alice, &root, &"n".repeat(255), 0o755)).unwrap();
    let long = "n".repeat(256);
    assert_eq!(
        run(fs.mkdir(&alice, &root, &long, 0o755)).unwrap_err(),
        Errno::ENAMETOOLONG
    );
    assert_eq!(
        run(fs.lookup(&alice, &root, &long)).unwrap_err(),
        Errno::ENAMETOOLONG
    );
    assert_eq!(
        run(fs.create(&alice, &root, &long, 0o644, &create_new(write()))).unwrap_err(),
        Errno::ENAMETOOLONG
    );
}

#[test]
fn forged_foreign_and_stale_handles_and_open_references_are_refused() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let file = new_file(&fs, &alice, &root, "f", 0o644, b"x");
    let mut forged = file.clone();
    forged.proof = "made-up".into();
    assert_eq!(
        run(fs.metadata(&alice, &forged)).unwrap_err(),
        Errno::EACCES
    );
    let mut foreign = file.clone();
    foreign.fs = FsId("another".into());
    assert_eq!(
        run(fs.metadata(&alice, &foreign)).unwrap_err(),
        Errno::EACCES
    );
    let mut old = file.clone();
    old.node.generation += 1;
    assert_eq!(run(fs.metadata(&alice, &old)).unwrap_err(), Errno::ESTALE);
    // An open reference from an earlier instance with the same id reads nothing of this one's.
    let earlier = open(&fs, &alice, &file, read()).unwrap();
    let later = memfs(8);
    let mine = new_file(&later, &alice, &later.root(), "g", 0o644, b"secret");
    open(&later, &alice, &mine, read()).unwrap();
    assert_eq!(read_at(&later, &earlier, 0).unwrap_err(), Errno::EBADF);
    assert_eq!(run(later.release(&earlier)).unwrap_err(), Errno::EBADF);
    run(fs.unlink(&alice, &root, "f")).unwrap();
    run(fs.release(&earlier)).unwrap();
    assert_eq!(run(fs.metadata(&alice, &file)).unwrap_err(), Errno::ESTALE);
}

// Permissions.

#[test]
fn permissions_follow_linux() {
    let fs = fs();
    let root = fs.root();
    let (alice, bob) = (user(1000), user(1001));
    let private = run(fs.mkdir(&alice, &root, "private", 0o700))
        .unwrap()
        .handle;
    new_file(&fs, &alice, &private, "secret", 0o600, b"s");
    assert_eq!(
        run(fs.lookup(&bob, &private, "secret")).unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(run(fs.read_dir(&bob, &private)).unwrap_err(), Errno::EACCES);
    assert_eq!(
        run(fs.create(&bob, &private, "x", 0o644, &create_new(write()))).unwrap_err(),
        Errno::EACCES
    );
    // The sticky root: bob cannot remove alice's file, nor rename it away.
    let shared = new_file(&fs, &alice, &root, "shared", 0o666, b"x");
    chmod(&fs, &alice, &shared, 0o666).unwrap();
    assert_eq!(
        run(fs.unlink(&bob, &root, "shared")).unwrap_err(),
        Errno::EPERM
    );
    assert_eq!(
        run(fs.rename(&bob, &root, "shared", &root, "mine", RenameFlags::empty())).unwrap_err(),
        Errno::EPERM
    );
    open(&fs, &bob, &shared, write()).unwrap();
    // mknod needs root.
    assert_eq!(
        run(fs.mknod(&alice, &root, "dev", 0o600, "null")).unwrap_err(),
        Errno::EPERM
    );
    // An existing name is EEXIST before the directory's permission is asked, as Linux's
    // filename_create has it.
    let admin = Cred::root();
    let sys = run(fs.mkdir(&admin, &root, "sys", 0o755)).unwrap().handle;
    run(fs.mkdir(&admin, &sys, "usr", 0o755)).unwrap();
    assert_eq!(
        run(fs.mkdir(&alice, &sys, "usr", 0o755)).unwrap_err(),
        Errno::EEXIST
    );
    assert_eq!(
        run(fs.mkdir(&alice, &sys, "new", 0o755)).unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        run(fs.link(&alice, &shared, &sys, "usr")).unwrap_err(),
        Errno::EEXIST
    );
}

#[test]
fn exec_access_is_checked_against_execute_permission() {
    let fs = fs();
    let root = fs.root();
    let (alice, bob) = (user(1000), user(1001));
    let tool = new_file(&fs, &alice, &root, "tool", 0o711, b"\0asm");
    // Execute-only: bob may run it, not read it.
    open(&fs, &bob, &tool, exec()).unwrap();
    assert_eq!(open(&fs, &bob, &tool, read()).unwrap_err(), Errno::EACCES);
    // Root still needs an execute bit; directories and devices do not run.
    let data = new_file(&fs, &alice, &root, "data", 0o644, b"x");
    assert_eq!(
        open(&fs, &Cred::root(), &data, exec()).unwrap_err(),
        Errno::EACCES
    );
    let dir = run(fs.mkdir(&alice, &root, "d", 0o755)).unwrap().handle;
    assert_eq!(open(&fs, &alice, &dir, exec()).unwrap_err(), Errno::EACCES);
    assert_eq!(open(&fs, &alice, &dir, write()).unwrap_err(), Errno::EISDIR);
    let device = run(fs.mknod(&Cred::root(), &root, "null", 0o777, "null"))
        .unwrap()
        .handle;
    assert_eq!(
        open(&fs, &alice, &device, exec()).unwrap_err(),
        Errno::EACCES
    );
}

#[test]
fn modes_owners_and_times_change_as_the_kernel_has_them() {
    let fs = fs();
    let root = fs.root();
    let (alice, bob, carol) = (user(1000), user(1001), user(1002));
    let admin = Cred::root();
    // mkdir keeps the sticky bit asked for; create keeps setuid and setgid.
    assert_eq!(
        run(fs.mkdir(&alice, &root, "tmp", 0o1777))
            .unwrap()
            .metadata
            .mode,
        0o1755
    );
    let (entry, _) = run(fs.create(&alice, &root, "suid", 0o6755, &create_new(write()))).unwrap();
    assert_eq!(entry.metadata.mode, 0o6755);
    // ... but not setgid and group-executable in a setgid directory of another group.
    let team = run(fs.mkdir(&admin, &root, "team", 0o2777)).unwrap().handle;
    chown(&fs, &admin, &team, None, Some(50)).unwrap();
    chmod(&fs, &admin, &team, 0o2777).unwrap();
    let (entry, _) = run(fs.create(&alice, &team, "f", 0o2775, &create_new(write()))).unwrap();
    assert_eq!((entry.metadata.mode, entry.metadata.gid), (0o755, 50));
    // chmod by an owner outside the group clears setgid, on a directory too.
    let dir = run(fs.mkdir(&alice, &root, "g", 0o755)).unwrap().handle;
    chown(&fs, &admin, &dir, None, Some(50)).unwrap();
    assert_eq!(chmod(&fs, &alice, &dir, 0o2775).unwrap(), 0o775);
    // A symbolic link's mode never changes.
    let link = run(fs.symlink(&alice, &root, "l", "g")).unwrap().handle;
    assert_eq!(
        chmod(&fs, &alice, &link, 0o600).unwrap_err(),
        Errno::EOPNOTSUPP
    );
    // Only root names an owner, even the one it has; an owner names the group it has or one
    // of its own.
    let file = new_file(&fs, &alice, &root, "f", 0o644, b"");
    assert_eq!(chmod(&fs, &bob, &file, 0o777).unwrap_err(), Errno::EPERM);
    assert_eq!(
        chown(&fs, &bob, &file, Some(1000), None).unwrap_err(),
        Errno::EPERM
    );
    assert_eq!(
        chown(&fs, &alice, &file, Some(1001), None).unwrap_err(),
        Errno::EPERM
    );
    chown(&fs, &admin, &file, None, Some(50)).unwrap();
    chown(&fs, &alice, &file, None, Some(50)).unwrap();
    assert_eq!(
        chown(&fs, &alice, &file, None, Some(60)).unwrap_err(),
        Errno::EPERM
    );
    chown(&fs, &alice, &file, None, Some(1000)).unwrap();
    // Times both set to now are anyone's who may write; anything else, the owner's or root's,
    // even one set to now and the other left alone.
    let times = |atime, mtime| SetAttr {
        atime,
        mtime,
        ..SetAttr::default()
    };
    chmod(&fs, &alice, &file, 0o646).unwrap();
    setattr(&fs, &bob, &file, times(Time::Now, Time::Now)).unwrap();
    assert_eq!(
        setattr(&fs, &bob, &file, times(Time::Now, Time::Omit)).unwrap_err(),
        Errno::EPERM
    );
    chmod(&fs, &alice, &file, 0o644).unwrap();
    assert_eq!(
        setattr(&fs, &carol, &file, times(Time::Now, Time::Now)).unwrap_err(),
        Errno::EACCES
    );
    let at = |nsec| Time::At { sec: 5, nsec };
    assert_eq!(
        setattr(&fs, &bob, &file, times(Time::Omit, at(0))).unwrap_err(),
        Errno::EPERM
    );
    // Nanoseconds past a second are refused, not clamped.
    assert_eq!(
        setattr(&fs, &alice, &file, times(Time::Omit, at(1_000_000_000))).unwrap_err(),
        Errno::EINVAL
    );
    let metadata = run(fs.setattr(&alice, &file, &times(Time::Omit, at(999_999_999)))).unwrap();
    assert_eq!(
        metadata.mtime,
        Timespec {
            sec: 5,
            nsec: 999_999_999
        }
    );
    // Leaving everything alone asks nothing and changes nothing.
    let before = run(fs.metadata(&carol, &file)).unwrap();
    assert_eq!(
        run(fs.setattr(&carol, &file, &SetAttr::default())).unwrap(),
        before
    );
    // A failed change changes nothing: chmod and chown together, one refused.
    let both = SetAttr {
        mode: Some(0o600),
        uid: Some(1001),
        ..SetAttr::default()
    };
    assert_eq!(setattr(&fs, &alice, &file, both).unwrap_err(), Errno::EPERM);
    assert_eq!(run(fs.metadata(&alice, &file)).unwrap().mode, 0o644);
    // A truncate moves mtime and ctime together.
    let truncated = run(fs.setattr(
        &alice,
        &file,
        &SetAttr {
            size: Some(1),
            ..SetAttr::default()
        },
    ))
    .unwrap();
    assert_eq!(truncated.mtime, truncated.ctime);
}

#[test]
fn protected_hard_links_follow_the_kernel() {
    let fs = fs();
    let root = fs.root();
    let (alice, bob) = (user(1000), user(1001));
    let admin = Cred::root();
    let special = |name, mode| {
        let file = new_file(&fs, &admin, &root, name, 0o644, b"x");
        assert_eq!(chmod(&fs, &admin, &file, mode).unwrap(), mode);
        file
    };
    // Someone else's setuid file, or setgid and group-executable one, even writable to all.
    for (name, mode) in [("suid", 0o4777), ("sgid", 0o2777)] {
        let file = special(name, mode);
        assert_eq!(
            run(fs.link(&bob, &file, &root, "stolen")).unwrap_err(),
            Errno::EPERM
        );
    }
    // Setgid without group execute is no privilege; a file one cannot read and write is.
    let marked = special("marked", 0o2767);
    run(fs.link(&bob, &marked, &root, "marked2")).unwrap();
    let other = new_file(&fs, &alice, &root, "other", 0o600, b"o");
    assert_eq!(
        run(fs.link(&bob, &other, &root, "taken")).unwrap_err(),
        Errno::EPERM
    );
    // Nor a device someone else owns; root may link one.
    let device = run(fs.mknod(&admin, &root, "null", 0o666, "null"))
        .unwrap()
        .handle;
    assert_eq!(
        run(fs.link(&bob, &device, &root, "null2")).unwrap_err(),
        Errno::EPERM
    );
    run(fs.link(&admin, &device, &root, "null2")).unwrap();
    // Its owner may link its own setuid file, and nobody a directory.
    let own = new_file(&fs, &alice, &root, "own", 0o755, b"x");
    chmod(&fs, &alice, &own, 0o4755).unwrap();
    run(fs.link(&alice, &own, &root, "own2")).unwrap();
    let dir = run(fs.mkdir(&alice, &root, "dir", 0o755)).unwrap().handle;
    assert_eq!(
        run(fs.link(&alice, &dir, &root, "dir2")).unwrap_err(),
        Errno::EPERM
    );
}

#[test]
fn symbolic_link_targets_follow_linux() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let symlink = |name, target: &str| run(fs.symlink(&alice, &root, name, target));
    assert_eq!(symlink("a", "").unwrap_err(), Errno::ENOENT);
    assert_eq!(symlink("b", "x\0y").unwrap_err(), Errno::EINVAL);
    assert_eq!(
        symlink("c", &"t".repeat(4096)).unwrap_err(),
        Errno::ENAMETOOLONG
    );
    let link = symlink("d", &"t".repeat(4095)).unwrap();
    assert_eq!((link.metadata.mode, link.metadata.size), (0o777, 4095));
    assert_eq!(run(fs.read_link(&alice, &link.handle)).unwrap().len(), 4095);
}

#[test]
fn xattrs_follow_the_kernel() {
    let fs = fs();
    let root = fs.root();
    let (alice, bob) = (user(1000), user(1001));
    let admin = Cred::root();
    let none = XattrFlags::empty();
    let file = new_file(&fs, &alice, &root, "f", 0o644, b"");
    run(fs.setxattr(&alice, &file, "user.tag", b"v", none)).unwrap();
    assert_eq!(run(fs.getxattr(&bob, &file, "user.tag")).unwrap(), b"v");
    assert_eq!(
        run(fs.setxattr(&bob, &file, "user.tag", b"w", none)).unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        run(fs.setxattr(&alice, &file, "trusted.x", b"t", none)).unwrap_err(),
        Errno::EPERM
    );
    assert_eq!(
        run(fs.setxattr(&alice, &file, "security.x", b"t", none)).unwrap_err(),
        Errno::EOPNOTSUPP
    );
    assert_eq!(
        run(fs.getxattr(&alice, &file, "user.none")).unwrap_err(),
        Errno::ENODATA
    );
    // trusted.*: root's, and others see none.
    run(fs.setxattr(&admin, &file, "trusted.t", b"t", none)).unwrap();
    assert_eq!(
        run(fs.getxattr(&alice, &file, "trusted.t")).unwrap_err(),
        Errno::ENODATA
    );
    assert_eq!(run(fs.listxattr(&alice, &file)).unwrap(), ["user.tag"]);
    assert_eq!(
        run(fs.listxattr(&admin, &file)).unwrap(),
        ["trusted.t", "user.tag"]
    );
    // XATTR_CREATE and XATTR_REPLACE.
    assert_eq!(
        run(fs.setxattr(&alice, &file, "user.tag", b"w", XattrFlags::CREATE)).unwrap_err(),
        Errno::EEXIST
    );
    assert_eq!(
        run(fs.setxattr(&alice, &file, "user.new", b"w", XattrFlags::REPLACE)).unwrap_err(),
        Errno::ENODATA
    );
    run(fs.setxattr(&alice, &file, "user.tag", b"w", XattrFlags::REPLACE)).unwrap();
    // The limits: a name's, a value's, and one node's in all.
    let long = format!("user.{}", "x".repeat(251));
    assert_eq!(
        run(fs.setxattr(&alice, &file, &long, b"v", none)).unwrap_err(),
        Errno::ERANGE
    );
    assert_eq!(
        run(fs.setxattr(&alice, &file, "user.big", &[0; 4097], none)).unwrap_err(),
        Errno::E2BIG
    );
    run(fs.setxattr(&alice, &file, "user.big", &[0; 4000], none)).unwrap();
    assert_eq!(
        run(fs.setxattr(&alice, &file, "user.more", &[0; 100], none)).unwrap_err(),
        Errno::ENOSPC
    );
    // user.* only on regular files and directories, and a sticky directory's are its owner's.
    let link = run(fs.symlink(&alice, &root, "l", "f")).unwrap().handle;
    assert_eq!(
        run(fs.setxattr(&alice, &link, "user.x", b"v", none)).unwrap_err(),
        Errno::EPERM
    );
    let sticky = run(fs.mkdir(&alice, &root, "t", 0o1777)).unwrap().handle;
    chmod(&fs, &alice, &sticky, 0o1777).unwrap();
    assert_eq!(
        run(fs.setxattr(&bob, &sticky, "user.x", b"v", none)).unwrap_err(),
        Errno::EPERM
    );
    // ACLs, as getfacl and setfacl read and write them: the entries, as JSON.
    let entries = br#"{"user":6,"group":4,"other":4,"mask":6,"users":{"1001":6}}"#;
    assert_eq!(open(&fs, &bob, &file, write()).unwrap_err(), Errno::EACCES);
    assert_eq!(
        run(fs.setxattr(&bob, &file, ACL_ACCESS, entries, none)).unwrap_err(),
        Errno::EPERM
    );
    run(fs.setxattr(&alice, &file, ACL_ACCESS, entries, none)).unwrap();
    open(&fs, &bob, &file, write()).unwrap();
    assert!(run(fs.metadata(&alice, &file)).unwrap().acl);
    let acl: serde_json::Value =
        serde_json::from_slice(&run(fs.getxattr(&bob, &file, ACL_ACCESS)).unwrap()).unwrap();
    assert_eq!(acl["users"]["1001"], 6);
    assert!(
        run(fs.listxattr(&alice, &file))
            .unwrap()
            .contains(&ACL_ACCESS.to_string())
    );
    // Setting an access ACL clears setgid when its setter is not in the group, as chmod does.
    let sgid = new_file(&fs, &alice, &root, "sgid", 0o755, b"");
    chown(&fs, &admin, &sgid, None, Some(50)).unwrap();
    chmod(&fs, &admin, &sgid, 0o2755).unwrap();
    run(fs.setxattr(&alice, &sgid, ACL_ACCESS, entries, none)).unwrap();
    assert_eq!(run(fs.metadata(&alice, &sgid)).unwrap().mode & 0o2000, 0);
    // A default ACL is a directory's, and what is made in it takes it.
    let default = br#"{"user":7,"group":5,"other":0,"mask":7,"users":{"1001":7}}"#;
    assert_eq!(
        run(fs.setxattr(&alice, &file, ACL_DEFAULT, default, none)).unwrap_err(),
        Errno::EACCES
    );
    let shared = run(fs.mkdir(&alice, &root, "shared", 0o755))
        .unwrap()
        .handle;
    run(fs.setxattr(&alice, &shared, ACL_DEFAULT, default, none)).unwrap();
    let made = new_file(&fs, &alice, &shared, "made", 0o666, b"");
    open(&fs, &bob, &made, write()).unwrap();
    // Removing an ACL leaves the mode.
    run(fs.removexattr(&alice, &file, ACL_ACCESS)).unwrap();
    assert_eq!(
        run(fs.removexattr(&alice, &file, ACL_ACCESS)).unwrap_err(),
        Errno::ENODATA
    );
    assert!(!run(fs.metadata(&alice, &file)).unwrap().acl);
}

// Opening and creating.

#[test]
fn the_creator_keeps_the_access_it_asked_for() {
    // tar: open(O_CREAT|O_WRONLY|O_EXCL, 0444), then write and ftruncate through that descriptor.
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let (entry, open_ref) =
        run(fs.create(&alice, &root, "ro", 0o444, &create_new(write()))).unwrap();
    assert_eq!(entry.metadata.mode, 0o444);
    assert_eq!(
        run(fs.write_at(&open_ref, b"data", WriteAt::Offset(0)))
            .unwrap()
            .len,
        4
    );
    run(fs.set_len(&open_ref, 2)).unwrap();
    assert_eq!(run(fs.metadata(&alice, &entry.handle)).unwrap().size, 2);
    // A second open for writing is refused: the mode applies to everyone else.
    assert_eq!(
        open(&fs, &alice, &entry.handle, write()).unwrap_err(),
        Errno::EACCES
    );
    // O_EXCL is atomic: the name is taken.
    assert_eq!(
        run(fs.create(&alice, &root, "ro", 0o644, &create_new(write()))).unwrap_err(),
        Errno::EEXIST
    );
    // A read-only open reference cannot write or truncate.
    let reader = run(fs.create(&alice, &root, "r", 0o644, &create_new(read())))
        .unwrap()
        .1;
    assert_eq!(
        run(fs.write_at(&reader, b"x", WriteAt::Offset(0))).unwrap_err(),
        Errno::EBADF
    );
    assert_eq!(run(fs.set_len(&reader, 0)).unwrap_err(), Errno::EINVAL);
    // Exec and path-only accesses create nothing.
    assert_eq!(
        run(fs.create(&alice, &root, "x", 0o755, &create_new(exec()))).unwrap_err(),
        Errno::EINVAL
    );
}

#[test]
fn create_and_open_truncate_in_the_same_call() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let file = new_file(&fs, &alice, &root, "f", 0o644, b"old content");
    // echo hi > f: one create call opens and empties it.
    let mut options = write();
    options.create(true).truncate(true);
    let (entry, open_ref) = run(fs.create(&alice, &root, "f", 0o644, &options)).unwrap();
    assert_eq!(
        (entry.handle.clone(), entry.metadata.size),
        (file.clone(), 0)
    );
    run(fs.write_at(&open_ref, b"hi", WriteAt::Offset(0))).unwrap();
    // O_RDONLY | O_TRUNC truncates too, given write permission, as on Linux.
    let mut truncating = read();
    truncating.truncate(true);
    open(&fs, &alice, &file, truncating).unwrap();
    assert_eq!(run(fs.metadata(&alice, &file)).unwrap().size, 0);
    assert_eq!(
        open(&fs, &user(1001), &file, truncating).unwrap_err(),
        Errno::EACCES
    );
    // A directory: EISDIR, for create on its name whatever the access, and for truncation.
    let dir = run(fs.mkdir(&alice, &root, "d", 0o755)).unwrap().handle;
    for options in [read(), write()] {
        let mut options = options;
        options.create(true);
        assert_eq!(
            run(fs.create(&alice, &root, "d", 0o644, &options)).unwrap_err(),
            Errno::EISDIR
        );
    }
    assert_eq!(
        open(&fs, &alice, &dir, truncating).unwrap_err(),
        Errno::EISDIR
    );
}

#[test]
fn open_flags_map_to_options() {
    let readonly_append = OpenOptions::from_flags(abi::O_RDONLY | abi::O_APPEND).unwrap();
    assert_eq!(readonly_append.access(), Ok(Access::Read));
    assert!(readonly_append.is_append());
    assert_eq!(OpenOptions::new().append(true).access(), Ok(Access::Write));
    assert_eq!(
        OpenOptions::new().read(true).append(true).access(),
        Ok(Access::ReadWrite)
    );
    let options =
        OpenOptions::from_flags(abi::O_RDWR | abi::O_APPEND | abi::O_CREAT | abi::O_EXCL).unwrap();
    assert_eq!(options.access().unwrap(), Access::ReadWrite);
    assert!(options.is_append() && options.is_create() && options.is_create_new());
    assert_eq!(
        OpenOptions::from_flags(abi::O_PATH)
            .unwrap()
            .access()
            .unwrap(),
        Access::Path
    );
    assert_eq!(OpenOptions::new().access().unwrap_err(), Errno::EINVAL);
    assert_eq!(
        OpenOptions::new()
            .exec(true)
            .read(true)
            .access()
            .unwrap_err(),
        Errno::EINVAL
    );
}

#[test]
fn path_only_opens_keep_a_directory_without_access() {
    let fs = fs();
    let root = fs.root();
    let (alice, bob) = (user(1000), user(1001));
    let home = run(fs.mkdir(&alice, &root, "home", 0o755)).unwrap().handle;
    let public = run(fs.mkdir(&alice, &home, "pub", 0o711)).unwrap().handle;
    // bob may search pub but not read it: a path-only open is all his working directory needs.
    assert_eq!(open(&fs, &bob, &public, read()).unwrap_err(), Errno::EACCES);
    let cwd = open(&fs, &bob, &public, path()).unwrap();
    // It grants nothing.
    assert_eq!(read_at(&fs, &cwd, 0).unwrap_err(), Errno::EBADF);
    assert_eq!(run(fs.sync_all(&cwd)).unwrap_err(), Errno::EBADF);
    assert_eq!(run(fs.set_len(&cwd, 0)).unwrap_err(), Errno::EBADF);
    assert_eq!(
        run(fs.try_lock(&cwd, LockFamily::Flock, LockKind::Shared, LockRange::WHOLE)).unwrap_err(),
        Errno::EBADF
    );
    // alice removes it: it lives on for bob, removed, as a cwd does on Linux.
    run(fs.rmdir(&alice, &home, "pub")).unwrap();
    assert_eq!(run(fs.metadata(&bob, &public)).unwrap().nlink, 0);
    assert_eq!(run(fs.parent(&bob, &public)).unwrap_err(), Errno::ENOENT);
    assert_eq!(
        run(fs.lookup(&bob, &public, "x")).unwrap_err(),
        Errno::ENOENT
    );
    run(fs.release(&cwd)).unwrap();
    assert_eq!(run(fs.metadata(&bob, &public)).unwrap_err(), Errno::ESTALE);
}

// Content.

#[test]
fn open_files_survive_unlink_and_rename_until_released() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let file = new_file(&fs, &alice, &root, "f", 0o644, b"content");
    let open_ref = open(&fs, &alice, &file, read()).unwrap();
    run(fs.rename(&alice, &root, "f", &root, "g", RenameFlags::empty())).unwrap();
    assert_eq!(read_at(&fs, &open_ref, 0).unwrap(), b"content");
    let objects = fs.inode_count();
    run(fs.unlink(&alice, &root, "g")).unwrap();
    // Unlinked but open: still readable, nlink 0, and its content is kept.
    assert_eq!(read_at(&fs, &open_ref, 0).unwrap(), b"content");
    assert_eq!(run(fs.metadata(&alice, &file)).unwrap().nlink, 0);
    assert_eq!(fs.inode_count(), objects);
    run(fs.release(&open_ref)).unwrap();
    // Released: the content is gone and the handle is stale.
    assert_eq!(fs.inode_count(), objects - 1);
    assert_eq!(run(fs.metadata(&alice, &file)).unwrap_err(), Errno::ESTALE);
    assert_eq!(read_at(&fs, &open_ref, 0).unwrap_err(), Errno::EBADF);
}

#[test]
fn offsets_are_64_bit_files_are_sparse_and_content_is_bounded() {
    let fs = memfs(7).with_capacity(64 * 1024);
    let root = fs.root();
    let alice = user(1000);
    let (entry, open_ref) =
        run(fs.create(&alice, &root, "f", 0o644, &create_new(read_write()))).unwrap();
    run(fs.write_at(&open_ref, b"hello", WriteAt::Offset(0))).unwrap();
    // Past 4 GiB: a hole between, not the start of the file, and only two pages held.
    let far = (1u64 << 32) + 10;
    assert_eq!(
        run(fs.write_at(&open_ref, b"tail", WriteAt::Offset(far))).unwrap(),
        Written {
            offset: far,
            len: 4
        }
    );
    assert_eq!(
        run(fs.metadata(&alice, &entry.handle)).unwrap().size,
        far + 4
    );
    assert_eq!(read_at(&fs, &open_ref, 1 << 32).unwrap()[..5], [0; 5]);
    assert_eq!(read_at(&fs, &open_ref, far).unwrap(), b"tail");
    assert_eq!(read_at(&fs, &open_ref, 0).unwrap()[..5], *b"hello");
    assert_eq!(used(&fs), 2);
    // Truncating up allocates nothing; truncating down frees what it cuts, and what lay past
    // the end reads as zeros once the file grows again.
    run(fs.set_len(&open_ref, 1 << 40)).unwrap();
    assert_eq!(used(&fs), 2);
    run(fs.set_len(&open_ref, 2)).unwrap();
    assert_eq!(used(&fs), 1);
    run(fs.set_len(&open_ref, 6)).unwrap();
    assert_eq!(read_at(&fs, &open_ref, 0).unwrap(), b"he\0\0\0\0");
    // At or past the largest offset: EFBIG. A write up to it is cut to fit.
    assert_eq!(
        run(fs.write_at(&open_ref, b"x", WriteAt::Offset(FILE_MAX))).unwrap_err(),
        Errno::EFBIG
    );
    assert_eq!(
        run(fs.set_len(&open_ref, FILE_MAX + 1)).unwrap_err(),
        Errno::EFBIG
    );
    assert_eq!(
        run(fs.write_at(&open_ref, b"abcd", WriteAt::Offset(FILE_MAX - 2))).unwrap(),
        Written {
            offset: FILE_MAX - 2,
            len: 2
        }
    );
    run(fs.set_len(&open_ref, 6)).unwrap();
    // Past the capacity: a short write of the pages that fit, then ENOSPC.
    let big = vec![1u8; 64 * 1024];
    assert_eq!(
        run(fs.write_at(&open_ref, &big, WriteAt::Offset(0))).unwrap(),
        Written {
            offset: 0,
            len: 64 * 1024
        }
    );
    assert_eq!(
        run(fs.write_at(&open_ref, &big, WriteAt::Offset(1 << 20))).unwrap_err(),
        Errno::ENOSPC
    );
    run(fs.set_len(&open_ref, 8192)).unwrap();
    assert_eq!(
        run(fs.write_at(&open_ref, &big, WriteAt::Offset(8192))).unwrap(),
        Written {
            offset: 8192,
            len: 56 * 1024
        }
    );
    // A path truncate past the largest size fails too, and the content goes with the file.
    let too_big = SetAttr {
        size: Some(FILE_MAX + 1),
        ..SetAttr::default()
    };
    assert_eq!(
        run(fs.setattr(&alice, &entry.handle, &too_big)).unwrap_err(),
        Errno::EFBIG
    );
    run(fs.release(&open_ref)).unwrap();
    run(fs.unlink(&alice, &root, "f")).unwrap();
    assert_eq!(used(&fs), 0);
}

#[test]
fn writes_of_nothing_change_nothing() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let file = new_file(&fs, &alice, &root, "z", 0o644, b"abc");
    let open_ref = open(&fs, &alice, &file, write()).unwrap();
    let before = run(fs.metadata(&alice, &file)).unwrap();
    assert_eq!(
        run(fs.write_at(&open_ref, b"", WriteAt::Offset(1000))).unwrap(),
        Written {
            offset: 1000,
            len: 0
        }
    );
    assert_eq!(run(fs.metadata(&alice, &file)).unwrap(), before);
}

#[test]
fn device_io_is_its_drivers() {
    let fs = fs();
    let root = fs.root();
    let device = run(fs.mknod(&Cred::root(), &root, "null", 0o666, "null"))
        .unwrap()
        .handle;
    let open_ref = open(&fs, &Cred::root(), &device, read_write()).unwrap();
    assert_eq!(
        run(fs.write_at(&open_ref, b"junk", WriteAt::Offset(0))).unwrap_err(),
        Errno::ENXIO
    );
    assert_eq!(read_at(&fs, &open_ref, 0).unwrap_err(), Errno::ENXIO);
    assert_eq!(run(fs.set_len(&open_ref, 100)).unwrap_err(), Errno::EINVAL);
    assert_eq!(used(&fs), 0);
}

#[test]
fn rename_and_remove_follow_linux() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let none = RenameFlags::empty();
    let d = run(fs.mkdir(&alice, &root, "d", 0o755)).unwrap().handle;
    let sub = run(fs.mkdir(&alice, &d, "sub", 0o755)).unwrap().handle;
    new_file(&fs, &alice, &sub, "x", 0o644, b"");
    new_file(&fs, &alice, &root, "a", 0o644, b"a");
    let b = new_file(&fs, &alice, &root, "b", 0o644, b"b");
    assert_eq!(
        run(fs.rename(&alice, &root, "a", &root, "b", RenameFlags::NOREPLACE)).unwrap_err(),
        Errno::EEXIST
    );
    assert_eq!(
        run(fs.rename(&alice, &root, "d", &sub, "loop", none)).unwrap_err(),
        Errno::EINVAL
    );
    assert_eq!(
        run(fs.rename(&alice, &root, "a", &root, "d", none)).unwrap_err(),
        Errno::EISDIR
    );
    assert_eq!(
        run(fs.rmdir(&alice, &root, "d")).unwrap_err(),
        Errno::ENOTEMPTY
    );
    assert_eq!(
        run(fs.unlink(&alice, &root, "d")).unwrap_err(),
        Errno::EISDIR
    );
    assert_eq!(
        run(fs.rmdir(&alice, &root, "a")).unwrap_err(),
        Errno::ENOTDIR
    );
    // Replacing rename: b now has a's content; the old b, kept by another name, records the
    // lost name in its ctime.
    run(fs.link(&alice, &b, &root, "b2")).unwrap();
    let before = run(fs.metadata(&alice, &b)).unwrap();
    run(fs.rename(&alice, &root, "a", &root, "b", none)).unwrap();
    let after = run(fs.metadata(&alice, &b)).unwrap();
    assert_eq!(after.nlink, before.nlink - 1);
    assert!(after.ctime > before.ctime);
    let b_now = run(fs.lookup(&alice, &root, "b")).unwrap();
    let open_ref = open(&fs, &alice, &b_now.handle, read()).unwrap();
    assert_eq!(read_at(&fs, &open_ref, 0).unwrap(), b"a");
    // Directory link counts follow moves.
    let other = run(fs.mkdir(&alice, &root, "other", 0o755)).unwrap().handle;
    run(fs.rename(&alice, &d, "sub", &other, "sub", none)).unwrap();
    assert_eq!(run(fs.metadata(&alice, &d)).unwrap().nlink, 2);
    assert_eq!(run(fs.metadata(&alice, &other)).unwrap().nlink, 3);
}

#[test]
fn hard_links_share_one_object() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let file = new_file(&fs, &alice, &root, "one", 0o644, b"shared");
    let second = run(fs.link(&alice, &file, &root, "two")).unwrap();
    assert_eq!(
        (second.handle.clone(), second.metadata.nlink),
        (file.clone(), 2)
    );
    run(fs.unlink(&alice, &root, "one")).unwrap();
    let left = run(fs.lookup(&alice, &root, "two")).unwrap();
    assert_eq!((left.handle, left.metadata.nlink), (file, 1));
}

#[test]
fn a_removed_directory_takes_no_names_and_lists_none() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let none = RenameFlags::empty();
    let a = run(fs.mkdir(&alice, &root, "a", 0o755)).unwrap().handle;
    let b = run(fs.mkdir(&alice, &a, "b", 0o755)).unwrap().handle;
    let file = new_file(&fs, &alice, &root, "f", 0o644, b"f");
    let held = open(&fs, &alice, &b, read()).unwrap();
    let objects = fs.inode_count();
    run(fs.rmdir(&alice, &a, "b")).unwrap();
    run(fs.rmdir(&alice, &root, "a")).unwrap();
    // b is open and its parent gone: `..` and getcwd say ENOENT, without a panic.
    assert_eq!(run(fs.parent(&alice, &b)).unwrap_err(), Errno::ENOENT);
    assert_eq!(run(fs.read_dir(&alice, &b)).unwrap_err(), Errno::ENOENT);
    // No name goes into it, as Linux's IS_DEADDIR has it.
    assert_eq!(
        run(fs.mkdir(&alice, &b, "x", 0o755)).unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(
        run(fs.create(&alice, &b, "x", 0o644, &create_new(write()))).unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(
        run(fs.symlink(&alice, &b, "x", "t")).unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(
        run(fs.mknod(&Cred::root(), &b, "x", 0o600, "null")).unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(
        run(fs.link(&alice, &file, &b, "x")).unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(
        run(fs.rename(&alice, &root, "f", &b, "x", none)).unwrap_err(),
        Errno::ENOENT
    );
    run(fs.mkdir(&alice, &root, "d", 0o755)).unwrap();
    assert_eq!(
        run(fs.rename(&alice, &root, "d", &b, "d", none)).unwrap_err(),
        Errno::ENOENT
    );
    // Released, it goes: a and b are gone, d was made.
    run(fs.release(&held)).unwrap();
    assert_eq!(fs.inode_count(), objects - 2 + 1);
    // An unlinked file, though still open, takes no name again.
    let open_ref = open(&fs, &alice, &file, read()).unwrap();
    run(fs.unlink(&alice, &root, "f")).unwrap();
    assert_eq!(
        run(fs.link(&alice, &file, &root, "again")).unwrap_err(),
        Errno::ENOENT
    );
    run(fs.release(&open_ref)).unwrap();
}

// Locks.

/// Poll a future once with a waker that only counts its wakes.
fn poll_once<F: Future + ?Sized>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn fcntl_locks_conflict_split_and_merge() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let posix = LockFamily::Posix;
    let file = new_file(&fs, &alice, &root, "db", 0o666, b"");
    let one = open(&fs, &alice, &file, read_write()).unwrap();
    let two = open(&fs, &alice, &file, read_write()).unwrap();
    run(fs.try_lock(&one, posix, LockKind::Exclusive, range(0, 100))).unwrap();
    // Unlocking the middle keeps both ends.
    run(fs.unlock(&one, posix, range(50, 60))).unwrap();
    for held in [range(0, 10), range(90, 100)] {
        assert_eq!(
            run(fs.try_lock(&two, posix, LockKind::Shared, held)).unwrap_err(),
            Errno::EAGAIN
        );
    }
    run(fs.try_lock(&two, posix, LockKind::Shared, range(50, 60))).unwrap();
    run(fs.unlock(&two, posix, range(50, 60))).unwrap();
    // Downgrading part keeps the rest exclusive.
    run(fs.try_lock(&one, posix, LockKind::Shared, range(0, 10))).unwrap();
    run(fs.try_lock(&two, posix, LockKind::Shared, range(0, 10))).unwrap();
    assert_eq!(
        run(fs.try_lock(&two, posix, LockKind::Shared, range(10, 20))).unwrap_err(),
        Errno::EAGAIN
    );
    // Locks of one kind that meet merge into one, as F_GETLK shows.
    run(fs.try_lock(&one, posix, LockKind::Exclusive, range(50, 60))).unwrap();
    let conflict = run(fs.test_lock(&two, LockKind::Shared, range(20, 21)))
        .unwrap()
        .unwrap();
    assert_eq!((conflict.range, conflict.owner), (range(10, 100), one.id));
    // A lock to the end of the file splits too.
    run(fs.unlock(&one, posix, LockRange::WHOLE)).unwrap();
    run(fs.try_lock(&one, posix, LockKind::Exclusive, LockRange::from(100..))).unwrap();
    run(fs.unlock(&one, posix, range(200, 210))).unwrap();
    let conflict_at = |at| {
        run(fs.test_lock(&two, LockKind::Exclusive, range(at, at + 1)))
            .unwrap()
            .map(|conflict| conflict.range)
    };
    assert_eq!(conflict_at(150), Some(range(100, 200)));
    assert_eq!(conflict_at(205), None);
    assert_eq!(conflict_at(300), Some(LockRange::from(210..)));
    // Locks go with their open reference.
    run(fs.release(&one)).unwrap();
    assert_eq!(conflict_at(150), None);
}

#[test]
fn flock_and_fcntl_locks_are_separate_and_ask_for_access() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let file = new_file(&fs, &alice, &root, "pid", 0o666, b"");
    let reader = open(&fs, &alice, &file, read()).unwrap();
    let writer = open(&fs, &alice, &file, write()).unwrap();
    // fcntl: shared needs read access, exclusive write access.
    assert_eq!(
        run(fs.try_lock(
            &reader,
            LockFamily::Posix,
            LockKind::Exclusive,
            range(0, 10)
        ))
        .unwrap_err(),
        Errno::EBADF
    );
    assert_eq!(
        run(fs.try_lock(&writer, LockFamily::Posix, LockKind::Shared, range(0, 10))).unwrap_err(),
        Errno::EBADF
    );
    // flock: any access, the whole file only.
    run(fs.try_lock(
        &reader,
        LockFamily::Flock,
        LockKind::Exclusive,
        LockRange::WHOLE,
    ))
    .unwrap();
    assert_eq!(
        run(fs.try_lock(&writer, LockFamily::Flock, LockKind::Shared, range(0, 10))).unwrap_err(),
        Errno::EINVAL
    );
    // An flock lock and an fcntl lock never conflict.
    run(fs.try_lock(
        &writer,
        LockFamily::Posix,
        LockKind::Exclusive,
        LockRange::WHOLE,
    ))
    .unwrap();
    assert_eq!(
        run(fs.try_lock(
            &writer,
            LockFamily::Flock,
            LockKind::Shared,
            LockRange::WHOLE
        ))
        .unwrap_err(),
        Errno::EAGAIN
    );
    // Unlocking one family leaves the other.
    run(fs.unlock(&writer, LockFamily::Flock, LockRange::WHOLE)).unwrap();
    assert!(
        run(fs.test_lock(&reader, LockKind::Shared, range(0, 1)))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_blocking_lock_waits_until_granted_and_refuses_a_deadlock() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let posix = LockFamily::Posix;
    let file = new_file(&fs, &alice, &root, "db", 0o666, b"");
    let one = open(&fs, &alice, &file, read_write()).unwrap();
    let two = open(&fs, &alice, &file, read_write()).unwrap();
    run(fs.lock(&one, posix, LockKind::Exclusive, range(0, 10))).unwrap();
    run(fs.lock(&two, posix, LockKind::Exclusive, range(10, 20))).unwrap();
    // two waits for one's range.
    {
        let mut wait = pin!(fs.lock(&two, posix, LockKind::Exclusive, range(0, 10)));
        assert!(poll_once(wait.as_mut()).is_pending());
        // one waiting for two's would close the cycle.
        assert_eq!(
            run(fs.lock(&one, posix, LockKind::Exclusive, range(10, 20))).unwrap_err(),
            Errno::EDEADLK
        );
        // Once one lets go, the wait is granted.
        run(fs.unlock(&one, posix, range(0, 10))).unwrap();
        assert!(matches!(poll_once(wait.as_mut()), Poll::Ready(Ok(()))));
    }
    // A wait given up leaves no trace.
    {
        let mut wait = pin!(fs.lock(&one, posix, LockKind::Exclusive, range(0, 10)));
        assert!(poll_once(wait.as_mut()).is_pending());
    }
    run(fs.release(&two)).unwrap();
    run(fs.lock(&one, posix, LockKind::Exclusive, range(0, 20))).unwrap();
}

#[test]
fn lock_ranges_are_never_empty() {
    assert_eq!(LockRange::try_from(10..10).unwrap_err(), Errno::EINVAL);
    assert!(LockRange::new(10, Some(5)).is_none());
    assert_eq!(LockRange::from(5..).end(), None);
    assert_eq!(
        range(0, 100).minus(range(40, 60)),
        [Some(range(0, 40)), Some(range(60, 100))]
    );
    assert_eq!(
        LockRange::WHOLE.minus(range(0, 10)),
        [None, Some(LockRange::from(10..))]
    );
}

#[test]
fn lock_waits_register_once_and_cancel_independently() {
    let fs = fs();
    let alice = user(1000);
    let file = new_file(&fs, &alice, &fs.root(), "db", 0o600, b"");
    let a = open(&fs, &alice, &file, read_write()).unwrap();
    let b = open(&fs, &alice, &file, read_write()).unwrap();
    let family = LockFamily::Posix;
    let kind = LockKind::Exclusive;
    run(fs.try_lock(&a, family, kind, range(0, 10))).unwrap();
    run(fs.try_lock(&b, family, kind, range(10, 20))).unwrap();
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut context = Context::from_waker(&waker);
    let mut first = fs.lock(&b, family, kind, range(0, 5));
    let mut second = fs.lock(&b, family, kind, range(5, 10));
    for _ in 0..3 {
        assert!(first.as_mut().poll(&mut context).is_pending());
    }
    assert!(second.as_mut().poll(&mut context).is_pending());
    drop(second);
    // Cancelling one call must not erase the other call's dependency on a.
    assert_eq!(
        run(fs.lock(&a, family, kind, range(10, 20))),
        Err(Errno::EDEADLK)
    );
    run(fs.unlock(&a, family, range(0, 10))).unwrap();
    assert_eq!(wakes.0.swap(0, Ordering::SeqCst), 1);
    assert_eq!(first.as_mut().poll(&mut context), Poll::Ready(Ok(())));
    // A cancelled waiter must not keep its task alive or receive later wakeups.
    let mut cancelled = fs.lock(&a, family, kind, range(0, 5));
    assert!(cancelled.as_mut().poll(&mut context).is_pending());
    drop(cancelled);
    run(fs.release(&b)).unwrap();
    assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
}

#[test]
fn deadlock_detection_follows_all_conflicts_and_current_locks() {
    let fs = fs();
    let alice = user(1000);
    let file = new_file(&fs, &alice, &fs.root(), "db", 0o600, b"");
    let a = open(&fs, &alice, &file, read_write()).unwrap();
    let b = open(&fs, &alice, &file, read_write()).unwrap();
    let c = open(&fs, &alice, &file, read_write()).unwrap();
    let family = LockFamily::Posix;
    // c's first conflicting holder (b) does not wait, but its second (a) closes a cycle.
    run(fs.try_lock(&b, family, LockKind::Shared, range(0, 10))).unwrap();
    run(fs.try_lock(&a, family, LockKind::Shared, range(0, 10))).unwrap();
    run(fs.try_lock(&c, family, LockKind::Exclusive, range(10, 20))).unwrap();
    let mut wait_a = fs.lock(&a, family, LockKind::Exclusive, range(10, 20));
    assert!(poll_once(wait_a.as_mut()).is_pending());
    assert_eq!(
        run(fs.lock(&c, family, LockKind::Exclusive, range(0, 10))),
        Err(Errno::EDEADLK)
    );
    // The released lock stops blocking a immediately, even before a's future polls again.
    run(fs.unlock(&c, family, range(10, 20))).unwrap();
    let mut wait_c = fs.lock(&c, family, LockKind::Exclusive, range(0, 10));
    assert!(poll_once(wait_c.as_mut()).is_pending());
    assert_eq!(poll_once(wait_a.as_mut()), Poll::Ready(Ok(())));
    run(fs.unlock(&a, family, range(0, 10))).unwrap();
    run(fs.unlock(&b, family, range(0, 10))).unwrap();
    assert_eq!(poll_once(wait_c.as_mut()), Poll::Ready(Ok(())));
}

#[test]
fn lock_waiters_wake_on_downgrade_and_release_in_both_families() {
    for family in [LockFamily::Posix, LockFamily::Flock] {
        let fs = fs();
        let alice = user(1000);
        let file = new_file(&fs, &alice, &fs.root(), "db", 0o600, b"");
        let a = open(&fs, &alice, &file, read_write()).unwrap();
        let b = open(&fs, &alice, &file, read_write()).unwrap();
        let wakes = Arc::new(WakeCount::default());
        let waker = Waker::from(wakes.clone());
        let mut context = Context::from_waker(&waker);
        run(fs.try_lock(&a, family, LockKind::Exclusive, LockRange::WHOLE)).unwrap();
        let mut wait = fs.lock(&b, family, LockKind::Shared, LockRange::WHOLE);
        assert!(wait.as_mut().poll(&mut context).is_pending());
        run(fs.try_lock(&a, family, LockKind::Shared, LockRange::WHOLE)).unwrap();
        assert!(wakes.0.swap(0, Ordering::SeqCst) >= 1);
        assert_eq!(wait.as_mut().poll(&mut context), Poll::Ready(Ok(())));
        let mut wait = fs.lock(&b, family, LockKind::Exclusive, LockRange::WHOLE);
        assert!(wait.as_mut().poll(&mut context).is_pending());
        run(fs.release(&a)).unwrap();
        assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
        assert_eq!(wait.as_mut().poll(&mut context), Poll::Ready(Ok(())));
    }
}

#[test]
fn flock_conversion_drops_the_old_lock_and_test_lock_needs_no_write_access() {
    let fs = fs();
    let alice = user(1000);
    let file = new_file(&fs, &alice, &fs.root(), "db", 0o600, b"");
    let a = open(&fs, &alice, &file, read()).unwrap();
    let b = open(&fs, &alice, &file, read()).unwrap();
    let family = LockFamily::Flock;
    run(fs.try_lock(&a, family, LockKind::Shared, LockRange::WHOLE)).unwrap();
    run(fs.try_lock(&b, family, LockKind::Shared, LockRange::WHOLE)).unwrap();
    assert_eq!(
        run(fs.try_lock(&a, family, LockKind::Exclusive, LockRange::WHOLE)),
        Err(Errno::EAGAIN)
    );
    // a no longer owns a shared lock after its failed conversion.
    run(fs.try_lock(&b, family, LockKind::Exclusive, LockRange::WHOLE)).unwrap();
    assert_eq!(
        run(fs.test_lock(&a, LockKind::Exclusive, LockRange::WHOLE)),
        Ok(None)
    );
    let writer = open(&fs, &alice, &file, write()).unwrap();
    run(fs.try_lock(
        &writer,
        LockFamily::Posix,
        LockKind::Exclusive,
        LockRange::WHOLE,
    ))
    .unwrap();
    assert_eq!(
        run(fs.test_lock(&a, LockKind::Exclusive, LockRange::WHOLE))
            .unwrap()
            .unwrap()
            .owner,
        writer.id
    );
}

// Descriptions and descriptors.

#[test]
fn descriptors_share_a_description_across_dup_and_fork() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let releases = ReleaseQueue::new();
    let file = new_file(&fs, &alice, &root, "f", 0o644, b"0123456789");
    let mut table = FdTable::new();
    let fd = table
        .insert(file_ref(&fs, &alice, &file, read_write(), &releases), false)
        .unwrap();
    assert_eq!(fd, 0);
    let dup = table.dup(fd, 0, false).unwrap();
    assert_eq!(dup, 1);
    let mut buf = [0; 3];
    assert_eq!(run(table.get(fd).unwrap().read(&mut buf)).unwrap(), 3);
    assert_eq!(&buf, b"012");
    // The position is the description's: dup and a forked child see it move.
    assert_eq!(read_file(&table.get(dup).unwrap())[..2], *b"34");
    let child = table.clone();
    assert_eq!(table.get(fd).unwrap().position(), 10);
    assert_eq!(
        run(child.get(fd).unwrap().seek(SeekFrom::Start(5))).unwrap(),
        5
    );
    assert_eq!(table.get(fd).unwrap().position(), 5);
    assert_eq!(
        run(table.get(fd).unwrap().seek(SeekFrom::End(-2))).unwrap(),
        8
    );
    assert_eq!(
        run(table.get(fd).unwrap().seek(SeekFrom::Current(-20))).unwrap_err(),
        Errno::EINVAL
    );
    let mut buf = [0; 2];
    run(table.get(fd).unwrap().read_at(&mut buf, 0)).unwrap();
    assert_eq!((&buf, table.get(fd).unwrap().position()), (b"01", 8));
    assert_eq!(table.get(9).err(), Some(Errno::EBADF));
    // The lowest free descriptor is used again.
    run(table.close(fd)).unwrap();
    assert_eq!(table.dup(dup, 0, false).unwrap(), 0);
}

#[test]
fn appends_land_at_the_end_the_driver_sees() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let releases = ReleaseQueue::new();
    let file = new_file(&fs, &alice, &root, "log", 0o644, b"start");
    let mut append = read_write();
    append.append(true);
    // Two descriptions append in turn, neither knowing the size: neither overwrites the other.
    let one = file_ref(&fs, &alice, &file, append, &releases);
    let two = file_ref(&fs, &alice, &file, append, &releases);
    run(one.write(b"+one")).unwrap();
    run(two.write(b"+two")).unwrap();
    assert_eq!((one.position(), two.position()), (9, 13));
    // The driver places a write at the end it sees, and says where it went.
    let open_ref = open(&fs, &alice, &file, write()).unwrap();
    assert_eq!(
        run(fs.write_at(&open_ref, b"!", WriteAt::End)).unwrap(),
        Written { offset: 13, len: 1 }
    );
    // pwrite with O_APPEND appends, as on Linux, and the position stays.
    assert_eq!(run(one.write_at(b"?", 0)).unwrap(), 1);
    assert_eq!(one.position(), 9);
    // An empty append write moves nothing, as on Linux.
    run(one.seek(SeekFrom::Start(0))).unwrap();
    run(one.write(b"")).unwrap();
    assert_eq!(one.position(), 0);
    // Cleared with fcntl, O_APPEND no longer applies: the next write goes at the position.
    one.set_append(false);
    run(one.seek(SeekFrom::Start(9))).unwrap();
    run(one.write(b"X")).unwrap();
    let reader = file_ref(&fs, &alice, &file, read(), &releases);
    assert_eq!(read_file(&reader), b"start+oneXtwo!?");
    // A write-only description cannot read, and a read-only one cannot write.
    let writer = file_ref(&fs, &alice, &file, write(), &releases);
    assert_eq!(run(writer.read(&mut [0; 1])).unwrap_err(), Errno::EBADF);
    assert_eq!(run(reader.write(b"x")).unwrap_err(), Errno::EBADF);
}

#[test]
fn the_last_reference_releases_on_every_path() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let releases = ReleaseQueue::new();
    let unlinked = |name: &str| {
        let file = new_file(&fs, &alice, &root, name, 0o644, name.as_bytes());
        let reference = file_ref(&fs, &alice, &file, read(), &releases);
        run(fs.unlink(&alice, &root, name)).unwrap();
        reference
    };
    // close releases the last reference at once.
    let mut table = FdTable::new();
    let objects = fs.inode_count();
    let fd = table.insert(unlinked("closed"), false).unwrap();
    run(table.close(fd)).unwrap();
    assert_eq!(fs.inode_count(), objects);
    // A reference kept past the close keeps the description open, as fget does; dropped, it
    // goes to the release queue.
    let fd = table.insert(unlinked("kept"), false).unwrap();
    let kept = table.get(fd).unwrap();
    run(table.close(fd)).unwrap();
    assert_eq!(read_file(&kept), b"kept");
    drop(kept);
    assert_eq!(releases.len(), 1);
    run(releases.flush()).unwrap();
    assert_eq!(fs.inode_count(), objects);
    // A full table gives the reference it refused back.
    let mut full = FdTable::new();
    full.insert(unlinked("first"), false).unwrap();
    while full.dup(0, 0, false).is_ok() {}
    assert_eq!(
        full.insert(unlinked("refused"), false).unwrap_err(),
        Errno::EMFILE
    );
    // A dropped table gives back every reference it held, a forked copy's included.
    let copy = full.clone();
    drop(full);
    drop(copy);
    run(releases.flush()).unwrap();
    assert_eq!((fs.inode_count(), releases.len()), (objects, 0));
    // fork, exec, dup2, dup3 and exit each give up their references.
    let mut parent = FdTable::new();
    let fa = parent.insert(unlinked("a"), false).unwrap();
    let fb = parent.insert(unlinked("b"), true).unwrap();
    let mut child = parent.clone();
    run(child.close_on_exec());
    assert_eq!(child.fds().collect::<Vec<_>>(), vec![fa]);
    assert_eq!(run(parent.dup2(fa, fa)).unwrap(), fa);
    assert_eq!(run(parent.dup3(fa, fa, false)).unwrap_err(), Errno::EINVAL);
    // dup2 over fb closes the last reference to b's description: only a remains.
    assert_eq!(run(parent.dup2(fa, fb)).unwrap(), fb);
    assert_eq!(fs.inode_count(), objects + 1);
    run(parent.close_all());
    assert_eq!(fs.inode_count(), objects + 1);
    run(child.close_all());
    assert_eq!((fs.inode_count(), releases.len()), (objects, 0));
}

#[test]
fn a_call_in_progress_holds_its_description() {
    let fs = fs();
    let root = fs.root();
    let alice = user(1000);
    let releases = ReleaseQueue::new();
    let file = new_file(&fs, &alice, &root, "f", 0o666, b"data");
    let blocker = open(&fs, &alice, &file, write()).unwrap();
    run(fs.try_lock(
        &blocker,
        LockFamily::Posix,
        LockKind::Exclusive,
        LockRange::WHOLE,
    ))
    .unwrap();
    let mut table = FdTable::new();
    let fd = table
        .insert(file_ref(&fs, &alice, &file, read_write(), &releases), false)
        .unwrap();
    run(fs.unlink(&alice, &root, "f")).unwrap();
    let objects = fs.inode_count();
    {
        // A process blocks in F_SETLKW through fd, and fd is closed meanwhile: the call holds
        // the description, so nothing is released yet.
        let reference = table.get(fd).unwrap();
        let mut call = pin!(async move {
            reference
                .lock(LockFamily::Posix, LockKind::Exclusive, LockRange::WHOLE)
                .await
        });
        assert!(poll_once(call.as_mut()).is_pending());
        run(table.close(fd)).unwrap();
        assert!(releases.is_empty());
        assert_eq!(fs.inode_count(), objects);
        // The process is killed: the call is dropped, and gives its reference back.
    }
    assert_eq!(releases.len(), 1);
    run(releases.flush()).unwrap();
    run(fs.release(&blocker)).unwrap();
    assert_eq!(fs.inode_count(), objects - 1);
}

// Profiles and identities.

#[test]
fn memory_is_not_durable() {
    assert_eq!(fs().capabilities().durability, Durability::Volatile);
}

#[test]
fn st_dev_is_stable_per_filesystem() {
    let one = FsId("cloud:authority".into());
    assert_eq!(one.dev(), FsId("cloud:authority".into()).dev());
    assert_ne!(one.dev(), FsId("opfs:1234".into()).dev());
    assert_ne!(FsId(String::new()).dev(), 0);
}
