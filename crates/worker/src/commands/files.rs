//! Files: touch, ls, mkdir, rm, mv, cp, ln, readlink and fd.

use serde_json::json;
use std::collections::BTreeSet;
use terminal_worker_core::cli::{files as args, parse};
use terminal_worker_core::permissions::*;
use terminal_worker_core::{find, parent};

use super::{Context, ServerCommand, entries, entry, is_permission_error, joined, long_line};

/// The most results fd returns.
const FD_MAX: usize = 50_000;

pub struct Touch;
impl ServerCommand for Touch {
    fn name(&self) -> &'static str {
        "touch"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let touch = parse::<args::Touch>(ctx.args)?;
        // Validate every path before creating any file; an empty file has no chunks.
        let mut paths = BTreeSet::new();
        for value in &touch.files {
            let path = ctx.path(value)?;
            ctx.db.traverse(ctx.who, &path)?;
            if let Some(node) = ctx.db.node(&path) {
                if node.directory {
                    return Err("This is a directory; touch only supports files".into());
                }
                ctx.db.access(ctx.who, &path, WRITE)?;
            } else if !touch.no_create {
                ctx.db.writable_parent(ctx.who, &path)?;
                paths.insert(path);
            }
        }
        for path in &paths {
            let file = ctx
                .db
                .create_file(0, 1, None, ctx.who.id, path, None, crate::auth::now());
            ctx.db.finish_file(&file);
            let above = ctx
                .db
                .node(parent(path))
                .ok_or("Parent directory does not exist")?;
            let mut node = created(ctx.who, &above, 0o666, false, UMASK);
            node.file = Some(file);
            ctx.db.set_node(path, &node);
        }
        Ok(if paths.is_empty() {
            "File already exists or was skipped; content unchanged".to_string()
        } else {
            format!(
                "Created {}",
                paths.into_iter().collect::<Vec<_>>().join(", ")
            )
        })
    }
}

pub struct Ls;
impl ServerCommand for Ls {
    fn name(&self) -> &'static str {
        "ls"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let ls = parse::<args::Ls>(ctx.args)?;
        let typed = ls.path.as_deref().unwrap_or(".");
        // A link to a directory lists the directory, but with -l or -d, itself, as GNU ls shows
        // a link named on the command line; a trailing / follows it.
        let follow = !(ls.long || ls.directory) || typed.ends_with('/');
        let path = if follow {
            ctx.path(typed)?
        } else {
            ctx.entry(typed)?
        };
        let shown = ctx.typed(typed)?;
        let db = &*ctx.db;
        let node = db.access(ctx.who, &path, 0)?;
        if node.directory && !ls.directory {
            db.access(ctx.who, &path, READ | EXEC)?;
            let children = db.children(&path);
            ctx.listing = Some((entries(db, ctx.who, &path, &shown, &children), ls.long));
            return Ok(children
                .iter()
                .map(|(p, n)| {
                    if ls.long {
                        long_line(db, p, n)
                    } else {
                        let name = p.rsplit('/').next().unwrap_or_default();
                        format!("{name}{}", if n.directory { "/" } else { "" })
                    }
                })
                .collect::<Vec<_>>()
                .join(if ls.long { "\n" } else { "  " }));
        }
        ctx.listing = Some((vec![entry(db, ctx.who, &shown, &path, &node)], ls.long));
        Ok(if ls.long {
            long_line(db, &path, &node)
        } else {
            typed.to_string()
        })
    }
}

pub struct Mkdir;
impl ServerCommand for Mkdir {
    fn name(&self) -> &'static str {
        "mkdir"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let mkdir = parse::<args::Mkdir>(ctx.args)?;
        let mut made = Vec::new();
        for value in &mkdir.dirs {
            let result = ctx.entry(value).and_then(|path| {
                ctx.db.mkdir(ctx.who, &path, mkdir.parents)?;
                Ok(path)
            });
            if let Some(path) = ctx.each(result) {
                made.push(format!("Created {path}"));
            }
        }
        Ok(made.join("\n"))
    }
}

pub struct Rm;
impl ServerCommand for Rm {
    fn name(&self) -> &'static str {
        "rm"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let rm = parse::<args::Rm>(ctx.args)?;
        let mut removed = Vec::new();
        for value in &rm.paths {
            if value.is_empty() {
                ctx.errors
                    .push("rm: cannot remove '': No such file or directory".into());
                continue;
            }
            let result = ctx
                .entry(value)
                .and_then(|path| remove_one(ctx, &path, &rm).map(|r| (path, r)));
            if let Some((path, true)) = ctx.each(result) {
                removed.push(format!("Removed {path}"));
            }
        }
        Ok(removed.join("\n"))
    }
}

/// Remove `path` as rm does, unless it is missing and `-f` was given; whether it was removed.
fn remove_one(ctx: &mut Context, path: &str, rm: &args::Rm) -> Result<bool, String> {
    if path == "/" {
        return Err("Cannot remove the root directory".into());
    }
    if ctx.here == path || ctx.here.starts_with(&format!("{path}/")) {
        return Err(
            "Cannot remove the current directory or its ancestors; cd elsewhere first".into(),
        );
    }
    // A missing path is no error with -f, but only where `who` may look: below a directory it
    // cannot search, rm -f fails whether the path exists or not.
    if rm.force {
        match ctx.db.traverse(ctx.who, path) {
            Ok(()) if ctx.db.node(path).is_none() => return Ok(false),
            // A directory on the way is missing, below ones `who` may search.
            Err(e) if !is_permission_error(&e) => return Ok(false),
            _ => {}
        }
    }
    ctx.db.remove(ctx.who, path, rm.recursive)?;
    Ok(true)
}

/// Into `dest` if it is, or leads to, a directory: the directory and true; otherwise `dest` as
/// a name, a link there being replaced or written through. Only where `who` may look is it told
/// what is there.
fn destination(ctx: &Context, dest: &str) -> Result<(String, bool), String> {
    let followed = ctx.path(dest)?;
    ctx.db.traverse(ctx.who, &followed)?;
    if ctx.db.node(&followed).is_some_and(|n| n.directory) {
        return Ok((followed, true));
    }
    Ok((ctx.entry(dest)?, false))
}

pub struct Mv;
impl ServerCommand for Mv {
    fn name(&self) -> &'static str {
        "mv"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let mv = parse::<args::Mv>(ctx.args)?;
        // -i replaces nothing here, and names each file that exists, as [SOURCE, TARGET], for
        // the terminal to ask about; -n asks about none.
        let replace = !mv.no_clobber && !mv.interactive;
        let (dest, into) = destination(ctx, &mv.dest)?;
        if mv.sources.len() > 1 && !into {
            return Err(format!("mv: target '{dest}' is not a directory"));
        }
        let mut done = Vec::new();
        for value in &mv.sources {
            let result = ctx.entry(value).and_then(|src| {
                let target = if into {
                    joined(&dest, src.rsplit('/').next().unwrap_or_default())
                } else {
                    dest.clone()
                };
                if !move_one(ctx, &src, &target, replace)? {
                    if mv.interactive {
                        ctx.existing.push(json!([src, target]));
                    }
                    return Ok(None);
                }
                Ok(Some(format!("renamed '{src}' -> '{target}'")))
            });
            if let Some(Some(line)) = ctx.each(result).filter(|_| mv.verbose) {
                done.push(line);
            }
        }
        Ok(done.join("\n"))
    }
}

/// Move `src` to `dst`, unless that would move the working directory or an account's home.
fn move_one(ctx: &mut Context, src: &str, dst: &str, replace: bool) -> Result<bool, String> {
    if ctx.here == src || ctx.here.starts_with(&format!("{src}/")) {
        return Err(
            "Cannot move the current directory or its ancestors; cd elsewhere first".into(),
        );
    }
    for (name, user) in ctx.db.users() {
        if user.home == src || user.home.starts_with(&format!("{src}/")) {
            return Err(format!(
                "'{src}' holds the home of {name}; move homes with usermod -d HOME -m {name}"
            ));
        }
    }
    ctx.db.rename(ctx.who, src, dst, replace)
}

pub struct Cp;
impl ServerCommand for Cp {
    fn name(&self) -> &'static str {
        "cp"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let cp = parse::<args::Cp>(ctx.args)?;
        let options = CopyOptions {
            recursive: cp.archive || cp.recursive,
            replace: !cp.no_clobber && !cp.interactive,
            force: cp.force,
            dereference: cp.dereference,
            links: cp.archive || cp.links,
            preserve: cp.archive || cp.preserve,
            xattrs: cp.archive,
        };
        // A link named on the command line is followed without -r, and with -L or -H; otherwise
        // copied as one (-P, also in -d and -a).
        let follow = cp.dereference
            || cp.command_line
            || !cp.no_dereference && !options.recursive && !options.links;
        let (dest, into) = destination(ctx, &cp.dest)?;
        if cp.sources.len() > 1 && !into {
            return Err(format!("cp: target '{dest}' is not a directory"));
        }
        let mut done = Vec::new();
        for value in &cp.sources {
            let src = if follow {
                ctx.path(value)
            } else {
                ctx.entry(value)
            };
            let result = src.and_then(|src| {
                let target = if into {
                    joined(&dest, src.rsplit('/').next().unwrap_or_default())
                } else {
                    dest.clone()
                };
                // Files still uploading are copied once whole: Store::copy refuses them.
                let skipped = ctx.db.copy(ctx.who, &src, &target, &options)?;
                let whole = skipped.contains(&target);
                if cp.interactive {
                    // What a directory holds is at the same place below the source.
                    ctx.existing.extend(
                        skipped
                            .into_iter()
                            .map(|t| json!([format!("{src}{}", &t[target.len()..]), t])),
                    );
                }
                Ok((!whole).then(|| format!("'{src}' -> '{target}'")))
            });
            if let Some(Some(line)) = ctx.each(result).filter(|_| cp.verbose) {
                done.push(line);
            }
        }
        Ok(done.join("\n"))
    }
}

pub struct Ln;
impl ServerCommand for Ln {
    fn name(&self) -> &'static str {
        "ln"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let ln = parse::<args::Ln>(ctx.args)?;
        let (targets, dest) = match ln.operands.as_slice() {
            // ln TARGET: a link of the same name in the working directory.
            [target] => (std::slice::from_ref(target), None),
            [targets @ .., dest] => (targets, Some(dest.as_str())),
            [] => return Err("ln: missing file operand; ln -h shows how to use it".into()),
        };
        // Into DEST if it is, or leads to, a directory, unless -n or -T take it as the name of
        // the link itself.
        let (dir, named) = match dest {
            None => (Some(ctx.here.clone()), None),
            Some(dest) => {
                let followed = ctx.path(dest)?;
                if !ln.no_dereference
                    && ctx.db.traverse(ctx.who, &followed).is_ok()
                    && ctx.db.node(&followed).is_some_and(|n| n.directory)
                {
                    (Some(followed), None)
                } else {
                    (None, Some(ctx.entry(dest)?))
                }
            }
        };
        if dir.is_none() && targets.len() > 1 {
            return Err(format!(
                "ln: target '{}' is not a directory",
                named.unwrap_or_default()
            ));
        }
        let mut made = Vec::new();
        for target in targets {
            let link = match (&dir, &named) {
                (Some(dir), _) => joined(
                    dir,
                    target
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or_default(),
                ),
                (_, Some(named)) => named.clone(),
                _ => unreachable!(),
            };
            // With -f, what is at the link's name gives way only once the link is known to be
            // possible, so that one that cannot be made leaves what was there.
            let result = (|| -> Result<String, String> {
                if ln.symbolic {
                    // The target is kept as typed, read from where the link is.
                    ctx.db.symlink(ctx.who, target, &link, ln.force)?;
                    return Ok(format!("'{link}' -> '{target}'"));
                }
                let src = if ln.logical {
                    ctx.path(target)?
                } else {
                    ctx.entry(target)?
                };
                ctx.db.link(ctx.who, &src, &link, ln.force)?;
                Ok(format!("'{link}' => '{src}'"))
            })()
            .map_err(|e| format!("ln: {e}"));
            if let Some(line) = ctx.each(result).filter(|_| ln.verbose) {
                made.push(line);
            }
        }
        Ok(made.join("\n"))
    }
}

pub struct Readlink;
impl ServerCommand for Readlink {
    fn name(&self) -> &'static str {
        "readlink"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let readlink = parse::<args::Readlink>(ctx.args)?;
        let mut lines = Vec::new();
        for value in &readlink.files {
            let result = if readlink.canonicalize || readlink.existing || readlink.missing {
                // -f: every part but the last must exist; -e: all; -m: none.
                ctx.path(value).and_then(|real| {
                    let missing =
                        ctx.db.traverse(ctx.who, &real).is_err() || ctx.db.node(&real).is_none();
                    let above = ctx.db.node(parent(&real)).is_some_and(|n| n.directory);
                    if missing && (readlink.existing || readlink.canonicalize && !above) {
                        return Err(format!("{value}: No such file or directory"));
                    }
                    Ok(real)
                })
            } else {
                ctx.entry(value).and_then(|path| {
                    ctx.db.traverse(ctx.who, &path)?;
                    ctx.db
                        .node(&path)
                        .and_then(|n| n.link)
                        .ok_or_else(|| format!("{value}: not a symbolic link"))
                })
            };
            if let Some(line) = ctx.each(result.map_err(|e| format!("readlink: {e}"))) {
                lines.push(line);
            }
        }
        // -n leaves out the trailing newline, which server output never has.
        Ok(lines.join("\n"))
    }
}

pub struct Fd;
impl ServerCommand for Fd {
    fn name(&self) -> &'static str {
        "fd"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let find = find::parse(ctx.args)?;
        if find.exec.is_some() {
            return Err("fd -x and -X run in the terminal, not on the server".into());
        }
        let roots = if find.roots.is_empty() {
            vec![(None, ctx.cwd.clone())]
        } else {
            find.roots
                .iter()
                .map(|r| Ok((Some(r.as_str()), ctx.path(r)?)))
                .collect::<Result<Vec<_>, String>>()?
        };
        let mut found = Vec::new();
        let mut hidden = 0;
        for (typed, root) in roots {
            match ctx.db.walk(ctx.who, &root, find.max_depth) {
                Ok((nodes, h)) => {
                    hidden += h;
                    found.extend(
                        nodes
                            .into_iter()
                            .filter(|(p, n)| {
                                find.matches(&root, p, n.directory, n.is_link(), n.mode)
                            })
                            .map(|(p, _)| find.display(typed, &root, &ctx.cwd, &p)),
                    );
                }
                Err(e) => ctx
                    .errors
                    .push(format!("fd: {}: {e}", typed.unwrap_or("."))),
            }
            if found.len() > FD_MAX {
                return Err(format!("fd: more than {FD_MAX} results; narrow the search"));
            }
        }
        if hidden > 0 {
            ctx.errors.push(format!(
                "fd: {hidden} director{} could not be read, so {} not searched",
                if hidden == 1 { "y" } else { "ies" },
                if hidden == 1 { "it was" } else { "they were" }
            ));
        }
        let separator = if find.null { '\0' } else { '\n' };
        let text = found.iter().map(|p| format!("{p}{separator}")).collect();
        ctx.results = Some(found);
        Ok(text)
    }
}
