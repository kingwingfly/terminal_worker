//! Permissions: chmod, chown, chgrp, getfacl, setfacl, getfattr and setfattr.

use terminal_worker_core::acl::{self, Entries, Spec};
use terminal_worker_core::cli::{parse, permissions as args};
use terminal_worker_core::permissions::*;
use terminal_worker_core::xattr;

use super::{Context, ServerCommand, group};

/// Change `paths` with `change`, each on its own, as chmod and chown do: a link named is followed
/// unless `itself`.
fn update(
    ctx: &mut Context,
    paths: &[String],
    itself: bool,
    change: impl Fn(&mut Context, &str) -> Result<(), String>,
) -> String {
    let mut updated = Vec::new();
    for value in paths {
        let path = if itself {
            ctx.entry(value)
        } else {
            ctx.path(value)
        };
        let result = path.and_then(|path| {
            change(ctx, &path)?;
            Ok(path)
        });
        if let Some(path) = ctx.each(result) {
            updated.push(format!("Updated {path}"));
        }
    }
    updated.join("\n")
}

pub struct Chmod;
impl ServerCommand for Chmod {
    fn name(&self) -> &'static str {
        "chmod"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let chmod = parse::<args::Chmod>(ctx.args)?;
        // chmod has no mode for a link, and goes to what it points to.
        Ok(update(ctx, &chmod.paths, false, |ctx, path| {
            ctx.db.chmod(ctx.who, path, &chmod.mode, chmod.recursive)
        }))
    }
}

pub struct Chown;
impl ServerCommand for Chown {
    fn name(&self) -> &'static str {
        "chown"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let chown = parse::<args::Chown>(ctx.args)?;
        let (user, grp) = chown
            .owner
            .split_once(':')
            .map(|(u, g)| (u, Some(g)))
            .unwrap_or((&chown.owner, None));
        let owner = if user.is_empty() {
            None
        } else {
            Some(ctx.db.user(user).ok_or("User does not exist")?.id)
        };
        // USER: also gives USER's group.
        let grp = match grp {
            Some("") => owner
                .and_then(|id| ctx.db.user_by_id(id))
                .map(|(_, u)| u.group),
            Some(g) => Some(group(ctx.db, g)?),
            None => None,
        };
        if owner.is_none() && grp.is_none() {
            return Err("Specify a user or group".into());
        }
        Ok(update(
            ctx,
            &chown.paths,
            chown.no_dereference,
            |ctx, path| ctx.db.chown(ctx.who, path, owner, grp, chown.recursive),
        ))
    }
}

pub struct Chgrp;
impl ServerCommand for Chgrp {
    fn name(&self) -> &'static str {
        "chgrp"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let chgrp = parse::<args::Chgrp>(ctx.args)?;
        let gid = group(ctx.db, &chgrp.group)?;
        Ok(update(
            ctx,
            &chgrp.paths,
            chgrp.no_dereference,
            |ctx, path| {
                ctx.db
                    .chown(ctx.who, path, None, Some(gid), chgrp.recursive)
            },
        ))
    }
}

/// `path`'s node, and with `recursive`, every one below it that `who` may list; links left out
/// unless `links` (getfattr's, as root may give a link trusted.* attributes; ACLs have none).
fn nodes(
    ctx: &Context,
    path: &str,
    recursive: bool,
    links: bool,
) -> Result<Vec<(String, Node)>, String> {
    let node = ctx.db.access(ctx.who, path, 0)?;
    if !recursive || !node.directory {
        return Ok(vec![(path.to_string(), node)]);
    }
    let (found, _) = ctx.db.walk(ctx.who, path, None)?;
    Ok(std::iter::once((path.to_string(), node))
        .chain(found)
        .filter(|(_, n)| links || !n.is_link())
        .collect())
}

pub struct Getfacl;
impl ServerCommand for Getfacl {
    fn name(&self) -> &'static str {
        "getfacl"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let getfacl = parse::<args::Getfacl>(ctx.args)?;
        let mut blocks = Vec::new();
        for value in &getfacl.files {
            let start = if getfacl.physical {
                ctx.entry(value)
            } else {
                ctx.path(value)
            };
            let found = start
                .and_then(|path| nodes(ctx, &path, getfacl.recursive, false))
                .map_err(|e| format!("getfacl: {value}: {e}"));
            let Some(found) = ctx.each(found) else {
                continue;
            };
            let db = &*ctx.db;
            let user = |id: u32| {
                if getfacl.numeric {
                    id.to_string()
                } else {
                    db.username(id)
                }
            };
            let group = |id: u32| {
                if getfacl.numeric {
                    id.to_string()
                } else {
                    db.groupname(id)
                }
            };
            for (path, node) in found {
                let mut lines = Vec::new();
                if !getfacl.omit_header {
                    let shown = if getfacl.absolute {
                        path.clone()
                    } else {
                        path.trim_start_matches('/').to_string()
                    };
                    lines.push(format!(
                        "# file: {}",
                        if shown.is_empty() { "/" } else { &shown }
                    ));
                    lines.push(format!("# owner: {}", user(node.owner)));
                    lines.push(format!("# group: {}", group(node.group)));
                    let special = node.mode & 0o7000;
                    if special != 0 {
                        let flag = |bit, c| if special & bit != 0 { c } else { '-' };
                        lines.push(format!(
                            "# flags: {}{}{}",
                            flag(SETUID, 's'),
                            flag(SETGID, 's'),
                            flag(STICKY, 't')
                        ));
                    }
                }
                if !getfacl.default {
                    lines.extend(acl::lines(&node.entries(), false, &user, &group));
                }
                if !getfacl.access
                    && let Some(default) = &node.default_acl
                {
                    lines.extend(acl::lines(default, true, &user, &group));
                }
                blocks.push(lines.join("\n") + "\n");
            }
        }
        Ok(blocks.join("\n"))
    }
}

/// The entries of `specs` for the default ACL, if `default`, or for the access ACL.
fn sided(specs: &[Spec], default: bool) -> Vec<&Spec> {
    specs.iter().filter(|s| s.default == default).collect()
}

pub struct Setfacl;
impl ServerCommand for Setfacl {
    fn name(&self) -> &'static str {
        "setfacl"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let setfacl = parse::<args::Setfacl>(ctx.args)?;
        if setfacl.modify_file.is_some() || setfacl.remove_file.is_some() {
            return Err("setfacl: -M and -X read files of entries, which this terminal does not; give them with -m and -x".into());
        }
        let parse_specs = |text: &Option<String>, perms| -> Result<Vec<Spec>, String> {
            let Some(text) = text else {
                return Ok(Vec::new());
            };
            let user = |name: &str| ctx.db.user(name).map(|u| u.id);
            let group = |name: &str| ctx.db.group(name);
            let mut specs = acl::parse_specs(text, perms, &user, &group)
                .map_err(|e| format!("setfacl: {e}"))?;
            // -d makes every entry a default one.
            if setfacl.default {
                for spec in &mut specs {
                    spec.default = true;
                }
            }
            Ok(specs)
        };
        let modify = parse_specs(&setfacl.modify, true)?;
        let remove = parse_specs(&setfacl.remove, false)?;
        let set = parse_specs(&setfacl.set, true)?;
        let touches_default = |default| {
            !sided(&set, default).is_empty()
                || !sided(&modify, default).is_empty()
                || !sided(&remove, default).is_empty()
        };
        let defaults = touches_default(true);
        let mut updated = Vec::new();
        for value in &setfacl.files {
            let result = ctx.path(value).and_then(|path| {
                let mut changed = Vec::new();
                for (p, mut n) in nodes(ctx, &path, setfacl.recursive, false)? {
                    if ctx.who.id != 0 && ctx.who.id != n.owner {
                        return Err(format!("setfacl: {p}: Operation not permitted"));
                    }
                    let executable = n.directory || n.mode & 0o111 != 0;
                    if setfacl.remove_all {
                        let mut entries = n.entries();
                        entries.users.clear();
                        entries.groups.clear();
                        entries.mask = None;
                        n.set_entries(&entries);
                        n.default_acl = None;
                    }
                    if setfacl.remove_default {
                        n.default_acl = None;
                    }
                    // A default ACL is a directory's; -R passes over files for one.
                    if defaults && !n.directory {
                        if setfacl.recursive && p != path {
                            continue;
                        }
                        return Err(format!(
                            "setfacl: {p}: Only directories can have default ACLs"
                        ));
                    }
                    let had_default = n.default_acl.is_some();
                    let mut access = n.entries();
                    // A default ACL begins as the access ACL's base entries.
                    let mut default = n.default_acl.clone().unwrap_or_else(|| Entries {
                        user: access.user,
                        group: access.group,
                        other: access.other,
                        ..Entries::default()
                    });
                    for (target, side) in [(&mut access, false), (&mut default, true)] {
                        let replacing = sided(&set, side);
                        if !replacing.is_empty() {
                            *target = acl::set(&replacing, executable)
                                .map_err(|e| format!("setfacl: {p}: {e}"))?;
                        }
                        acl::modify(target, &sided(&modify, side), executable, setfacl.no_mask);
                        let removing = sided(&remove, side);
                        if !removing.is_empty() {
                            acl::remove(target, &removing, setfacl.no_mask)
                                .map_err(|e| format!("setfacl: {p}: {e}"))?;
                        }
                    }
                    n.set_entries(&access);
                    if n.directory && (had_default || defaults) {
                        n.default_acl = Some(default);
                    }
                    if setfacl.remove_default || setfacl.remove_all && !defaults {
                        n.default_acl = None;
                    }
                    changed.push((p, n));
                }
                for (p, n) in changed {
                    ctx.db.set_node(&p, &n);
                }
                Ok(path)
            });
            if let Some(path) = ctx.each(result) {
                updated.push(format!("Updated the ACL of {path}"));
            }
        }
        Ok(updated.join("\n"))
    }
}

pub struct Getfattr;
impl ServerCommand for Getfattr {
    fn name(&self) -> &'static str {
        "getfattr"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let getfattr = parse::<args::Getfattr>(ctx.args)?;
        // -m - and -m . match everything; the default, user. only.
        let matcher = match getfattr.pattern.as_deref().unwrap_or("^user\\.") {
            "-" | "." | "" => None,
            p => Some(regex::Regex::new(p).map_err(|e| format!("getfattr: bad pattern: {e}"))?),
        };
        let encoding = getfattr.encoding.as_deref().unwrap_or("text");
        let mut out = Vec::new();
        for value in &getfattr.files {
            let start = if getfattr.no_dereference {
                ctx.entry(value)
            } else {
                ctx.path(value)
            };
            let result = start.and_then(|path| {
                for (p, n) in nodes(ctx, &path, getfattr.recursive, true)? {
                    // Reading a file's attributes needs read permission on it.
                    if !ctx.who.allows(&n, READ) && !n.is_link() {
                        return Err(format!("{p}: Permission denied"));
                    }
                    let attrs = ctx.db.xattrs(n.ino);
                    let user_or_root = |name: &str| {
                        ctx.who.id == 0
                            || xattr::namespace(name).is_ok_and(|s| s == xattr::Namespace::User)
                    };
                    let shown: Vec<(&String, &Vec<u8>)> = match &getfattr.name {
                        // trusted.* is root's: to others it is not there (ENODATA), as on Linux.
                        Some(name) => {
                            match attrs.get_key_value(name).filter(|_| user_or_root(name)) {
                                Some(pair) => vec![pair],
                                None => return Err(format!("{name}: No such attribute")),
                            }
                        }
                        None => attrs
                            .iter()
                            .filter(|(name, _)| {
                                xattr::namespace(name).is_ok()
                                    && user_or_root(name)
                                    && matcher.as_ref().is_none_or(|m| m.is_match(name))
                            })
                            .collect(),
                    };
                    if shown.is_empty() {
                        continue;
                    }
                    let file = if getfattr.absolute {
                        p.clone()
                    } else {
                        p.trim_start_matches('/').to_string()
                    };
                    let mut block = vec![format!("# file: {file}")];
                    for (name, value) in shown {
                        block.push(if getfattr.dump || getfattr.name.is_some() {
                            format!("{name}={}", xattr::encode(value, encoding))
                        } else {
                            name.clone()
                        });
                    }
                    out.push(block.join("\n") + "\n");
                }
                Ok(())
            });
            ctx.each(result.map_err(|e| format!("getfattr: {value}: {e}")));
        }
        Ok(out.join("\n"))
    }
}

pub struct Setfattr;
impl ServerCommand for Setfattr {
    fn name(&self) -> &'static str {
        "setfattr"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let setfattr = parse::<args::Setfattr>(ctx.args)?;
        let (name, value) = match (&setfattr.name, &setfattr.remove) {
            (Some(name), _) => (
                name,
                Some(xattr::decode(setfattr.value.as_deref().unwrap_or(""))?),
            ),
            (None, Some(name)) => (name, None),
            (None, None) => unreachable!("clap requires -n or -x"),
        };
        let space = xattr::namespace(name)?;
        for path in &setfattr.files {
            let start = if setfattr.no_dereference {
                ctx.entry(path)
            } else {
                ctx.path(path)
            };
            let result = start.and_then(|path| set_one(ctx, &path, name, space, value.clone()));
            ctx.each(result.map_err(|e| format!("setfattr: {path}: {e}")));
        }
        Ok(String::new())
    }
}

/// Set or, without a value, remove the attribute `name` of `path`: user.* needs w on it, and in a
/// sticky directory being its owner; trusted.* root. A link has none.
fn set_one(
    ctx: &mut Context,
    path: &str,
    name: &str,
    space: xattr::Namespace,
    value: Option<Vec<u8>>,
) -> Result<(), String> {
    let node = ctx.db.access(ctx.who, path, 0)?;
    let who = ctx.who;
    match space {
        xattr::Namespace::Trusted if who.id != 0 => {
            return Err(format!("{name}: Operation not permitted"));
        }
        xattr::Namespace::User if node.is_link() => {
            return Err(format!("{name}: Operation not permitted"));
        }
        xattr::Namespace::User => {
            if !who.allows(&node, WRITE) {
                return Err("Permission denied".into());
            }
            if node.directory && node.mode & STICKY != 0 && who.id != 0 && who.id != node.owner {
                return Err(format!("{name}: Operation not permitted"));
            }
        }
        xattr::Namespace::Trusted => {}
    }
    let mut attrs = ctx.db.xattrs(node.ino);
    match value {
        Some(value) => {
            if value.len() > xattr::VALUE_MAX {
                return Err(format!("{name}: Argument list too long"));
            }
            attrs.insert(name.to_string(), value);
            let total: usize = attrs.iter().map(|(n, v)| n.len() + v.len()).sum();
            if total > xattr::TOTAL_MAX {
                return Err(format!("{name}: No space left on device"));
            }
        }
        None => {
            if attrs.remove(name).is_none() {
                return Err(format!("{name}: No such attribute"));
            }
        }
    }
    ctx.db.set_xattrs(node.ino, &attrs);
    Ok(())
}
