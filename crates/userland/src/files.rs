//! Programs on names: ls, stat, mkdir, rmdir, rm, mv, ln, readlink, realpath and touch, as
//! GNU's are.
use crate::io::{Io, View, basename, joined, quote};
use crate::sys::{Sys, call, calls, done, getdents, lstat, stat};
use crate::walk;
use abi::{Call, Errno, Error, MAXSYMLINKS, R_OK, Stat, X_OK};
use serde_json::Value;
use terminal_worker_core::cli::{files as args, parse};
use terminal_worker_core::permissions::type_mode_text;
use terminal_worker_core::{Entry, kind, resolve};

/// Parse the program's arguments as its definition says, or report why not and return 1.
macro_rules! parsed {
    ($io:expr, $args:ty) => {
        match parse::<$args>($io.rest()) {
            Ok(parsed) => parsed,
            Err(error) => {
                $io.fail(error);
                return 1;
            }
        }
    };
}
pub(crate) use parsed;

/// `path` as the terminal shows it: absolute, `.` and `..` taken away as written.
pub(crate) fn shown_path(path: &str) -> String {
    resolve("/", path).unwrap_or_else(|_| path.to_string())
}

/// An entry of a listing: the node `name`, at `path`, as its lstat says; as what it opens as
/// (`opened`: through a link, what it leads to), and whether the account may open it.
fn entry(
    name: &str,
    path: String,
    lstat: Option<&Stat>,
    opened: Option<&Stat>,
    denied: bool,
) -> Entry {
    let Some(lstat) = lstat else {
        // In a directory that may be read but not searched: a name, nothing more.
        return Entry {
            name: name.to_string(),
            path,
            kind: kind(name).into(),
            owner: "?".into(),
            group: "?".into(),
            denied: true,
            ..Entry::default()
        };
    };
    let directory = opened.unwrap_or(lstat).is_dir();
    Entry {
        name: name.to_string(),
        path,
        directory,
        size: lstat.size,
        kind: if directory { "directory" } else { kind(name) }.into(),
        owner: lstat.user.clone(),
        group: lstat.group.clone(),
        mode: lstat.perm(),
        uploaded: lstat.writing.as_ref().map(|writing| writing.received),
        recording: lstat
            .writing
            .as_ref()
            .is_some_and(|writing| writing.recording),
        link: lstat.link.clone(),
        links: lstat.nlink,
        acl: lstat.acl,
        denied,
    }
}

/// Whether an answer of access(2) says no: a link leading nowhere is broken, not forbidden.
fn refused<T>(answer: &Result<T, Error>) -> bool {
    answer
        .as_ref()
        .is_err_and(|error| matches!(error.errno, Errno::EACCES | Errno::EPERM))
}

/// The entries of the directory `dir` (absolute), at `shown` as the terminal shows it, as ls
/// and the file panel show them: directories first, then by name; each as what it opens as,
/// and whether the account may open it (list a directory, read a file), which one batch asks.
pub async fn entries(sys: &impl Sys, dir: &str, shown: &str) -> Result<Vec<Entry>, Error> {
    let dirents = getdents(sys, dir).await?;
    let mut asked = Vec::new();
    for dirent in &dirents {
        let path = joined(dir, &dirent.name);
        asked.push(Call::Stat { path: path.clone() });
        asked.push(Call::Access {
            path: path.clone(),
            mode: R_OK,
        });
        asked.push(Call::Access {
            path,
            mode: R_OK | X_OK,
        });
    }
    let mut answers = calls::<Value>(sys, asked).await.into_iter();
    let mut entries: Vec<Entry> = dirents
        .iter()
        .map(|dirent| {
            let opened: Option<Stat> = answers
                .next()
                .and_then(Result::ok)
                .and_then(|value| serde_json::from_value(value).ok());
            let (read, list) = (answers.next(), answers.next());
            let directory = opened.as_ref().is_some_and(Stat::is_dir);
            let allowed = if directory { list } else { read };
            let path = joined(shown, &dirent.name);
            entry(
                &dirent.name,
                path,
                dirent.stat.as_ref(),
                opened.as_ref(),
                allowed.is_some_and(|answer| refused(&answer)),
            )
        })
        .collect();
    entries.sort_by(|a, b| {
        b.directory
            .cmp(&a.directory)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

pub async fn ls(sys: &impl Sys, io: &mut Io<'_>) -> i32 {
    let ls = parsed!(io, args::Ls);
    let typed = ls.path.clone().unwrap_or_else(|| ".".into());
    let path = io.path(&typed);
    // A link to a directory lists the directory, but with -l or -d, itself, as GNU ls shows a
    // link named on the command line; a trailing / follows it.
    let follow = !(ls.long || ls.directory) || typed.ends_with('/');
    let found = if follow {
        stat(sys, &path).await
    } else {
        lstat(sys, &path).await
    };
    let node = match found {
        Ok(node) => node,
        Err(error) => {
            io.fail(format!("ls: cannot access {}: {error}", quote(&typed)));
            return 2;
        }
    };
    if node.is_dir() && !ls.directory {
        return match entries(sys, &path, &shown_path(&path)).await {
            Ok(mut list) => {
                if !ls.all {
                    list.retain(|entry| !entry.name.starts_with('.'));
                }
                io.view = Some(View::Listing {
                    entries: list,
                    long: ls.long,
                });
                0
            }
            Err(error) => {
                io.fail(format!(
                    "ls: cannot open directory {}: {error}",
                    quote(&typed)
                ));
                2
            }
        };
    }
    let opened = if node.is_link() {
        stat(sys, &path).await.ok()
    } else {
        None
    };
    let mode = if opened.as_ref().unwrap_or(&node).is_dir() {
        R_OK | X_OK
    } else {
        R_OK
    };
    let denied = refused(
        &done(
            sys,
            Call::Access {
                path: path.clone(),
                mode,
            },
        )
        .await,
    );
    io.view = Some(View::Listing {
        entries: vec![entry(
            &typed,
            shown_path(&path),
            Some(&node),
            opened.as_ref(),
            denied,
        )],
        long: ls.long,
    });
    0
}

/// `seconds` since 1970 as a date and time in UTC, as stat shows them.
fn datetime(seconds: u64) -> String {
    let (days, rest) = ((seconds / 86_400) as i64, seconds % 86_400);
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} +0000",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

pub async fn stat_program(sys: &impl Sys, io: &mut Io<'_>) -> i32 {
    let stat = parsed!(io, args::Stat);
    let mut status = 0;
    for typed in &stat.files {
        let path = io.path(typed);
        let found = if stat.dereference {
            self::stat_of(sys, &path, true).await
        } else {
            self::stat_of(sys, &path, false).await
        };
        let node = match found {
            Ok(node) => node,
            Err(error) => {
                io.fail(format!("stat: cannot statx {}: {error}", quote(typed)));
                status = 1;
                continue;
            }
        };
        let what = match () {
            _ if node.is_dir() => "directory",
            _ if node.is_link() => "symbolic link",
            _ if node.is_device() => "character special file",
            _ if node.size == 0 => "regular empty file",
            _ => "regular file",
        };
        let name = match &node.link {
            Some(target) => format!("{} -> {}", quote(typed), quote(target)),
            None => quote(typed),
        };
        let kind = if node.is_dir() {
            'd'
        } else if node.is_link() {
            'l'
        } else if node.is_device() {
            'c'
        } else {
            '-'
        };
        io.say(format!("  File: {name}"));
        io.say(format!("  Size: {:<15} {what}", node.size));
        io.say(format!(" Inode: {:<15} Links: {}", node.ino, node.nlink));
        io.say(format!(
            "Access: ({:04o}/{})  Uid: ({:>5}/{:>8})   Gid: ({:>5}/{:>8})",
            node.perm(),
            type_mode_text(node.perm(), kind),
            node.uid,
            node.user,
            node.gid,
            node.group
        ));
        io.say(format!("Modify: {}", datetime(node.mtime)));
        io.say(format!("Change: {}", datetime(node.ctime)));
    }
    status
}

async fn stat_of(sys: &impl Sys, path: &str, follow: bool) -> Result<Stat, Error> {
    if follow {
        stat(sys, path).await
    } else {
        lstat(sys, path).await
    }
}

pub async fn mkdir(sys: &impl Sys, io: &mut Io<'_>) -> i32 {
    let mkdir = parsed!(io, args::Mkdir);
    let mut status = 0;
    for typed in &mkdir.dirs {
        let path = io.path(typed);
        let made = if mkdir.parents {
            parents(sys, &path).await
        } else {
            done(
                sys,
                Call::Mkdir {
                    path: path.clone(),
                    mode: 0o777,
                },
            )
            .await
        };
        if let Err(error) = made {
            io.fail(format!(
                "mkdir: cannot create directory {}: {error}",
                quote(typed)
            ));
            status = 1;
        }
    }
    status
}

/// mkdir -p: `path` and each directory above it that is not there, in one batch. One that is
/// there is fine if it is a directory.
async fn parents(sys: &impl Sys, path: &str) -> Result<(), Error> {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let prefixes: Vec<String> = (1..=parts.len())
        .map(|n| format!("/{}", parts[..n].join("/")))
        .collect();
    let made = calls::<Value>(
        sys,
        prefixes
            .iter()
            .map(|path| Call::Mkdir {
                path: path.clone(),
                mode: 0o777,
            })
            .collect(),
    )
    .await;
    for (prefix, result) in prefixes.iter().zip(made) {
        match result {
            Ok(_) => {}
            Err(error) if error.errno == Errno::EEXIST => {
                if !stat(sys, prefix).await.is_ok_and(|node| node.is_dir()) {
                    return Err(Errno::ENOTDIR.into());
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub async fn rmdir(sys: &impl Sys, io: &mut Io<'_>) -> i32 {
    let rmdir = parsed!(io, args::Rmdir);
    let mut status = 0;
    for typed in &rmdir.dirs {
        // -p: then each directory above it, as written, while one goes.
        let mut names = vec![typed.trim_end_matches('/').to_string()];
        if rmdir.parents {
            let mut above = names[0].clone();
            while let Some((dir, _)) = above.rsplit_once('/') {
                if dir.is_empty() {
                    break;
                }
                names.push(dir.to_string());
                above = dir.to_string();
            }
        }
        for name in names {
            if let Err(error) = done(
                sys,
                Call::Rmdir {
                    path: io.path(&name),
                },
            )
            .await
            {
                io.fail(format!("rmdir: failed to remove {}: {error}", quote(&name)));
                status = 1;
                break;
            }
        }
    }
    status
}

pub async fn rm(sys: &impl Sys, io: &mut Io<'_>) -> i32 {
    let rm = parsed!(io, args::Rm);
    let mut status = 0;
    for typed in &rm.paths {
        if (io.cancelled)() {
            return 130;
        }
        if typed.is_empty() {
            io.fail("rm: cannot remove '': No such file or directory");
            status = 1;
            continue;
        }
        if matches!(basename(typed), "." | "..") {
            io.fail(format!(
                "rm: refusing to remove '.' or '..' directory: skipping {}",
                quote(typed)
            ));
            status = 1;
            continue;
        }
        let path = io.path(typed);
        if rm.recursive && shown_path(&path) == "/" {
            io.fail("rm: it is dangerous to operate recursively on '/'");
            io.fail("rm: use --no-preserve-root to override this failsafe");
            status = 1;
            continue;
        }
        let node = match lstat(sys, &path).await {
            Ok(node) => node,
            Err(error) if rm.force && error.errno == Errno::ENOENT => continue,
            Err(error) => {
                io.fail(format!("rm: cannot remove {}: {error}", quote(typed)));
                status = 1;
                continue;
            }
        };
        if !node.is_dir() {
            if let Err(error) = done(sys, Call::Unlink { path }).await {
                io.fail(format!("rm: cannot remove {}: {error}", quote(typed)));
                status = 1;
            }
            continue;
        }
        if !rm.recursive {
            io.fail(format!(
                "rm: cannot remove {}: Is a directory",
                quote(typed)
            ));
            status = 1;
            continue;
        }
        if !remove_tree(sys, io, typed, &path).await {
            status = 1;
        }
    }
    status
}

/// rm -r of the directory `path`, `typed` as written: what is in it, files first, then its
/// directories, the deepest first, a batch a level; then it. What cannot be removed is said,
/// and the directories holding it are left. Whether all went.
async fn remove_tree(sys: &impl Sys, io: &mut Io<'_>, typed: &str, path: &str) -> bool {
    let tree = walk::below(sys, path, None, io.cancelled).await;
    let named = |rel: &str| joined(typed, rel);
    let mut failed: Vec<String> = Vec::new();
    for (rel, _, error) in &tree.unread {
        io.fail(format!("rm: cannot remove {}: {error}", quote(&named(rel))));
        failed.push(rel.clone());
    }
    let (dirs, files): (Vec<&walk::Found>, Vec<&walk::Found>) = tree
        .found
        .iter()
        .partition(|found| found.stat.as_ref().is_some_and(Stat::is_dir));
    let unlinked = calls::<Value>(
        sys,
        files
            .iter()
            .map(|found| Call::Unlink {
                path: found.abs.clone(),
            })
            .collect(),
    )
    .await;
    for (found, result) in files.iter().zip(unlinked) {
        if let Err(error) = result {
            io.fail(format!(
                "rm: cannot remove {}: {error}",
                quote(&named(&found.rel))
            ));
            failed.push(found.rel.clone());
        }
    }
    // A directory goes once nothing below it failed to.
    let holds = |dir: &str, failed: &[String]| {
        failed
            .iter()
            .any(|rel| rel == dir || rel.starts_with(&format!("{dir}/")))
    };
    let deepest = dirs.iter().map(|found| found.depth).max().unwrap_or(0);
    for depth in (1..=deepest).rev() {
        if (io.cancelled)() {
            return false;
        }
        let level: Vec<&&walk::Found> = dirs
            .iter()
            .filter(|found| found.depth == depth && !holds(&found.rel, &failed))
            .collect();
        let removed = calls::<Value>(
            sys,
            level
                .iter()
                .map(|found| Call::Rmdir {
                    path: found.abs.clone(),
                })
                .collect(),
        )
        .await;
        for (found, result) in level.iter().zip(removed) {
            if let Err(error) = result {
                io.fail(format!(
                    "rm: cannot remove {}: {error}",
                    quote(&named(&found.rel))
                ));
                failed.push(found.rel.clone());
            }
        }
    }
    if !failed.is_empty() {
        return false;
    }
    match done(
        sys,
        Call::Rmdir {
            path: path.to_string(),
        },
    )
    .await
    {
        Ok(()) => true,
        Err(error) => {
            io.fail(format!("rm: cannot remove {}: {error}", quote(typed)));
            false
        }
    }
}

/// Where SOURCEs go: into DEST if it is, or leads to, a directory; else to DEST itself.
async fn destination(sys: &impl Sys, io: &Io<'_>, dest: &str) -> (String, bool) {
    let path = io.path(dest);
    let into = stat(sys, &path).await.is_ok_and(|node| node.is_dir());
    (path, into)
}

/// Whether the terminal's answer to `question` is yes.
pub(crate) async fn yes(io: &Io<'_>, question: String) -> bool {
    (io.ask)(question)
        .await
        .is_some_and(|answer| answer.trim_start().starts_with(['y', 'Y']))
}

pub async fn mv(sys: &impl Sys, io: &mut Io<'_>) -> i32 {
    let mv = parsed!(io, args::Mv);
    let (dest, into) = destination(sys, io, &mv.dest).await;
    if mv.sources.len() > 1 && !into {
        io.fail(format!("mv: target {} is not a directory", quote(&mv.dest)));
        return 1;
    }
    let mut status = 0;
    for typed in &mv.sources {
        if (io.cancelled)() {
            return 130;
        }
        let source = io.path(typed);
        let (target, shown_target) = if into {
            let name = basename(typed);
            (joined(&dest, name), joined(&mv.dest, name))
        } else {
            (dest.clone(), mv.dest.clone())
        };
        if let Err(error) = lstat(sys, &source).await {
            io.fail(format!("mv: cannot stat {}: {error}", quote(typed)));
            status = 1;
            continue;
        }
        let exists = lstat(sys, &target).await.is_ok();
        if exists && mv.no_clobber {
            continue;
        }
        if exists
            && mv.interactive
            && !yes(io, format!("mv: overwrite {}?", quote(&shown_target))).await
        {
            continue;
        }
        let moved = done(
            sys,
            Call::Rename {
                from: source,
                to: target,
                noreplace: false,
            },
        )
        .await;
        match moved {
            Ok(()) if mv.verbose => io.say(format!(
                "renamed {} -> {}",
                quote(typed),
                quote(&shown_target)
            )),
            Ok(()) => {}
            Err(error) if error.errno == Errno::EINVAL => {
                io.fail(format!(
                    "mv: cannot move {} to a subdirectory of itself, {}",
                    quote(typed),
                    quote(&shown_target)
                ));
                status = 1;
            }
            Err(error) => {
                io.fail(format!(
                    "mv: cannot move {} to {}: {error}",
                    quote(typed),
                    quote(&shown_target)
                ));
                status = 1;
            }
        }
    }
    status
}

pub async fn ln(sys: &impl Sys, io: &mut Io<'_>) -> i32 {
    let ln = parsed!(io, args::Ln);
    let (targets, dest) = match ln.operands.as_slice() {
        // ln TARGET: a link of the same name in the working directory.
        [target] => (std::slice::from_ref(target), None),
        [targets @ .., dest] => (targets, Some(dest.clone())),
        [] => {
            io.fail("ln: missing file operand");
            return 1;
        }
    };
    // Into DEST if it is, or leads to, a directory, unless -n or -T name the link itself.
    let into = match &dest {
        None => Some((io.cwd.clone(), ".".to_string())),
        Some(dest) => {
            let path = io.path(dest);
            let dir = !ln.no_dereference && stat(sys, &path).await.is_ok_and(|n| n.is_dir());
            dir.then(|| (path, dest.clone()))
        }
    };
    if into.is_none() && targets.len() > 1 {
        io.fail(format!(
            "ln: target {} is not a directory",
            quote(dest.as_deref().unwrap_or_default())
        ));
        return 1;
    }
    let mut status = 0;
    for target in targets {
        // A directory has one name, as Linux keeps it.
        if !ln.symbolic {
            let source = io.path(target);
            let found = if ln.logical {
                stat(sys, &source).await
            } else {
                lstat(sys, &source).await
            };
            if found.is_ok_and(|node| node.is_dir()) {
                io.fail(format!("ln: {target}: hard link not allowed for directory"));
                status = 1;
                continue;
            }
        }
        let (link, shown_link) = match (&into, &dest) {
            (Some((dir, typed)), _) => {
                let name = basename(target);
                let shown = if dest.is_none() {
                    name.to_string()
                } else {
                    joined(typed, name)
                };
                (joined(dir, name), shown)
            }
            (None, Some(dest)) => (io.path(dest), dest.clone()),
            (None, None) => unreachable!(),
        };
        let make = |at: &str| {
            if ln.symbolic {
                // The target is kept as written, read from where the link is.
                Call::Symlink {
                    target: target.clone(),
                    path: at.to_string(),
                }
            } else {
                Call::Link {
                    from: io.path(target),
                    to: at.to_string(),
                    follow: ln.logical,
                }
            }
        };
        // -f: made under another name, then put in place of what is there, so that one that
        // cannot be made leaves it.
        let made = if ln.force && lstat(sys, &link).await.is_ok() {
            let (dir, name) = link.rsplit_once('/').unwrap_or(("", &link));
            let temporary = format!("{dir}/.{name}.ln~");
            match done(sys, make(&temporary)).await {
                Ok(()) => {
                    let replaced = done(
                        sys,
                        Call::Rename {
                            from: temporary.clone(),
                            to: link.clone(),
                            noreplace: false,
                        },
                    )
                    .await;
                    if replaced.is_err() {
                        let _ = done(sys, Call::Unlink { path: temporary }).await;
                    }
                    replaced
                }
                Err(error) => Err(error),
            }
        } else {
            done(sys, make(&link)).await
        };
        match made {
            Ok(()) if ln.verbose && ln.symbolic => {
                io.say(format!("{} -> {}", quote(&shown_link), quote(target)))
            }
            Ok(()) if ln.verbose => io.say(format!("{} => {}", quote(&shown_link), quote(target))),
            Ok(()) => {}
            Err(error) => {
                io.fail(if ln.symbolic {
                    format!(
                        "ln: failed to create symbolic link {}: {error}",
                        quote(&shown_link)
                    )
                } else {
                    format!(
                        "ln: failed to create hard link {} => {}: {error}",
                        quote(&shown_link),
                        quote(target)
                    )
                });
                status = 1;
            }
        }
    }
    status
}

/// How much of a path must exist, for readlink -f, -e and -m, and realpath.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Existing {
    /// All but its last part (-f, realpath's default).
    Above,
    /// All (-e).
    All,
    /// None (-m).
    None,
}

/// `path` (absolute) with every link followed, `.` and `..` gone, as canonicalize_file_name
/// makes it: a part at a time, each looked up, as `existing` says must be there.
pub(crate) async fn canonical(
    sys: &impl Sys,
    path: &str,
    existing: Existing,
) -> Result<String, Error> {
    let mut todo: std::collections::VecDeque<String> = path
        .split('/')
        .filter(|part| !part.is_empty())
        .map(String::from)
        .collect();
    let mut at: Vec<String> = Vec::new();
    let mut links = 0;
    let mut missing = false;
    while let Some(part) = todo.pop_front() {
        match part.as_str() {
            "." => continue,
            ".." => {
                at.pop();
                continue;
            }
            _ => {}
        }
        at.push(part);
        if missing {
            continue;
        }
        let here = format!("/{}", at.join("/"));
        match lstat(sys, &here).await {
            Ok(node) if node.is_link() => {
                links += 1;
                if links > MAXSYMLINKS {
                    return Err(Errno::ELOOP.into());
                }
                at.pop();
                let target = node.link.unwrap_or_default();
                if target.starts_with('/') {
                    at.clear();
                }
                for part in target.split('/').filter(|part| !part.is_empty()).rev() {
                    todo.push_front(part.to_string());
                }
            }
            Ok(node) if !node.is_dir() && !todo.is_empty() => {
                return Err(Errno::ENOTDIR.into());
            }
            Ok(_) => {}
            Err(error) if error.errno == Errno::ENOENT => match existing {
                Existing::All => return Err(error),
                Existing::Above if !todo.is_empty() => return Err(error),
                _ => missing = true,
            },
            Err(error) => return Err(error),
        }
    }
    Ok(format!("/{}", at.join("/")))
}

/// readlink, and realpath (`real`), which is readlink -f that says why it fails.
pub async fn readlink(sys: &impl Sys, io: &mut Io<'_>, real: bool) -> i32 {
    let (files, existing, no_newline) = if real {
        let realpath = parsed!(io, args::Realpath);
        let existing = if realpath.existing {
            Some(Existing::All)
        } else if realpath.missing {
            Some(Existing::None)
        } else {
            Some(Existing::Above)
        };
        (realpath.files, existing, false)
    } else {
        let readlink = parsed!(io, args::Readlink);
        let existing = if readlink.existing {
            Some(Existing::All)
        } else if readlink.missing {
            Some(Existing::None)
        } else if readlink.canonicalize {
            Some(Existing::Above)
        } else {
            None
        };
        (readlink.files, existing, readlink.no_newline)
    };
    let mut status = 0;
    let mut lines = Vec::new();
    for typed in &files {
        let path = io.path(typed);
        let result = match existing {
            Some(existing) => canonical(sys, &path, existing).await,
            None => call::<String>(sys, Call::Readlink { path }).await,
        };
        match result {
            Ok(line) => lines.push(line),
            Err(error) => {
                // readlink fails without a word, as GNU's does without -v; realpath says why.
                if real {
                    io.fail(format!("realpath: {typed}: {error}"));
                }
                status = 1;
            }
        }
    }
    io.out = lines.join("\n");
    if !(no_newline && files.len() == 1) && !io.out.is_empty() {
        io.out.push('\n');
    }
    status
}

pub async fn touch(sys: &impl Sys, io: &mut Io<'_>) -> i32 {
    let touch = parsed!(io, args::Touch);
    let paths: Vec<String> = touch.files.iter().map(|typed| io.path(typed)).collect();
    // Its times, now; or, not there, a new empty file.
    let touched = calls::<Value>(
        sys,
        paths
            .iter()
            .map(|path| Call::Utimens { path: path.clone() })
            .collect(),
    )
    .await;
    let mut status = 0;
    let mut making = Vec::new();
    for ((typed, path), result) in touch.files.iter().zip(&paths).zip(touched) {
        match result {
            Ok(_) => {}
            Err(error) if error.errno == Errno::ENOENT && touch.no_create => {}
            Err(error) if error.errno == Errno::ENOENT => making.push((typed, path)),
            Err(error) => {
                io.fail(format!("touch: cannot touch {}: {error}", quote(typed)));
                status = 1;
            }
        }
    }
    let made = calls::<Value>(
        sys,
        making
            .iter()
            .map(|(_, path)| Call::Mknod {
                path: (*path).clone(),
                mode: 0o666,
            })
            .collect(),
    )
    .await;
    for ((typed, _), result) in making.iter().zip(made) {
        if let Err(error) = result {
            io.fail(format!("touch: cannot touch {}: {error}", quote(typed)));
            status = 1;
        }
    }
    status
}
