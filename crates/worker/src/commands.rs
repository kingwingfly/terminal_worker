use crate::db::Db;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use terminal_worker_core::{Entry, kind, parent, permissions::*, resolve};
#[derive(Deserialize)]
pub struct Command {
    pub args: Vec<String>,
    #[serde(default = "root_path")]
    pub cwd: String,
    #[serde(default)]
    pub sudo: bool,
    /// The home `~` stands for: the signed-in account's, under sudo too, as a shell expands `~`
    /// before sudo runs; if not set, the acting account's.
    #[serde(skip)]
    pub home: Option<String>,
}
fn root_path() -> String {
    "/".into()
}
struct Options {
    flags: BTreeSet<String>,
    values: BTreeMap<String, String>,
    args: Vec<String>,
}
fn options(
    args: &[String],
    flags: &[&str],
    values: &[&str],
) -> std::result::Result<Options, String> {
    let mut out = Options {
        flags: BTreeSet::new(),
        values: BTreeMap::new(),
        args: Vec::new(),
    };
    let mut i = 0;
    let mut end = false;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if !end && arg == "--" {
            end = true;
            continue;
        }
        if !end && arg.starts_with('-') && arg != "-" {
            // Support the familiar -aG group shorthand alongside separate flags.
            if arg == "-aG" && flags.contains(&"-a") && values.contains(&"-G") {
                out.flags.insert("-a".into());
                let value = args.get(i).ok_or("-G requires a group list")?;
                out.values.insert("-G".into(), value.clone());
                i += 1;
            } else if flags.contains(&arg.as_str()) {
                if !out.flags.insert(arg.clone()) {
                    return Err("Duplicate command option".into());
                }
            } else if arg.len() > 2
                && arg[1..]
                    .chars()
                    .all(|f| flags.contains(&format!("-{f}").as_str()))
            {
                // Combined short flags, as in cp -rv.
                for f in arg[1..].chars() {
                    out.flags.insert(format!("-{f}"));
                }
            } else if values.contains(&arg.as_str()) {
                let value = args.get(i).ok_or("Command option requires an argument")?;
                if out.values.insert(arg.clone(), value.clone()).is_some() {
                    return Err("Duplicate command option".into());
                }
                i += 1;
            } else {
                return Err(format!(
                    "Unsupported option {arg}; enter help for supported commands"
                ));
            }
        } else {
            out.args.push(arg.clone());
        }
    }
    Ok(out)
}
fn one(o: &Options) -> std::result::Result<&str, String> {
    if o.args.len() != 1 {
        return Err("Exactly one name or path is required".into());
    }
    Ok(&o.args[0])
}
fn group(c: &Db, name: &str) -> std::result::Result<u32, String> {
    c.group(name)
        .or_else(|| name.parse().ok().filter(|id| c.group_name(*id).is_some()))
        .ok_or_else(|| format!("Group does not exist: {name}"))
}
/// The group named `name`, as groupmod and groupdel take one: by its name only, as on Linux, so
/// that what they refuse to change by name, they refuse by number too.
fn named_group(c: &Db, name: &str) -> std::result::Result<u32, String> {
    c.group(name)
        .ok_or_else(|| format!("Group does not exist: {name}"))
}
fn groups(c: &Db, value: &str) -> std::result::Result<BTreeSet<u32>, String> {
    if value.is_empty() {
        return Ok(BTreeSet::new());
    }
    value.split(',').map(|n| group(c, n)).collect()
}
fn absolute_home(path: &str) -> std::result::Result<String, String> {
    if !path.starts_with('/') {
        return Err("Home must be an absolute path".into());
    }
    let path = resolve("/", path)?;
    if path == "/" {
        return Err("Home cannot be the root directory".into());
    }
    Ok(path)
}
fn protect_home(c: &Db, path: &str, except: u32) -> std::result::Result<(), String> {
    for (_, u) in c.users() {
        // The homes of the accounts guests make are in guest's.
        let a_guests = u.id == GUEST && path.starts_with(&format!("{}/", u.home));
        if u.id != except
            && !a_guests
            && (u.home == path
                || u.home.starts_with(&format!("{path}/"))
                || path.starts_with(&format!("{}/", u.home)))
        {
            return Err("Home must not overlap another account's home directory".into());
        }
    }
    Ok(())
}
fn show_id(c: &Db, who: &Identity) -> String {
    format!(
        "uid={}({}) gid={}({}) groups={}",
        who.id,
        c.username(who.id),
        who.group,
        c.groupname(who.group),
        who.groups
            .iter()
            .map(|g| format!("{g}({})", c.groupname(*g)))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// What `who` sees of `n`, named `path`, shown at `shown`: the path as it was asked for, through
/// any link on the way.
pub fn entry(c: &Db, who: &Identity, shown: &str, path: &str, n: &Node) -> Entry {
    let uploaded = n
        .file
        .as_deref()
        .and_then(|id| c.file(id))
        .filter(|f| !f.complete())
        .map(|f| c.readable(&f));
    let mut entry = described(
        c,
        who,
        path,
        n,
        uploaded,
        c.username(n.owner),
        c.groupname(n.group),
    );
    entry.path = shown.into();
    entry
}
/// The entries of `children`, the nodes in directory `dir`, directories first, then by name,
/// with their paths in `shown`, the directory as it was asked for. Each owner, group and
/// unfinished upload is looked up once, not once per entry.
pub fn entries(
    c: &Db,
    who: &Identity,
    dir: &str,
    shown: &str,
    children: &[(String, Node)],
) -> Vec<Entry> {
    let uploading: BTreeMap<String, u64> = c
        .uploading_in(dir)
        .into_iter()
        .map(|f| (f.id.clone(), c.readable(&f)))
        .collect();
    let mut owners = BTreeMap::new();
    let mut groups = BTreeMap::new();
    let mut entries: Vec<Entry> = children
        .iter()
        .map(|(path, n)| {
            let owner = owners.entry(n.owner).or_insert_with(|| c.username(n.owner));
            let group = groups
                .entry(n.group)
                .or_insert_with(|| c.groupname(n.group));
            let uploaded = n.file.as_ref().and_then(|id| uploading.get(id).copied());
            let mut entry = described(c, who, path, n, uploaded, owner.clone(), group.clone());
            entry.path = joined(shown, &entry.name);
            entry
        })
        .collect();
    entries.sort_by(|a, b| {
        b.directory
            .cmp(&a.directory)
            .then_with(|| a.name.cmp(&b.name))
    });
    entries
}
/// `name` in the directory `dir`.
pub fn joined(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}
fn described(
    c: &Db,
    who: &Identity,
    path: &str,
    n: &Node,
    uploaded: Option<u64>,
    owner: String,
    group: String,
) -> Entry {
    // A link to a directory opens as one, if `who` may get there.
    let directory = n.directory
        || n.is_link()
            && c.physical(who, path, true)
                .ok()
                .and_then(|real| c.node(&real))
                .is_some_and(|target| target.directory);
    Entry {
        uploaded,
        name: path.rsplit('/').next().unwrap_or("").into(),
        path: path.into(),
        directory,
        size: n.size,
        kind: if directory { "directory" } else { kind(path) }.into(),
        owner,
        group,
        mode: n.mode,
        link: n.link.clone(),
        links: n.links,
        acl: n.acl.is_some(),
    }
}
/// ls -l's line for `n`, named `path`: its type and mode, `+` for an ACL, names, owner, group,
/// size and name, and where a link points.
fn long_line(c: &Db, path: &str, n: &Node) -> String {
    let kind = if n.is_link() {
        'l'
    } else if n.directory {
        'd'
    } else {
        '-'
    };
    let name = path
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or("/");
    format!(
        "{}{} {:>2} {} {} {:>8} {name}{}",
        type_mode_text(n.mode, kind),
        if n.acl.is_some() { "+" } else { " " },
        n.links,
        c.username(n.owner),
        c.groupname(n.group),
        n.size,
        match (&n.link, n.directory) {
            (Some(target), _) => format!(" -> {target}"),
            (None, true) if name != "/" => "/".into(),
            _ => String::new(),
        }
    )
}

/// `value`, a path typed in `command`, from its working directory, `~` its home, physically: a
/// link on the way, and at its end, followed, as most commands take a path (stat(2)).
pub fn command_path(
    c: &Db,
    who: &Identity,
    command: &Command,
    value: &str,
) -> Result<String, String> {
    c.physical(who, &typed_path(c, who, command, value)?, true)
}
/// The same, but a link at the end is itself, as rm, mv and ln take names (lstat(2)).
pub fn entry_path(
    c: &Db,
    who: &Identity,
    command: &Command,
    value: &str,
) -> Result<String, String> {
    c.physical(who, &typed_path(c, who, command, value)?, false)
}
/// `value` as typed, from the working directory: links not followed.
fn typed_path(c: &Db, who: &Identity, command: &Command, value: &str) -> Result<String, String> {
    let cwd = resolve("/", &command.cwd)?;
    let home = command
        .home
        .clone()
        .or_else(|| c.user_by_id(who.id).map(|(_, user)| user.home))
        .unwrap_or_else(|| "/".into());
    let value = if value == "~" {
        home
    } else if let Some(rest) = value.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        value.into()
    };
    resolve(&cwd, &value)
}

fn touch_paths(c: &Db, who: &Identity, command: &Command) -> Result<Vec<String>, String> {
    let o = options(&command.args[1..], &["-c"], &[])?;
    if o.args.is_empty() {
        return Err("Usage: touch [-c] [--] FILE...".into());
    }
    let mut paths = BTreeSet::new();
    for value in &o.args {
        let path = command_path(c, who, command, value)?;
        c.traverse(who, &path)?;
        if let Some(node) = c.node(&path) {
            if node.directory {
                return Err("This is a directory; touch only supports files".into());
            }
            c.access(who, &path, WRITE)?;
        } else if !o.flags.contains("-c") {
            c.writable_parent(who, &path)?;
            paths.insert(path);
        }
    }
    Ok(paths.into_iter().collect())
}

/// The most results fd returns.
const FD_MAX: usize = 50_000;

/// Remove `path` as rm does, unless it is missing and `-f` was given; whether it was removed.
fn remove_one(
    c: &mut Db,
    who: &Identity,
    cwd: &str,
    path: &str,
    removal: &terminal_worker_core::Removal,
) -> Result<bool, String> {
    if path == "/" {
        return Err("Cannot remove the root directory".into());
    }
    if cwd == path || cwd.starts_with(&format!("{path}/")) {
        return Err(
            "Cannot remove the current directory or its ancestors; cd elsewhere first".into(),
        );
    }
    // A missing path is no error with -f, but only where `who` may look: below a directory it
    // cannot search, rm -f fails whether the path exists or not.
    if removal.force {
        match c.traverse(who, path) {
            Ok(()) if c.node(path).is_none() => return Ok(false),
            // A directory on the way is missing, below ones `who` may search.
            Err(e) if !is_permission_error(&e) => return Ok(false),
            _ => {}
        }
    }
    c.remove(who, path, removal.recursive)?;
    Ok(true)
}

/// Move `src` to `dst`, unless that would move the working directory or an account's home; `cwd`
/// is where the working directory physically is.
fn move_one(
    c: &mut Db,
    who: &Identity,
    cwd: &str,
    src: &str,
    dst: &str,
    replace: bool,
) -> Result<bool, String> {
    if cwd == src || cwd.starts_with(&format!("{src}/")) {
        return Err(
            "Cannot move the current directory or its ancestors; cd elsewhere first".into(),
        );
    }
    for (name, user) in c.users() {
        if user.home == src || user.home.starts_with(&format!("{src}/")) {
            return Err(format!(
                "'{src}' holds the home of {name}; move homes with usermod -d HOME -m {name}"
            ));
        }
    }
    c.rename(who, src, dst, replace)
}

/// Copy `src` to `dst`, sharing content; files still uploading cannot be copied yet. Returns
/// the files that exist, and were not replaced.
fn copy_one(
    c: &mut Db,
    who: &Identity,
    src: &str,
    dst: &str,
    options: &CopyOptions,
) -> Result<Vec<String>, String> {
    // Only what `who` may read and list: the copy refuses the rest, without telling it apart.
    let Ok(node) = c.access(who, src, 0) else {
        return c.copy(who, src, dst, options);
    };
    let below = if node.directory && options.recursive {
        c.walk(who, src, None).map(|(n, _)| n).unwrap_or_default()
    } else {
        Vec::new()
    };
    for (path, node) in std::iter::once((src.to_string(), node)).chain(below) {
        if let Some(file) = node.file.as_deref().and_then(|id| c.file(id))
            && !file.complete()
        {
            return Err(format!(
                "cp: '{path}' is still uploading; copy it once it completes"
            ));
        }
    }
    c.copy(who, src, dst, options)
}

pub fn execute(c: &mut Db, who: &Identity, command: Command) -> Result<Value, String> {
    let (cmd, args) = command.args.split_first().ok_or("Missing command")?;
    let cwd = resolve("/", &command.cwd)?;
    let path_of = |c: &Db, value: &str| command_path(c, who, &command, value);
    let entry_of = |c: &Db, value: &str| entry_path(c, who, &command, value);
    // Where the working directory physically is, which nothing may remove or move.
    let here = c.physical(who, &cwd, true)?;
    let mut listing = None;
    // Commands that take several paths go on after one fails, and report each failure.
    let mut errors: Vec<String> = Vec::new();
    // Whether any of those paths succeeded, so that what did is kept.
    let mut succeeded = false;
    let mut results = None;
    // The files mv and cp -i did not replace, which the terminal asks about.
    let mut existing: Vec<Value> = Vec::new();
    let output = match cmd.as_str() {
        "touch" => {
            // Validate every path before creating any file; an empty file has no chunks.
            let paths = touch_paths(c, who, &command)?;
            for path in &paths {
                let file = c.create_file(0, 1, None, who.id, path, None, crate::auth::now());
                c.finish_file(&file);
                let above = c
                    .node(parent(path))
                    .ok_or("Parent directory does not exist")?;
                let mut node = created(who, &above, 0o666, false, UMASK);
                node.file = Some(file);
                c.set_node(path, &node);
            }
            if paths.is_empty() {
                "File already exists or was skipped; content unchanged".to_string()
            } else {
                format!("Created {}", paths.join(", "))
            }
        }
        "whoami" => {
            if !args.is_empty() {
                return Err("Usage: whoami".into());
            }
            c.username(who.id)
        }
        "id" | "groups" => {
            if args.len() > 1 {
                return Err("Usage: id [USER] / groups [USER]".into());
            }
            let target = if let Some(name) = args.first() {
                Identity::of(&c.user(name).ok_or("User does not exist")?)
            } else {
                who.clone()
            };
            if cmd == "id" {
                show_id(c, &target)
            } else {
                target
                    .groups
                    .iter()
                    .map(|g| c.groupname(*g))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
        "users" => {
            if who.id != 0 {
                return Err("Only root can list accounts".into());
            }
            if !args.is_empty() {
                return Err("Usage: users".into());
            }
            c.users()
                .into_iter()
                .map(|(name, u)| {
                    format!(
                        "{name}\tuid={}\t{}\t{}",
                        u.id,
                        u.home,
                        if u.locked { "locked" } else { "active" }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "ls" => {
            let o = options(args, &["-l", "-a", "-d"], &[])?;
            if o.args.len() > 1 {
                return Err("ls accepts one path at a time".into());
            }
            let typed = o.args.first().map(String::as_str).unwrap_or(".");
            let long = o.flags.contains("-l");
            let itself = o.flags.contains("-d");
            // A link to a directory lists the directory, but with -l or -d, itself, as GNU ls
            // shows a link named on the command line; a trailing / follows it.
            let follow = !(long || itself) || typed.ends_with('/');
            let path = if follow {
                path_of(c, typed)?
            } else {
                entry_of(c, typed)?
            };
            let shown = typed_path(c, who, &command, typed)?;
            let node = c.access(who, &path, 0)?;
            if node.directory && !itself {
                c.access(who, &path, READ | EXEC)?;
                let children = c.children(&path);
                listing = Some((entries(c, who, &path, &shown, &children), long));
                children
                    .iter()
                    .map(|(p, n)| {
                        if long {
                            long_line(c, p, n)
                        } else {
                            let name = p.rsplit('/').next().unwrap_or_default();
                            format!("{name}{}", if n.directory { "/" } else { "" })
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(if long { "\n" } else { "  " })
            } else {
                listing = Some((vec![entry(c, who, &shown, &path, &node)], long));
                if long {
                    long_line(c, &path, &node)
                } else {
                    typed.to_string()
                }
            }
        }
        "mkdir" => {
            let o = options(args, &["-p"], &[])?;
            if o.args.is_empty() {
                return Err("Usage: mkdir [-p] PATH...".into());
            }
            let mut created = Vec::new();
            for value in &o.args {
                match entry_of(c, value).and_then(|path| {
                    c.mkdir(who, &path, o.flags.contains("-p"))?;
                    Ok(path)
                }) {
                    Ok(path) => {
                        succeeded = true;
                        created.push(format!("Created {path}"));
                    }
                    Err(e) => errors.push(e),
                }
            }
            created.join("\n")
        }
        "rm" => {
            let removal = terminal_worker_core::remove_args(args)?;
            let mut removed = Vec::new();
            for value in &removal.paths {
                match entry_of(c, value)
                    .and_then(|path| remove_one(c, who, &here, &path, &removal).map(|r| (path, r)))
                {
                    Ok((path, true)) => {
                        succeeded = true;
                        removed.push(format!("Removed {path}"));
                    }
                    Ok((_, false)) => succeeded = true,
                    Err(e) => errors.push(e),
                }
            }
            removed.join("\n")
        }
        "chmod" | "chown" | "chgrp" => {
            let o = options(
                args,
                if cmd == "chmod" {
                    &["-R"]
                } else {
                    &["-R", "-h"]
                },
                &[],
            )?;
            if o.args.len() < 2 {
                return Err(format!(
                    "Usage: {cmd} [-R]{} {} PATH...",
                    if cmd == "chmod" { "" } else { " [-h]" },
                    match cmd.as_str() {
                        "chmod" => "MODE",
                        "chown" => "USER[:GROUP]",
                        _ => "GROUP",
                    }
                ));
            }
            let recursive = o.flags.contains("-R");
            // chown -h changes a link itself; chmod has no mode for a link, and goes to what it
            // points to, as chown and chgrp do without -h.
            let itself = o.flags.contains("-h");
            let owner_group = if cmd == "chgrp" {
                Some((None, Some(group(c, &o.args[0])?)))
            } else if cmd == "chown" {
                let (user, grp) = o.args[0]
                    .split_once(':')
                    .map(|(u, g)| (u, Some(g)))
                    .unwrap_or((&o.args[0], None));
                let owner = if user.is_empty() {
                    None
                } else {
                    Some(c.user(user).ok_or("User does not exist")?.id)
                };
                let grp = if let Some(g) = grp {
                    if g.is_empty() {
                        owner.and_then(|id| c.user_by_id(id)).map(|(_, u)| u.group)
                    } else {
                        Some(group(c, g)?)
                    }
                } else {
                    None
                };
                if owner.is_none() && grp.is_none() {
                    return Err("Specify a user or group".into());
                }
                Some((owner, grp))
            } else {
                None
            };
            let mut updated = Vec::new();
            for value in &o.args[1..] {
                let path = if itself {
                    entry_of(c, value)
                } else {
                    path_of(c, value)
                };
                match path.and_then(|path| {
                    match owner_group {
                        Some((owner, grp)) => c.chown(who, &path, owner, grp, recursive)?,
                        None => c.chmod(who, &path, &o.args[0], recursive)?,
                    }
                    Ok(path)
                }) {
                    Ok(path) => {
                        succeeded = true;
                        updated.push(format!("Updated {path}"));
                    }
                    Err(e) => errors.push(e),
                }
            }
            updated.join("\n")
        }
        "mv" | "cp" => {
            let copy = cmd == "cp";
            let flags: &[&str] = if copy {
                &[
                    "-r", "-R", "-f", "-n", "-i", "-v", "-P", "-L", "-H", "-d", "-p", "-a",
                ]
            } else {
                &["-f", "-n", "-i", "-v"]
            };
            let o = options(args, flags, &[])?;
            let Some((dest, sources)) = o.args.split_last().filter(|(_, s)| !s.is_empty()) else {
                return Err(format!(
                    "Usage: {cmd} [{}-f|-n|-i] [-v] [--] SOURCE... DEST",
                    if copy {
                        "-r] [-a|-d|-p] [-L|-P|-H] ["
                    } else {
                        ""
                    }
                ));
            };
            // -i replaces nothing here, and names each file that exists, as [SOURCE, TARGET],
            // for the terminal to ask about; -n asks about none.
            let ask = o.flags.contains("-i") && !o.flags.contains("-n");
            let replace = !o.flags.contains("-n") && !ask;
            let all = o.flags.contains("-a");
            let options = CopyOptions {
                recursive: all || o.flags.contains("-r") || o.flags.contains("-R"),
                replace,
                force: o.flags.contains("-f"),
                dereference: o.flags.contains("-L"),
                links: all || o.flags.contains("-d"),
                preserve: all || o.flags.contains("-p"),
                xattrs: all,
            };
            // A link named on the command line is followed by cp without -r, and with -L or -H;
            // otherwise copied as one (-P, also in -d and -a). mv moves names.
            let follow = copy
                && (o.flags.contains("-L")
                    || o.flags.contains("-H")
                    || !options.recursive && !options.links);
            // Into DEST, if it is, or leads to, a directory; otherwise DEST itself, a link there
            // being replaced, or written through.
            let into_dir = path_of(c, dest)?;
            // Only where `who` may look is it told what is there.
            c.traverse(who, &into_dir)?;
            let into = c.node(&into_dir).is_some_and(|n| n.directory);
            let dest = if into { into_dir } else { entry_of(c, dest)? };
            if sources.len() > 1 && !into {
                return Err(format!("{cmd}: target '{dest}' is not a directory"));
            }
            let mut done = Vec::new();
            for value in sources {
                let src = if follow {
                    path_of(c, value)
                } else {
                    entry_of(c, value)
                };
                let result = src.and_then(|src| {
                    let target = if into {
                        joined(&dest, src.rsplit('/').next().unwrap_or_default())
                    } else {
                        dest.clone()
                    };
                    if copy {
                        let skipped = copy_one(c, who, &src, &target, &options)?;
                        let whole = skipped.contains(&target);
                        if ask {
                            // What a directory holds is at the same place below the source.
                            existing.extend(
                                skipped
                                    .into_iter()
                                    .map(|t| json!([format!("{src}{}", &t[target.len()..]), t])),
                            );
                        }
                        if whole {
                            return Ok(None);
                        }
                    } else if !move_one(c, who, &here, &src, &target, replace)? {
                        if ask {
                            existing.push(json!([src, target]));
                        }
                        return Ok(None);
                    }
                    Ok(Some(if copy {
                        format!("'{src}' -> '{target}'")
                    } else {
                        format!("renamed '{src}' -> '{target}'")
                    }))
                });
                match result {
                    Ok(line) => {
                        succeeded = true;
                        if let Some(line) = line.filter(|_| o.flags.contains("-v")) {
                            done.push(line);
                        }
                    }
                    Err(e) => errors.push(e),
                }
            }
            done.join("\n")
        }
        "ln" => {
            let o = options(args, &["-s", "-f", "-n", "-T", "-v", "-L", "-P"], &[])?;
            let symbolic = o.flags.contains("-s");
            let usage =
                || "Usage: ln [-s] [-f] [-n|-T] [-L|-P] [-v] [--] TARGET... [LINK_NAME|DIRECTORY]";
            let (targets, dest) = match o.args.as_slice() {
                [] => return Err(usage().into()),
                // ln TARGET: a link of the same name in the working directory.
                [target] => (std::slice::from_ref(target), None),
                [targets @ .., dest] => (targets, Some(dest.as_str())),
            };
            // Into DEST if it is, or leads to, a directory, unless -n or -T take it as the
            // name of the link itself.
            let plain = o.flags.contains("-n") || o.flags.contains("-T");
            let (dir, named) = match dest {
                None => (Some(here.clone()), None),
                Some(dest) => {
                    let followed = path_of(c, dest)?;
                    if !plain
                        && c.traverse(who, &followed).is_ok()
                        && c.node(&followed).is_some_and(|n| n.directory)
                    {
                        (Some(followed), None)
                    } else {
                        (None, Some(entry_of(c, dest)?))
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
                let result = (|| {
                    if o.flags.contains("-f")
                        && let Some(existing) = c.node(&link)
                    {
                        if existing.directory {
                            return Err(format!("ln: '{link}': cannot overwrite directory"));
                        }
                        c.remove(who, &link, false)?;
                    }
                    if symbolic {
                        // The target is kept as typed, read from where the link is.
                        c.symlink(who, target, &link)?;
                        return Ok(format!("'{link}' -> '{target}'"));
                    }
                    let src = if o.flags.contains("-L") {
                        path_of(c, target)?
                    } else {
                        entry_of(c, target)?
                    };
                    if let Some(file) = c
                        .node(&src)
                        .and_then(|n| n.file)
                        .and_then(|id| c.file(&id))
                        .filter(|f| !f.complete())
                    {
                        let _ = file;
                        return Err(format!(
                            "ln: '{src}' is still uploading; link it once it completes"
                        ));
                    }
                    c.link(who, &src, &link)?;
                    Ok(format!("'{link}' => '{src}'"))
                })();
                match result {
                    Ok(line) => {
                        succeeded = true;
                        if o.flags.contains("-v") {
                            made.push(line);
                        }
                    }
                    Err(e) => errors.push(e),
                }
            }
            made.join("\n")
        }
        "readlink" => {
            let o = options(args, &["-f", "-e", "-m", "-n"], &[])?;
            if o.args.is_empty() {
                return Err("Usage: readlink [-f|-e|-m] [-n] [--] FILE...".into());
            }
            let canonical = ["-f", "-e", "-m"]
                .into_iter()
                .find(|f| o.flags.contains(*f));
            let mut lines = Vec::new();
            for value in &o.args {
                let result = match canonical {
                    // -f: every part but the last must exist; -e: all; -m: none.
                    Some(flag) => path_of(c, value).and_then(|real| {
                        let missing = c.traverse(who, &real).is_err() || c.node(&real).is_none();
                        let above = c.node(parent(&real)).is_some_and(|n| n.directory);
                        match flag {
                            "-e" if missing => Err(format!("{value}: No such file or directory")),
                            "-f" if missing && !above => {
                                Err(format!("{value}: No such file or directory"))
                            }
                            _ => Ok(real),
                        }
                    }),
                    None => entry_of(c, value).and_then(|path| {
                        c.traverse(who, &path)?;
                        c.node(&path)
                            .and_then(|n| n.link)
                            .ok_or_else(|| format!("{value}: not a symbolic link"))
                    }),
                };
                match result {
                    Ok(line) => {
                        succeeded = true;
                        lines.push(line);
                    }
                    Err(e) => errors.push(format!("readlink: {e}")),
                }
            }
            let mut text = lines.join("\n");
            if o.flags.contains("-n") {
                text.push('\0');
            }
            text
        }
        "getfacl" => {
            let o = options(args, &["-a", "-d", "-c", "-n", "-p", "-R", "-P"], &[])?;
            if o.args.is_empty() {
                return Err("Usage: getfacl [-a|-d] [-c] [-n] [-p] [-R] [-P] [--] FILE...".into());
            }
            let numeric = o.flags.contains("-n");
            let user = |id: u32| {
                if numeric {
                    id.to_string()
                } else {
                    c.username(id)
                }
            };
            let group = |id: u32| {
                if numeric {
                    id.to_string()
                } else {
                    c.groupname(id)
                }
            };
            let mut blocks = Vec::new();
            for value in &o.args {
                let start = if o.flags.contains("-P") {
                    entry_of(c, value)
                } else {
                    path_of(c, value)
                };
                let nodes = start.and_then(|path| {
                    let node = c.access(who, &path, 0)?;
                    Ok(if o.flags.contains("-R") && node.directory {
                        c.walk(who, &path, None).map(|(found, _)| {
                            std::iter::once((path.clone(), node.clone()))
                                .chain(found)
                                .filter(|(_, n)| !n.is_link())
                                .collect::<Vec<_>>()
                        })?
                    } else {
                        vec![(path, node)]
                    })
                });
                let nodes = match nodes {
                    Ok(nodes) => nodes,
                    Err(e) => {
                        errors.push(format!("getfacl: {value}: {e}"));
                        continue;
                    }
                };
                succeeded = true;
                for (path, node) in nodes {
                    let mut lines = Vec::new();
                    if !o.flags.contains("-c") {
                        let shown = if o.flags.contains("-p") {
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
                    if !o.flags.contains("-d") {
                        lines.extend(terminal_worker_core::acl::lines(
                            &node.entries(),
                            false,
                            &user,
                            &group,
                        ));
                    }
                    if !o.flags.contains("-a")
                        && let Some(default) = &node.default_acl
                    {
                        lines.extend(terminal_worker_core::acl::lines(
                            default, true, &user, &group,
                        ));
                    }
                    blocks.push(lines.join("\n") + "\n");
                }
            }
            blocks.join("\n")
        }
        "setfacl" => {
            let o = options(
                args,
                &["-b", "-k", "-d", "-R", "-n", "--mask"],
                &["-m", "-x", "--set", "-M", "-X"],
            )?;
            let usage = "Usage: setfacl [-b] [-k] [-d] [-R] [-n] {-m|-x|--set} ACL_ENTRIES FILE...";
            if o.args.is_empty()
                || ["-m", "-x", "--set"]
                    .iter()
                    .all(|f| !o.values.contains_key(*f))
                    && !o.flags.contains("-b")
                    && !o.flags.contains("-k")
            {
                return Err(usage.into());
            }
            if o.values.contains_key("-M") || o.values.contains_key("-X") {
                return Err("setfacl: -M and -X read files of entries, which this terminal does not; give them with -m and -x".into());
            }
            let user = |name: &str| c.user(name).map(|u| u.id);
            let group_id = |name: &str| c.group(name);
            let parse = |text: &str, perms| {
                terminal_worker_core::acl::parse_specs(text, perms, &user, &group_id)
                    .map_err(|e| format!("setfacl: {e}"))
            };
            // -d makes every entry a default one.
            let defaulted = |mut specs: Vec<terminal_worker_core::acl::Spec>| {
                if o.flags.contains("-d") {
                    for spec in &mut specs {
                        spec.default = true;
                    }
                }
                specs
            };
            let modify = defaulted(match o.values.get("-m") {
                Some(text) => parse(text, true)?,
                None => Vec::new(),
            });
            let remove = defaulted(match o.values.get("-x") {
                Some(text) => parse(text, false)?,
                None => Vec::new(),
            });
            let set = defaulted(match o.values.get("--set") {
                Some(text) => parse(text, true)?,
                None => Vec::new(),
            });
            let keep_mask = o.flags.contains("-n");
            let mut updated = Vec::new();
            for value in &o.args {
                let result = path_of(c, value).and_then(|path| {
                    let node = c.access(who, &path, 0)?;
                    let nodes = if o.flags.contains("-R") && node.directory {
                        c.walk(who, &path, None).map(|(found, _)| {
                            std::iter::once((path.clone(), node))
                                .chain(found)
                                .filter(|(_, n)| !n.is_link())
                                .collect::<Vec<_>>()
                        })?
                    } else {
                        vec![(path.clone(), node)]
                    };
                    let mut changed = Vec::new();
                    for (p, mut n) in nodes {
                        if who.id != 0 && who.id != n.owner {
                            return Err(format!("setfacl: {p}: Operation not permitted"));
                        }
                        let executable = n.directory || n.mode & 0o111 != 0;
                        if o.flags.contains("-b") {
                            let mut entries = n.entries();
                            entries.users.clear();
                            entries.groups.clear();
                            entries.mask = None;
                            n.set_entries(&entries);
                            n.default_acl = None;
                        }
                        if o.flags.contains("-k") {
                            n.default_acl = None;
                        }
                        // A default ACL is a directory's; -R passes over files for one.
                        let defaults = !sided(&set, true).is_empty()
                            || !sided(&modify, true).is_empty()
                            || !sided(&remove, true).is_empty();
                        if defaults && !n.directory {
                            if o.flags.contains("-R") && p != path {
                                continue;
                            }
                            return Err(format!(
                                "setfacl: {p}: Only directories can have default ACLs"
                            ));
                        }
                        let mut access = n.entries();
                        let mut default = n.default_acl.clone().unwrap_or_else(|| {
                            // A default ACL begins as the access ACL's base entries.
                            let base = n.entries();
                            terminal_worker_core::acl::Entries {
                                user: base.user,
                                group: base.group,
                                other: base.other,
                                ..Default::default()
                            }
                        });
                        let had_default = n.default_acl.is_some();
                        for (target, default_side) in [(&mut access, false), (&mut default, true)] {
                            let set = sided(&set, default_side);
                            if !set.is_empty() {
                                *target = terminal_worker_core::acl::set(&set, executable)
                                    .map_err(|e| format!("setfacl: {p}: {e}"))?;
                            }
                            terminal_worker_core::acl::modify(
                                target,
                                &sided(&modify, default_side),
                                executable,
                                keep_mask,
                            );
                            let removing = sided(&remove, default_side);
                            if !removing.is_empty() {
                                terminal_worker_core::acl::remove(target, &removing, keep_mask)
                                    .map_err(|e| format!("setfacl: {p}: {e}"))?;
                            }
                        }
                        n.set_entries(&access);
                        let touched_default = !sided(&set, true).is_empty()
                            || !sided(&modify, true).is_empty()
                            || !sided(&remove, true).is_empty();
                        if n.directory && (had_default || touched_default) {
                            n.default_acl = Some(default);
                        }
                        if o.flags.contains("-k") || o.flags.contains("-b") && !touched_default {
                            n.default_acl = None;
                        }
                        changed.push((p, n));
                    }
                    for (p, n) in changed {
                        c.set_node(&p, &n);
                    }
                    Ok(path)
                });
                match result {
                    Ok(path) => {
                        succeeded = true;
                        updated.push(format!("Updated the ACL of {path}"));
                    }
                    Err(e) => errors.push(e),
                }
            }
            updated.join("\n")
        }
        "getfattr" | "setfattr" => {
            let reading = cmd == "getfattr";
            let o = if reading {
                options(
                    args,
                    &["-d", "-h", "-R", "--absolute-names"],
                    &["-n", "-m", "-e"],
                )?
            } else {
                options(args, &["-h"], &["-n", "-v", "-x"])?
            };
            if o.args.is_empty() {
                return Err(if reading {
                    "Usage: getfattr [-d] [-n NAME] [-m PATTERN] [-e text|hex|base64] [-h] [-R] FILE..."
                } else {
                    "Usage: setfattr {-n NAME [-v VALUE] | -x NAME} [-h] FILE..."
                }
                .into());
            }
            let mut out = Vec::new();
            for value in &o.args {
                let start = if o.flags.contains("-h") {
                    entry_of(c, value)
                } else {
                    path_of(c, value)
                };
                let result = start.and_then(|path| {
                    if reading {
                        getfattr(c, who, &path, &o, &mut out)
                    } else {
                        setfattr(c, who, &path, &o)
                    }
                });
                match result {
                    Ok(()) => succeeded = true,
                    Err(e) => errors.push(format!("{cmd}: {value}: {e}")),
                }
            }
            out.join("\n")
        }
        "fd" => {
            let find = terminal_worker_core::find::parse(args)?;
            if find.exec.is_some() {
                return Err("fd -x and -X run in the terminal, not on the server".into());
            }
            let roots = if find.roots.is_empty() {
                vec![(None, cwd.clone())]
            } else {
                find.roots
                    .iter()
                    .map(|r| Ok((Some(r.as_str()), path_of(c, r)?)))
                    .collect::<Result<Vec<_>, String>>()?
            };
            let mut found = Vec::new();
            let mut hidden = 0;
            for (typed, root) in roots {
                match c.walk(who, &root, find.max_depth) {
                    Ok((nodes, h)) => {
                        hidden += h;
                        found.extend(
                            nodes
                                .into_iter()
                                .filter(|(p, n)| {
                                    find.matches(&root, p, n.directory, n.is_link(), n.mode)
                                })
                                .map(|(p, _)| find.display(typed, &root, &cwd, &p)),
                        );
                    }
                    Err(e) => errors.push(format!("fd: {}: {e}", typed.unwrap_or("."))),
                }
                if found.len() > FD_MAX {
                    return Err(format!("fd: more than {FD_MAX} results; narrow the search"));
                }
            }
            if hidden > 0 {
                errors.push(format!(
                    "fd: {hidden} director{} could not be read, so {} not searched",
                    if hidden == 1 { "y" } else { "ies" },
                    if hidden == 1 { "it was" } else { "they were" }
                ));
            }
            results = Some(found.clone());
            let separator = if find.null { '\0' } else { '\n' };
            found.iter().map(|p| format!("{p}{separator}")).collect()
        }
        "useradd" | "usermod" | "userdel" | "groupadd" | "groupmod" | "groupdel" => {
            if who.id != 0 {
                return Err(if cmd == "useradd" && who.id == GUEST {
                    "useradd: signed out, useradd NAME makes an account of your own; options need root".into()
                } else {
                    "Root permission required; use sudo".into()
                });
            }
            match cmd.as_str() {
                "useradd" => {
                    let o = options(args, &["-m"], &["-d", "-g", "-G"])?;
                    let name = one(&o)?;
                    if !valid_name(name) || c.user(name).is_some() {
                        return Err(
                            "Username is invalid or already exists (start with a lowercase letter; up to 32 lowercase letters, digits, underscores, or hyphens)".into(),
                        );
                    }
                    let home = absolute_home(
                        o.values
                            .get("-d")
                            .map(String::as_str)
                            .unwrap_or(&format!("/home/{name}")),
                    )?;
                    protect_home(c, &home, u32::MAX)?;
                    let extra = groups(c, o.values.get("-G").map(String::as_str).unwrap_or(""))?;
                    let gid = if let Some(g) = o.values.get("-g") {
                        group(c, g)?
                    } else {
                        if c.group(name).is_some() {
                            return Err(
                                "Group already exists; use -g to choose the primary group".into()
                            );
                        }
                        let gid = c.next_id(Id::Group);
                        c.set_group(name, gid);
                        gid
                    };
                    let uid = c.next_id(Id::User);
                    if o.flags.contains("-m") {
                        if c.node(&home).is_some() {
                            return Err(
                                "Home already exists; existing content cannot be taken over".into(),
                            );
                        }
                        c.mkdir(&Identity::root(), parent(&home), true)?;
                        c.set_node(&home, &Node::directory(uid, gid, 0o700));
                    }
                    c.set_user(
                        name,
                        &User {
                            id: uid,
                            group: gid,
                            groups: extra,
                            home,
                            locked: true,
                            salt: String::new(),
                            hash: String::new(),
                        },
                    );
                    format!(
                        "Created user {name}; use passwd {name} to set a password before signing in"
                    )
                }
                "usermod" => {
                    let o = options(args, &["-a", "-m", "-L", "-U"], &["-d", "-g", "-G", "-l"])?;
                    let name = one(&o)?;
                    if o.flags.contains("-a") && !o.values.contains_key("-G") {
                        return Err("-a requires -G".into());
                    }
                    if o.flags.contains("-m") && !o.values.contains_key("-d") {
                        return Err("-m requires -d".into());
                    }
                    if o.flags.contains("-L") && o.flags.contains("-U") {
                        return Err("-L and -U are mutually exclusive".into());
                    }
                    if name == "guest" {
                        return Err("The guest account cannot be changed".into());
                    }
                    let mut user = c.user(name).ok_or("User does not exist")?;
                    if user.id == 0 {
                        return Err(
                            "The root identity is fixed; use passwd root to change its password"
                                .into(),
                        );
                    }
                    if o.flags.is_empty() && o.values.is_empty() {
                        return Err(
                            "usermod requires an update option; use passwd to change a password"
                                .into(),
                        );
                    }
                    if let Some(g) = o.values.get("-g") {
                        user.group = group(c, g)?;
                    }
                    if let Some(g) = o.values.get("-G") {
                        let new = groups(c, g)?;
                        if o.flags.contains("-a") {
                            user.groups.extend(new);
                        } else {
                            user.groups = new;
                        }
                    }
                    if let Some(home) = o.values.get("-d") {
                        let home = absolute_home(home)?;
                        protect_home(c, &home, user.id)?;
                        if o.flags.contains("-m") {
                            c.move_home(&user.home, &home)?;
                        }
                        user.home = home;
                    }
                    if o.flags.contains("-L") {
                        user.locked = true;
                    }
                    if o.flags.contains("-U") {
                        if user.hash.is_empty() {
                            return Err("Set a password with passwd first".into());
                        }
                        user.locked = false;
                    }
                    let new = o.values.get("-l").map(String::as_str).unwrap_or(name);
                    if new != name && (!valid_name(new) || c.user(new).is_some()) {
                        return Err("New username is invalid or already exists".into());
                    }
                    if user.locked {
                        c.delete_sessions(user.id);
                    }
                    c.delete_user(name);
                    c.set_user(new, &user);
                    format!("Updated user {new}")
                }
                "userdel" => {
                    let o = options(args, &["-r"], &[])?;
                    let name = one(&o)?;
                    let user = c.user(name).ok_or("User does not exist")?;
                    if user.id == 0 || user.id == GUEST {
                        return Err(format!("Cannot delete {name}"));
                    }
                    if o.flags.contains("-r") && c.node(&user.home).is_some() {
                        protect_home(c, &user.home, user.id)?;
                        c.remove(&Identity::root(), &user.home, true)?;
                    }
                    c.delete_user(name);
                    c.delete_sessions(user.id);
                    format!("Deleted user {name}")
                }
                "groupadd" => {
                    let o = options(args, &[], &[])?;
                    let name = one(&o)?;
                    if !valid_name(name) || c.group(name).is_some() {
                        return Err("Group name is invalid or already exists".into());
                    }
                    let gid = c.next_id(Id::Group);
                    c.set_group(name, gid);
                    format!("Created group {name}")
                }
                "groupmod" => {
                    let o = options(args, &[], &["-n"])?;
                    let name = one(&o)?;
                    let new = o.values.get("-n").ok_or("Usage: groupmod -n NEW OLD")?;
                    if ["root", "wheel", "guest"].contains(&name) {
                        return Err("Cannot rename a system group".into());
                    }
                    let gid = named_group(c, name)?;
                    if !valid_name(new) || c.group(new).is_some() {
                        return Err("Group name is invalid or already exists".into());
                    }
                    c.delete_group(name);
                    c.set_group(new, gid);
                    format!("Group {name} renamed to {new}")
                }
                _ => {
                    let o = options(args, &[], &[])?;
                    let name = one(&o)?;
                    if ["root", "wheel", "guest"].contains(&name) {
                        return Err("Cannot delete a system group".into());
                    }
                    let gid = named_group(c, name)?;
                    if c.users().iter().any(|(_, u)| u.group == gid) {
                        return Err("This group is still an account's primary group".into());
                    }
                    c.delete_group(name);
                    for (n, mut u) in c.users() {
                        if u.groups.remove(&gid) {
                            c.set_user(&n, &u);
                        }
                    }
                    format!("Deleted group {name}")
                }
            }
        }
        _ => {
            return Err(
                "Unsupported server command; sudo supports file and account management commands. Use su root to browse restricted content".into(),
            );
        }
    };
    if !errors.is_empty() && !succeeded && results.is_none() {
        return Err(errors.join("\n"));
    }
    let mut value = json!({"output":output});
    if !errors.is_empty() {
        value["errors"] = json!(errors);
    }
    if let Some((entries, long)) = listing {
        value["entries"] = json!(entries);
        value["long"] = json!(long);
    }
    if let Some(results) = results {
        value["results"] = json!(results);
    }
    if !existing.is_empty() {
        value["existing"] = json!(existing);
    }
    Ok(value)
}

pub fn is_permission_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("current account is not in the wheel group")
        || message.contains("sudo requires reauthentication")
        || message.contains("permission denied")
        || message.contains("only root")
        || message.contains("only the owner")
        || message.contains("root permission required")
        || message.contains("cannot traverse")
        || message.contains("operation not permitted")
}

/// getfattr for one path: its names, or with -d or -n their values, in blocks as getfattr
/// writes them; -R walks a directory.
fn getfattr(
    c: &Db,
    who: &Identity,
    path: &str,
    o: &Options,
    out: &mut Vec<String>,
) -> Result<(), String> {
    use terminal_worker_core::xattr;
    let node = c.access(who, path, 0)?;
    let nodes = if o.flags.contains("-R") && node.directory {
        let (found, _) = c.walk(who, path, None)?;
        std::iter::once((path.to_string(), node))
            .chain(found)
            .collect()
    } else {
        vec![(path.to_string(), node)]
    };
    let encoding = o.values.get("-e").map(String::as_str).unwrap_or("text");
    if !["text", "hex", "base64"].contains(&encoding) {
        return Err(format!(
            "unknown encoding {encoding}; use text, hex or base64"
        ));
    }
    // -m - and -m . match everything; the default, user. only.
    let pattern = o.values.get("-m").map(String::as_str).unwrap_or("^user\\.");
    let matcher = match pattern {
        "-" | "." | "" => None,
        p => Some(regex::Regex::new(p).map_err(|e| format!("bad pattern: {e}"))?),
    };
    for (p, n) in nodes {
        // Reading a file's attributes needs read permission on it.
        if !who.allows(&n, READ) && !n.is_link() {
            return Err(format!("{p}: Permission denied"));
        }
        let attrs = c.xattrs(n.ino);
        let shown: Vec<(&String, &Vec<u8>)> = match o.values.get("-n") {
            Some(name) => match attrs.get_key_value(name) {
                Some(pair) => vec![pair],
                None => return Err(format!("{name}: No such attribute")),
            },
            None => attrs
                .iter()
                .filter(|(name, _)| {
                    xattr::namespace(name)
                        .is_ok_and(|space| space == xattr::Namespace::User || who.id == 0)
                        && matcher.as_ref().is_none_or(|m| m.is_match(name))
                })
                .collect(),
        };
        if shown.is_empty() {
            continue;
        }
        let file = if o.flags.contains("--absolute-names") {
            p.clone()
        } else {
            p.trim_start_matches('/').to_string()
        };
        let mut block = vec![format!("# file: {file}")];
        for (name, value) in shown {
            block.push(if o.flags.contains("-d") || o.values.contains_key("-n") {
                format!("{name}={}", xattr::encode(value, encoding))
            } else {
                name.clone()
            });
        }
        out.push(block.join("\n") + "\n");
    }
    Ok(())
}
/// setfattr for one path: user.* needs w on what is changed, and in a sticky directory being
/// its owner; trusted.* root. A link has none.
fn setfattr(c: &mut Db, who: &Identity, path: &str, o: &Options) -> Result<(), String> {
    use terminal_worker_core::xattr;
    let (name, value) = match (o.values.get("-n"), o.values.get("-x")) {
        (Some(name), None) => (
            name,
            Some(xattr::decode(
                o.values.get("-v").map(String::as_str).unwrap_or(""),
            )?),
        ),
        (None, Some(name)) => (name, None),
        _ => return Err("give -n NAME [-v VALUE], or -x NAME".into()),
    };
    let space = xattr::namespace(name)?;
    let node = c.access(who, path, 0)?;
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
    let mut attrs = c.xattrs(node.ino);
    match value {
        Some(value) => {
            if value.len() > xattr::VALUE_MAX {
                return Err(format!("{name}: Argument list too long"));
            }
            attrs.insert(name.clone(), value);
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
    c.set_xattrs(node.ino, &attrs);
    Ok(())
}

/// The entries of `specs` for the default ACL, if `default`, or for the access ACL.
fn sided(
    specs: &[terminal_worker_core::acl::Spec],
    default: bool,
) -> Vec<&terminal_worker_core::acl::Spec> {
    specs.iter().filter(|s| s.default == default).collect()
}
