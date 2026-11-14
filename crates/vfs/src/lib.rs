//! The VFS of the architecture contract (docs/architecture/contract.md §2.2, §2.3), for #48
//! (L1a). Portable: no DOM, `web-sys` or `wasm-bindgen` types, so the browser kernel, a native
//! host and a headless test host run the same code.
//!
//! The names follow Rust's standard library where it has the operation ([`OpenOptions`],
//! [`Metadata`], [`FileType`], [`SeekFrom`], `read_at`, `set_len`, `sync_all`, `lock` and
//! `try_lock`), and the Linux kernel's where it does not (`lookup`, `setattr`, `statfs`,
//! `release`, [`FdTable`]).
//!
//! - A filesystem is a [`Filesystem`] driver, addressed by [`Handle`]s, never by paths. Path
//!   walking, `..`, symlinks and mount crossings belong to the client (§2.3); a driver resolves
//!   one name at a time ([`Filesystem::lookup`]) or several within itself ([`Filesystem::walk`]).
//! - Every call names the [`Cred`] acting, and the driver checks it, as Linux checks the
//!   calling process's credentials. The rules are `os_core::inode`'s, which the server's
//!   kernel shares.
//! - [`Filesystem::open`] and [`Filesystem::create`] return an [`OpenRef`]: the driver's open
//!   reference, which records the credential and access granted at open. Reads and writes
//!   through it check only that access (§2.3).
//! - [`FileRef`] is a counted reference to an open file description ([`File`], Linux's
//!   `struct file`), and [`FdTable`] is one process's descriptors, whatever the driver behind
//!   them.
//! - [`memfs`] is a complete in-memory driver: the reference the other drivers are tested
//!   against, and the instance's tmpfs.

pub mod fdtable;
pub mod file;
pub mod memfs;
pub mod profiles;

pub use fdtable::{FD_MAX, Fd, FdTable};
pub use file::{File, FileRef, ReleaseQueue};
pub use std::io::SeekFrom;

use abi::Errno;
pub use abi::Time;
use abi::contract::FsCapabilities;
use bitflags::bitflags;
pub use os_core::permissions::Identity;
use os_core::permissions::{EXEC, READ, WRITE};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::ops::{Range, RangeFrom};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

pub type Result<T, E = Errno> = std::result::Result<T, E>;
/// A driver call's result. Not `Send`: the browser kernel is single-threaded.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + 'a>>;

/// A filesystem's identity (contract §2.5): the authority's id, a UUID, a helper folder's id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FsId(pub String);
impl FsId {
    /// `st_dev`: a stable number for this filesystem, the same wherever it is mounted (FNV-1a of
    /// its id, never 0).
    pub fn dev(&self) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.0.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash.max(1)
    }
}

/// An object in a filesystem: what a handle names. The generation distinguishes an inode
/// number used again after its file is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId {
    pub ino: u64,
    pub generation: u64,
}

/// A handle (contract §2.3): names one object of one filesystem. `proof` is whatever the driver
/// or service needs to know the handle is its own (empty for local drivers; a server-issued tag
/// for remote ones). A handle grants nothing by itself: every call still checks its [`Cred`].
/// A handle does not keep its object alive: an open reference does, including a path-only one
/// ([`OpenOptions::path`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Handle {
    pub fs: FsId,
    pub node: NodeId,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub proof: String,
}

/// The credential a call acts with (contract §2.4), as Linux's `struct cred`: who, and, for
/// remote drivers, the credential the identity service issued. Local drivers check `identity`.
#[derive(Clone, Debug)]
pub struct Cred {
    pub identity: Identity,
    pub umask: u16,
    /// The remote credential presented with this call, if the driver is remote.
    pub token: Option<String>,
}
impl Cred {
    pub fn new(identity: Identity) -> Self {
        Self {
            identity,
            umask: os_core::permissions::UMASK,
            token: None,
        }
    }
    pub fn root() -> Self {
        Self::new(Identity::root())
    }
}

/// What kind of object a node is, as `std::fs::FileType` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileType {
    File,
    Dir,
    Symlink,
    CharDevice,
}
impl FileType {
    pub fn is_file(self) -> bool {
        self == FileType::File
    }
    pub fn is_dir(self) -> bool {
        self == FileType::Dir
    }
    pub fn is_symlink(self) -> bool {
        self == FileType::Symlink
    }
    pub fn is_char_device(self) -> bool {
        self == FileType::CharDevice
    }
    /// The `S_IFMT` bits of `st_mode`.
    pub fn mode(self) -> u32 {
        match self {
            FileType::File => abi::S_IFREG,
            FileType::Dir => abi::S_IFDIR,
            FileType::Symlink => abi::S_IFLNK,
            FileType::CharDevice => abi::S_IFCHR,
        }
    }
}

/// Seconds and nanoseconds since 1970, as Linux's `struct timespec64`.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Timespec {
    pub sec: u64,
    pub nsec: u32,
}
impl From<(u64, u32)> for Timespec {
    fn from((sec, nsec): (u64, u32)) -> Self {
        Self { sec, nsec }
    }
}

/// What a filesystem tells of an object, as `std::fs::Metadata` and Linux's `struct kstat`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub file_type: FileType,
    /// Permission and special bits (`0o7777`).
    pub mode: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    /// A device's driver.
    pub rdev: Option<String>,
    /// Whether it has an ACL beyond its mode.
    pub acl: bool,
}
impl Metadata {
    pub fn is_file(&self) -> bool {
        self.file_type.is_file()
    }
    pub fn is_dir(&self) -> bool {
        self.file_type.is_dir()
    }
    pub fn is_symlink(&self) -> bool {
        self.file_type.is_symlink()
    }
    /// `st_mode`: the type bits and the permission bits.
    pub fn st_mode(&self) -> u32 {
        self.file_type.mode() | u32::from(self.mode)
    }
}

/// An object found by name: its handle and metadata (Linux's dentry and inode, as a client
/// keeps them).
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub handle: Handle,
    pub metadata: Metadata,
}

/// Why [`Filesystem::walk`] stopped before its last name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The name at this index does not exist.
    Missing(usize),
    /// The name at this index is a symbolic link: the client reads and follows it.
    Symlink(usize),
    /// The name at this index is not a directory, and more names follow it.
    NotDirectory(usize),
}

/// What [`Filesystem::walk`] resolved: one entry per name resolved, and why it stopped, if it did.
#[derive(Clone, Debug, PartialEq)]
pub struct Walked {
    pub entries: Vec<Entry>,
    pub stop: Option<Stop>,
}

/// A directory entry, as `read_dir` gives it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub node: NodeId,
    pub file_type: FileType,
}

/// The access an open reference grants (contract §2.3), as Linux's `FMODE_*`. Exec is checked
/// against execute permission, not read, and serves only the loader. Path (`O_PATH`) grants no
/// access at all: it keeps the object alive and anchors later calls, as a working directory, a
/// process root or a mount point needs, and asks no permission of the object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Access {
    Read,
    Write,
    ReadWrite,
    Exec,
    Path,
}
impl Access {
    pub fn reads(self) -> bool {
        matches!(self, Access::Read | Access::ReadWrite | Access::Exec)
    }
    pub fn writes(self) -> bool {
        matches!(self, Access::Write | Access::ReadWrite)
    }
    /// The permission bits it needs (`os_core::permissions` READ, WRITE, EXEC); none for Path.
    pub fn mask(self) -> u16 {
        match self {
            Access::Read => READ,
            Access::Write => WRITE,
            Access::ReadWrite => READ | WRITE,
            Access::Exec => EXEC,
            Access::Path => 0,
        }
    }
}

/// How to open a file, as `std::fs::OpenOptions`, with what Linux adds: execute access for the
/// loader, and path-only opens (`O_PATH`). A driver reads the access, `truncate`, `create` and
/// `create_new`; `append` belongs to the description ([`File`]), which places each write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    // The Rust builder grants write access for append; raw O_APPEND is only a status flag.
    append_implies_write: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
    exec: bool,
    path: bool,
}
impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }
    /// Every write goes at the end of the file; implies write.
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self.append_implies_write = append;
        self
    }
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }
    /// Create the file, failing with EEXIST if the name is taken (`O_CREAT | O_EXCL`).
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }
    /// Execute access, for the loader.
    pub fn exec(&mut self, exec: bool) -> &mut Self {
        self.exec = exec;
        self
    }
    /// A path-only open (`O_PATH`): no access, only a reference that keeps the object alive.
    pub fn path(&mut self, path: bool) -> &mut Self {
        self.path = path;
        self
    }
    /// From open(2)'s flags: the access mode, `O_APPEND`, `O_TRUNC`, `O_CREAT`, `O_EXCL` and
    /// `O_PATH`.
    pub fn from_flags(flags: u32) -> Result<Self> {
        let mut options = Self::new();
        if flags & abi::O_PATH != 0 {
            return Ok(*options.path(true));
        }
        match flags & abi::O_ACCMODE {
            abi::O_RDONLY => options.read(true),
            abi::O_WRONLY => options.write(true),
            abi::O_RDWR => options.read(true).write(true),
            _ => return Err(Errno::EINVAL),
        };
        options
            .append(flags & abi::O_APPEND != 0)
            .truncate(flags & abi::O_TRUNC != 0)
            .create(flags & abi::O_CREAT != 0)
            .create_new(flags & abi::O_CREAT != 0 && flags & abi::O_EXCL != 0);
        options.append_implies_write = false;
        Ok(options)
    }
    /// The access these options ask: EINVAL for none, or for exec or path with another.
    pub fn access(&self) -> Result<Access> {
        let write = self.write || (self.append && self.append_implies_write);
        match (self.path, self.exec, self.read, write) {
            (true, false, false, false) => Ok(Access::Path),
            (false, true, false, false) => Ok(Access::Exec),
            (false, false, true, false) => Ok(Access::Read),
            (false, false, false, true) => Ok(Access::Write),
            (false, false, true, true) => Ok(Access::ReadWrite),
            _ => Err(Errno::EINVAL),
        }
    }
    pub fn is_append(&self) -> bool {
        self.append
    }
    pub fn is_truncate(&self) -> bool {
        self.truncate
    }
    pub fn is_create(&self) -> bool {
        self.create || self.create_new
    }
    pub fn is_create_new(&self) -> bool {
        self.create_new
    }
}

/// An open reference (contract §2.3): the driver's record of one open, with the credential and
/// access granted then. Released by [`Filesystem::release`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OpenRef {
    pub fs: FsId,
    pub id: u64,
    pub node: NodeId,
    pub access: Access,
}

/// Attribute changes, as Linux's `struct iattr`: chmod, chown, utimensat and truncate. `None`
/// and [`Time::Omit`] leave one alone. A driver checks them all before it changes any, as
/// `os_core::inode` decides.
#[derive(Clone, Debug, PartialEq)]
pub struct SetAttr {
    pub mode: Option<u16>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Time,
    pub mtime: Time,
}
impl Default for SetAttr {
    fn default() -> Self {
        Self {
            mode: None,
            uid: None,
            gid: None,
            size: None,
            atime: Time::Omit,
            mtime: Time::Omit,
        }
    }
}

/// Where a write goes (contract §2.3): at an offset, or at the end of the file as it is when
/// the write lands, which the driver decides atomically (`O_APPEND`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriteAt {
    Offset(u64),
    End,
}

/// What a write did: the offset its data went to, and how many bytes went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Written {
    pub offset: u64,
    pub len: usize,
}

/// The largest file size and offset, as Linux's `MAX_LFS_FILESIZE` (`off_t` is 64 bits on
/// every target): a write or truncate past it fails with `EFBIG`.
pub const FILE_MAX: u64 = i64::MAX as u64;

bitflags! {
    /// renameat2(2)'s flags; RENAME_NOREPLACE is the one the contract requires.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct RenameFlags: u32 {
        const NOREPLACE = 1;
    }
}

bitflags! {
    /// setxattr(2)'s flags.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct XattrFlags: u8 {
        const CREATE = abi::XATTR_CREATE;
        const REPLACE = abi::XATTR_REPLACE;
    }
}

/// An advisory lock's kind, as `std::fs::File::lock_shared` and `lock` take them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockKind {
    Shared,
    Exclusive,
}

/// Which locks a lock is among (contract §3). As on Linux, flock(2) locks and fcntl(2) byte-range
/// locks never conflict with each other. A flock lock covers the whole file and needs no
/// particular access; an fcntl lock needs read access to be shared and write access to be
/// exclusive (EBADF otherwise).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockFamily {
    Flock,
    Posix,
}

/// The bytes a lock covers: from `start` up to `end`, not included, or to the end of the file,
/// however far it grows (fcntl(2)'s `l_len` of 0). Never empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LockRange {
    start: u64,
    end: Option<u64>,
}
impl LockRange {
    /// The whole file, as flock(2) locks it.
    pub const WHOLE: LockRange = LockRange {
        start: 0,
        end: None,
    };
    /// `start..end`, or `start..` without `end`; `None` if that would be empty.
    pub fn new(start: u64, end: Option<u64>) -> Option<Self> {
        match end {
            Some(end) if end <= start => None,
            _ => Some(Self { start, end }),
        }
    }
    pub fn start(self) -> u64 {
        self.start
    }
    /// Where it ends, not included; `None` for the end of the file.
    pub fn end(self) -> Option<u64> {
        self.end
    }
    fn end_or_max(self) -> u64 {
        self.end.unwrap_or(u64::MAX)
    }
    pub fn overlaps(self, other: LockRange) -> bool {
        self.start < other.end_or_max() && other.start < self.end_or_max()
    }
    /// Whether the two overlap or meet, so that one lock may take both.
    pub fn touches(self, other: LockRange) -> bool {
        self.start <= other.end_or_max() && other.start <= self.end_or_max()
    }
    /// The smallest range covering both.
    pub fn union(self, other: LockRange) -> LockRange {
        let end = match (self.end, other.end) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
        LockRange {
            start: self.start.min(other.start),
            end,
        }
    }
    /// What is left of it outside `hole`: up to two pieces, before and after.
    pub fn minus(self, hole: LockRange) -> [Option<LockRange>; 2] {
        if !self.overlaps(hole) {
            return [Some(self), None];
        }
        let before = LockRange::new(self.start, Some(hole.start));
        let after = hole.end.and_then(|end| LockRange::new(end, self.end));
        [before, after]
    }
}
impl From<RangeFrom<u64>> for LockRange {
    fn from(range: RangeFrom<u64>) -> Self {
        LockRange {
            start: range.start,
            end: None,
        }
    }
}
impl TryFrom<Range<u64>> for LockRange {
    type Error = Errno;
    /// EINVAL for an empty range.
    fn try_from(range: Range<u64>) -> Result<Self> {
        LockRange::new(range.start, Some(range.end)).ok_or(Errno::EINVAL)
    }
}

/// A lock that conflicts with one asked for, as F_GETLK reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub kind: LockKind,
    pub range: LockRange,
    pub owner: u64,
}

/// statfs(2), as far as a driver knows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatFs {
    pub block_size: u64,
    pub blocks: u64,
    pub blocks_free: u64,
    pub files: u64,
    pub files_free: u64,
    pub name_max: u64,
}

/// A filesystem driver: the version 2 calls of the contract §2.3, as Linux's
/// `inode_operations` and `file_operations` together. Every call names the credential acting;
/// names are single components, never `.`, `..` or containing `/` (`EINVAL`), and at most
/// `NAME_MAX` bytes (`ENAMETOOLONG`), as [`check_component`] has it. A handle whose object is
/// gone, or whose generation changed, fails with `ESTALE`; a handle the driver did not issue
/// fails with `EACCES`. A directory that has been removed, though still open, takes no new
/// names and lists none (`ENOENT`).
pub trait Filesystem {
    fn id(&self) -> &FsId;
    /// What the driver provides (contract §3); every gate is read against it.
    fn capabilities(&self) -> FsCapabilities;
    fn root(&self) -> Handle;

    fn lookup<'a>(&'a self, cred: &'a Cred, dir: &'a Handle, name: &'a str)
    -> BoxFuture<'a, Entry>;
    /// Resolve `names` one after another from `dir`, within this filesystem, stopping at the
    /// first missing name, symbolic link, or non-directory with names after it. Every name is
    /// checked before any is resolved.
    fn walk<'a>(
        &'a self,
        cred: &'a Cred,
        dir: &'a Handle,
        names: &'a [String],
    ) -> BoxFuture<'a, Walked>;
    /// The directory's parent and the directory's name in it; `None` at the filesystem's root.
    /// It asks no permission, as getcwd(2) asks none; a removed directory has none (ENOENT).
    fn parent<'a>(
        &'a self,
        cred: &'a Cred,
        dir: &'a Handle,
    ) -> BoxFuture<'a, Option<(Entry, String)>>;
    /// getattr: the object's metadata.
    fn metadata<'a>(&'a self, cred: &'a Cred, handle: &'a Handle) -> BoxFuture<'a, Metadata>;
    fn setattr<'a>(
        &'a self,
        cred: &'a Cred,
        handle: &'a Handle,
        attr: &'a SetAttr,
    ) -> BoxFuture<'a, Metadata>;
    fn read_dir<'a>(&'a self, cred: &'a Cred, dir: &'a Handle) -> BoxFuture<'a, Vec<DirEntry>>;
    fn read_link<'a>(&'a self, cred: &'a Cred, link: &'a Handle) -> BoxFuture<'a, String>;

    /// Create a regular file and open it in one call, as open(2) with `O_CREAT` (contract
    /// §2.3): the open reference has the access `options` ask, whatever the new file's mode, as
    /// Linux gives the creator. With `create_new`, an existing name fails with `EEXIST`;
    /// without it, an existing file is opened as [`Filesystem::open`] would, truncated if
    /// `options` ask, in the same call, and a directory fails with `EISDIR`.
    fn create<'a>(
        &'a self,
        cred: &'a Cred,
        dir: &'a Handle,
        name: &'a str,
        mode: u16,
        options: &'a OpenOptions,
    ) -> BoxFuture<'a, (Entry, OpenRef)>;
    fn mkdir<'a>(
        &'a self,
        cred: &'a Cred,
        dir: &'a Handle,
        name: &'a str,
        mode: u16,
    ) -> BoxFuture<'a, Entry>;
    fn mknod<'a>(
        &'a self,
        cred: &'a Cred,
        dir: &'a Handle,
        name: &'a str,
        mode: u16,
        rdev: &'a str,
    ) -> BoxFuture<'a, Entry>;
    fn symlink<'a>(
        &'a self,
        cred: &'a Cred,
        dir: &'a Handle,
        name: &'a str,
        target: &'a str,
    ) -> BoxFuture<'a, Entry>;
    fn link<'a>(
        &'a self,
        cred: &'a Cred,
        target: &'a Handle,
        dir: &'a Handle,
        name: &'a str,
    ) -> BoxFuture<'a, Entry>;
    fn unlink<'a>(&'a self, cred: &'a Cred, dir: &'a Handle, name: &'a str) -> BoxFuture<'a, ()>;
    fn rmdir<'a>(&'a self, cred: &'a Cred, dir: &'a Handle, name: &'a str) -> BoxFuture<'a, ()>;
    #[allow(clippy::too_many_arguments)]
    fn rename<'a>(
        &'a self,
        cred: &'a Cred,
        from_dir: &'a Handle,
        from: &'a str,
        to_dir: &'a Handle,
        to: &'a str,
        flags: RenameFlags,
    ) -> BoxFuture<'a, ()>;

    fn getxattr<'a>(
        &'a self,
        cred: &'a Cred,
        handle: &'a Handle,
        name: &'a str,
    ) -> BoxFuture<'a, Vec<u8>>;
    fn setxattr<'a>(
        &'a self,
        cred: &'a Cred,
        handle: &'a Handle,
        name: &'a str,
        value: &'a [u8],
        flags: XattrFlags,
    ) -> BoxFuture<'a, ()>;
    fn listxattr<'a>(&'a self, cred: &'a Cred, handle: &'a Handle) -> BoxFuture<'a, Vec<String>>;
    fn removexattr<'a>(
        &'a self,
        cred: &'a Cred,
        handle: &'a Handle,
        name: &'a str,
    ) -> BoxFuture<'a, ()>;
    fn statfs<'a>(&'a self, cred: &'a Cred) -> BoxFuture<'a, StatFs>;

    /// Open an existing object, checking the access `options` ask once (contract §2.3). A
    /// directory opens only for reading or as a path. `truncate` needs write permission, even
    /// with read-only access, as on Linux, and empties a regular file in the same call.
    fn open<'a>(
        &'a self,
        cred: &'a Cred,
        handle: &'a Handle,
        options: &'a OpenOptions,
    ) -> BoxFuture<'a, OpenRef>;
    /// pread: read into `buf` from `offset`, and say how much was read (0 at the end).
    fn read_at<'a>(
        &'a self,
        open: &'a OpenRef,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, usize>;
    /// pwrite: write `buf` at `at`, and say where it went and how much went, which may be
    /// less than all of it (a short write). Nothing at all changes for an empty `buf`.
    fn write_at<'a>(
        &'a self,
        open: &'a OpenRef,
        buf: &'a [u8],
        at: WriteAt,
    ) -> BoxFuture<'a, Written>;
    /// ftruncate: a regular file open for writing only (EINVAL otherwise).
    fn set_len<'a>(&'a self, open: &'a OpenRef, size: u64) -> BoxFuture<'a, ()>;
    /// fsync: return after the driver's durability point (contract §3).
    fn sync_all<'a>(&'a self, open: &'a OpenRef) -> BoxFuture<'a, ()>;
    /// fdatasync: the data and what is needed to read it back, at least.
    fn sync_data<'a>(&'a self, open: &'a OpenRef) -> BoxFuture<'a, ()> {
        self.sync_all(open)
    }
    /// Take an advisory lock for `open`, waiting until it is granted (F_SETLKW, flock without
    /// LOCK_NB). Dropping the future gives up the wait. An fcntl wait that would deadlock fails
    /// with EDEADLK; flock does not detect deadlock, as on Linux.
    fn lock<'a>(
        &'a self,
        open: &'a OpenRef,
        family: LockFamily,
        kind: LockKind,
        range: LockRange,
    ) -> BoxFuture<'a, ()>;
    /// Take an advisory lock for `open`, or fail at once with EAGAIN (F_SETLK, LOCK_NB).
    fn try_lock<'a>(
        &'a self,
        open: &'a OpenRef,
        family: LockFamily,
        kind: LockKind,
        range: LockRange,
    ) -> BoxFuture<'a, ()>;
    fn unlock<'a>(
        &'a self,
        open: &'a OpenRef,
        family: LockFamily,
        range: LockRange,
    ) -> BoxFuture<'a, ()>;
    /// F_GETLK: the first fcntl lock of another open reference that would conflict.
    fn test_lock<'a>(
        &'a self,
        open: &'a OpenRef,
        kind: LockKind,
        range: LockRange,
    ) -> BoxFuture<'a, Option<Conflict>>;
    /// End the open reference: its locks go, and an unlinked file's content goes with its last
    /// open reference.
    fn release<'a>(&'a self, open: &'a OpenRef) -> BoxFuture<'a, ()>;
}

/// Whether `name` may be one component of a path: `ENAMETOOLONG` past `NAME_MAX` bytes, as
/// Linux says before anything else, and `EINVAL` for an empty name, `.`, `..`, or one with `/`
/// or NUL.
pub fn check_component(name: &str) -> Result<()> {
    if name.len() > abi::NAME_MAX {
        return Err(Errno::ENAMETOOLONG);
    }
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

/// Poll a future once: its output if it is ready, as `futures::FutureExt::now_or_never`.
/// Tests and synchronous callers use this for calls that cannot wait. A contended lock can
/// return `None`, which cancels its wait when this helper drops the future.
pub fn now_or_never<F: Future>(future: F) -> Option<F::Output> {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}
