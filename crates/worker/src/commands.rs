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
        if u.id != except
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

pub fn entry(c: &Db, path: &str, n: &Node) -> Entry {
    let uploaded = n
        .file
        .as_deref()
        .and_then(|id| c.file(id))
        .filter(|f| !f.complete())
        .map(|f| c.readable(&f));
    described(path, n, uploaded, c.username(n.owner), c.groupname(n.group))
}
/// The entries of `children`, the nodes in directory `dir`, directories first, then by name.
/// Each owner, group and unfinished upload is looked up once, not once per entry.
pub fn entries(c: &Db, dir: &str, children: &[(String, Node)]) -> Vec<Entry> {
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
            described(path, n, uploaded, owner.clone(), group.clone())
        })
        .collect();
    entries.sort_by(|a, b| {
        b.directory
            .cmp(&a.directory)
            .then_with(|| a.name.cmp(&b.name))
    });
    entries
}
fn described(path: &str, n: &Node, uploaded: Option<u64>, owner: String, group: String) -> Entry {
    Entry {
        uploaded,
        name: path.rsplit('/').next().unwrap_or("").into(),
        path: path.into(),
        directory: n.directory,
        size: n.size,
        kind: if n.directory { "directory" } else { kind(path) }.into(),
        owner,
        group,
        mode: n.mode,
    }
}

pub fn command_path(
    c: &Db,
    who: &Identity,
    command: &Command,
    value: &str,
) -> Result<String, String> {
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

/// Move `src` to `dst`, unless that would move the working directory or an account's home.
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
    recursive: bool,
    replace: bool,
) -> Result<Vec<String>, String> {
    // Only what `who` may read and list: the copy refuses the rest, without telling it apart.
    let Ok(node) = c.access(who, src, READ) else {
        return c.copy(who, src, dst, recursive, replace);
    };
    let below = if node.directory && recursive {
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
    c.copy(who, src, dst, recursive, replace)
}

pub fn execute(c: &mut Db, who: &Identity, command: Command) -> Result<Value, String> {
    let (cmd, args) = command.args.split_first().ok_or("Missing command")?;
    let cwd = resolve("/", &command.cwd)?;
    let path_of = |c: &Db, value: &str| command_path(c, who, &command, value);
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
                c.set_node(path, &Node::file(who.id, who.group, 0o644, file, 0));
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
            let o = options(args, &["-l", "-a", "-la", "-al"], &[])?;
            if o.args.len() > 1 {
                return Err("ls accepts one path at a time".into());
            }
            let path = path_of(c, o.args.first().map(String::as_str).unwrap_or("."))?;
            let node = c.access(who, &path, READ | EXEC)?;
            if !node.directory {
                return Err("Not a directory".into());
            }
            let long = o.flags.iter().any(|s| s.contains('l'));
            let children = c.children(&path);
            listing = Some((entries(c, &path, &children), long));
            children
                .iter()
                .map(|(p, n)| {
                    let name = p.rsplit('/').next().unwrap();
                    if long {
                        format!(
                            "{} {} {} {:>8} {name}{}",
                            mode_text(n.mode, n.directory),
                            c.username(n.owner),
                            c.groupname(n.group),
                            n.size,
                            if n.directory { "/" } else { "" }
                        )
                    } else {
                        format!("{name}{}", if n.directory { "/" } else { "" })
                    }
                })
                .collect::<Vec<_>>()
                .join(if long { "\n" } else { "  " })
        }
        "mkdir" => {
            let o = options(args, &["-p"], &[])?;
            if o.args.is_empty() {
                return Err("Usage: mkdir [-p] PATH...".into());
            }
            let mut created = Vec::new();
            for value in &o.args {
                match path_of(c, value).and_then(|path| {
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
                match path_of(c, value)
                    .and_then(|path| remove_one(c, who, &cwd, &path, &removal).map(|r| (path, r)))
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
        "chmod" | "chown" => {
            let o = options(args, &["-R"], &[])?;
            if o.args.len() < 2 {
                return Err(format!(
                    "Usage: {cmd} [-R] {} PATH...",
                    if cmd == "chmod" {
                        "MODE"
                    } else {
                        "USER[:GROUP]"
                    }
                ));
            }
            let recursive = o.flags.contains("-R");
            let owner_group = if cmd == "chown" {
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
                match path_of(c, value).and_then(|path| {
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
                &["-r", "-R", "-f", "-n", "-i", "-v"]
            } else {
                &["-f", "-n", "-i", "-v"]
            };
            let o = options(args, flags, &[])?;
            let Some((dest, sources)) = o.args.split_last().filter(|(_, s)| !s.is_empty()) else {
                return Err(format!(
                    "Usage: {cmd} [{}-f|-n|-i] [-v] [--] SOURCE... DEST",
                    if copy { "-r] [" } else { "" }
                ));
            };
            // -i replaces nothing here, and names each file that exists, as [SOURCE, TARGET],
            // for the terminal to ask about; -n asks about none.
            let ask = o.flags.contains("-i") && !o.flags.contains("-n");
            let replace = !o.flags.contains("-n") && !ask;
            let dest = path_of(c, dest)?;
            // Only where `who` may look is it told what is there.
            c.traverse(who, &dest)?;
            let into = c.node(&dest).is_some_and(|n| n.directory);
            if sources.len() > 1 && !into {
                return Err(format!("{cmd}: target '{dest}' is not a directory"));
            }
            let mut done = Vec::new();
            for value in sources {
                let result = path_of(c, value).and_then(|src| {
                    let target = if into {
                        let name = src.rsplit('/').next().unwrap_or_default();
                        if dest == "/" {
                            format!("/{name}")
                        } else {
                            format!("{dest}/{name}")
                        }
                    } else {
                        dest.clone()
                    };
                    if copy {
                        let skipped = copy_one(
                            c,
                            who,
                            &src,
                            &target,
                            o.flags.contains("-r") || o.flags.contains("-R"),
                            replace,
                        )?;
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
                    } else if !move_one(c, who, &cwd, &src, &target, replace)? {
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
                                .filter(|(p, n)| find.matches(&root, p, n.directory, n.mode))
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
                return Err("Root permission required; use sudo".into());
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
                    if user.id == 0 {
                        return Err("Cannot delete root".into());
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
                    if ["root", "wheel", "nogroup"].contains(&name) {
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
                    if ["root", "wheel", "nogroup"].contains(&name) {
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
}
