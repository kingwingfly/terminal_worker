//! /usr: the system's own files, read-only, as an image-based system mounts its /usr (systemd's
//! hermetic /usr): what the build put there (the Worker's assets), not R2's. /bin is a link to
//! usr/bin, as on merged-/usr distributions (Arch, Fedora, Debian).
//!
//! /usr/bin holds what a name typed runs. The terminal's own programs (ls, chmod, passwd…), built
//! into the page's binary (`/pkg/frontend_bg.wasm`), are each a file of it, as a distribution's
//! programs are each a file: 755, root's; those that act beyond the caller's own rights by a
//! policy of their own, setuid root (4755), as passwd, su and sudo are on Linux, and useradd and
//! userdel here, with which a guest makes an account of its own and an account deletes itself.
//! Here the bit says, rather than grants: the page cannot be trusted with a setuid program, so
//! the kernel's calls hold each one's policy. uutils' utilities the terminal has none of its own
//! for are links to their binary, `coreutils` (wasm32-wasip1, run by the page's WASI engine), as
//! busybox installs its own. The shell's builtins (cd, exit, logout, jobs, help) are not there.
//!
//! The store answers for these names itself (db.rs), so that the kernel resolves, lists, stats
//! and permits them as it does any; changing any of them fails with EROFS (sys.rs).
use std::cell::{Cell, RefCell};
use terminal_worker_core::permissions::Node;

/// What each binary's content is, as a node names it (none of R2's), and where the Worker's
/// assets have it.
pub const COREUTILS: &str = "usr:coreutils";
pub const TERMINAL: &str = "usr:terminal";
pub fn asset(content: &str) -> Option<&'static str> {
    match content {
        COREUTILS => Some("/usr/bin/coreutils.wasm"),
        TERMINAL => Some("/pkg/frontend_bg.wasm"),
        _ => None,
    }
}
/// What the build says of the binaries: their sizes (scripts/build.mjs).
pub const MANIFEST: &str = "/usr/manifest.json";
/// The shell's builtins, which no file is.
const BUILTINS: &[&str] = &["cd", "exit", "logout", "jobs", "help"];
/// The programs setuid root.
const SETUID: &[&str] = &["passwd", "su", "sudo", "useradd", "userdel"];
/// The inodes of /usr, above any the store numbers.
const INO: u64 = 1 << 40;

/// An entry of /usr/bin: a file of a binary, or a link to `coreutils`.
#[derive(Clone)]
struct Program {
    name: String,
    content: Option<&'static str>,
}

thread_local! {
    /// The binaries' sizes, and when this instance mounted /usr, as its times.
    static SIZES: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    static MOUNTED: Cell<(u64, u32)> = const { Cell::new((0, 0)) };
    static PROGRAMS: RefCell<Vec<Program>> = const { RefCell::new(Vec::new()) };
}

/// /usr/bin, by name: the terminal's programs, coreutils, and links to it of the utilities
/// the terminal has no program of its own for.
fn programs() -> Vec<Program> {
    PROGRAMS.with(|programs| {
        let mut programs = programs.borrow_mut();
        if programs.is_empty() {
            let own: Vec<String> = terminal_worker_core::cli::names()
                .iter()
                .filter(|name| !BUILTINS.contains(&name.as_str()))
                .cloned()
                .collect();
            let mut all: Vec<Program> = own
                .iter()
                .map(|name| Program {
                    name: name.clone(),
                    content: Some(TERMINAL),
                })
                .chain(std::iter::once(Program {
                    name: "coreutils".into(),
                    content: Some(COREUTILS),
                }))
                .chain(
                    terminal_worker_core::bin::PROGRAMS
                        .iter()
                        .filter(|name| !own.iter().any(|own| own == *name))
                        .map(|name| Program {
                            name: name.to_string(),
                            content: None,
                        }),
                )
                .collect();
            all.sort_by(|a, b| a.name.cmp(&b.name));
            *programs = all;
        }
        programs.clone()
    })
}

/// Mount /usr: coreutils is `coreutils` bytes, the page's binary `terminal`, and this is when.
pub fn mount(coreutils: u64, terminal: u64) {
    SIZES.with(|cell| cell.set((coreutils, terminal)));
    MOUNTED.with(|cell| cell.set(crate::auth::clock()));
}

/// Whether `path`, as the kernel resolved it, is the image's: /usr, what is in it, and /bin.
pub fn is(path: &str) -> bool {
    path == "/usr" || path.starts_with("/usr/") || path == "/bin"
}

/// The node at `path`, if it is the image's.
pub fn node(path: &str) -> Option<Node> {
    // One name each, but for the directories, which count theirs.
    let numbered = |node: Node, ino: u64| Node {
        ino: INO + ino,
        links: node.links.max(1),
        ..node
    };
    Some(match path {
        "/bin" => numbered(Node::symlink(0, 0, "usr/bin"), 0),
        "/usr" => numbered(directory(1), 1),
        "/usr/bin" => numbered(directory(0), 2),
        _ => {
            let name = path.strip_prefix("/usr/bin/")?;
            let programs = programs();
            let at = programs.iter().position(|program| program.name == name)?;
            let (coreutils, terminal) = SIZES.with(Cell::get);
            let node = match programs[at].content {
                Some(content) => {
                    let mode = if SETUID.contains(&name) {
                        0o4755
                    } else {
                        0o755
                    };
                    let size = if content == COREUTILS {
                        coreutils
                    } else {
                        terminal
                    };
                    Node::file(0, 0, mode, content.into(), size)
                }
                None => Node::symlink(0, 0, "coreutils"),
            };
            numbered(node, 3 + at as u64)
        }
    })
}

/// A directory of the image, root's, 755, with `subdirectories` in it.
fn directory(subdirectories: u32) -> Node {
    Node {
        links: 2 + subdirectories,
        ..Node::directory(0, 0, 0o755)
    }
}

/// What is in the image's directory `path`, if it is one of them; and in /, the image's names.
pub fn children(path: &str) -> Option<Vec<(String, Node)>> {
    let names: Vec<String> = match path {
        "/" => vec!["/bin".into(), "/usr".into()],
        "/usr" => vec!["/usr/bin".into()],
        "/usr/bin" => programs()
            .into_iter()
            .map(|program| format!("/usr/bin/{}", program.name))
            .collect(),
        _ => return None,
    };
    Some(
        names
            .into_iter()
            .filter_map(|name| node(&name).map(|node| (name, node)))
            .collect(),
    )
}

/// The times of the image's inode `ino`: when it was mounted.
pub fn times(ino: u64) -> Option<kernel::Times> {
    let at = MOUNTED.with(Cell::get);
    (ino >= INO && ino < INO + 3 + programs().len() as u64).then_some(kernel::Times {
        atime: at,
        mtime: at,
        ctime: at,
    })
}

/// What statfs tells of the image: its blocks, all used.
pub fn statfs() -> abi::StatFs {
    let (coreutils, terminal) = SIZES.with(Cell::get);
    let blocks = coreutils.div_ceil(4096) + terminal.div_ceil(4096);
    abi::StatFs {
        bsize: 4096,
        blocks,
        files: 3 + programs().len() as u64,
        namelen: abi::NAME_MAX as u64,
        ..abi::StatFs::default()
    }
}
