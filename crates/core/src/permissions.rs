//! Unix-style access decisions for the virtual filesystem, over any [`Store`]. No host OS access.
use crate::{parent, resolve};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
pub const GUEST: u32 = 65534;
pub const READ: u16 = 4;
pub const WRITE: u16 = 2;
pub const EXEC: u16 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: u32,
    pub group: u32,
    pub groups: BTreeSet<u32>,
    pub home: String,
    pub locked: bool,
    pub salt: String,
    pub hash: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub owner: u32,
    pub group: u32,
    pub mode: u16,
    pub directory: bool,
    /// The immutable content a file shows; copies share it. None for a directory.
    pub file: Option<String>,
    pub size: u64,
}
impl Node {
    pub fn directory(owner: u32, group: u32, mode: u16) -> Self {
        Self {
            owner,
            group,
            mode,
            directory: true,
            file: None,
            size: 0,
        }
    }
    pub fn file(owner: u32, group: u32, mode: u16, file: String, size: u64) -> Self {
        Self {
            owner,
            group,
            mode,
            directory: false,
            file: Some(file),
            size,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub user: u32,
    pub expires: u64,
    pub sudo_until: u64,
}
#[derive(Clone, Debug)]
pub struct Identity {
    pub id: u32,
    pub group: u32,
    pub groups: BTreeSet<u32>,
}
impl Identity {
    pub fn guest() -> Self {
        Self {
            id: GUEST,
            group: GUEST,
            groups: BTreeSet::from([GUEST]),
        }
    }
    pub fn root() -> Self {
        Self {
            id: 0,
            group: 0,
            groups: BTreeSet::from([0]),
        }
    }
    pub fn of(user: &User) -> Self {
        let mut groups = user.groups.clone();
        groups.insert(user.group);
        Self {
            id: user.id,
            group: user.group,
            groups,
        }
    }
    pub fn allows(&self, node: &Node, bits: u16) -> bool {
        if self.id == 0 {
            return true;
        }
        let shift = if self.id == node.owner {
            6
        } else if self.groups.contains(&node.group) {
            3
        } else {
            0
        };
        ((node.mode >> shift) & bits) == bits
    }
}

/// The identifiers [`Store::next_id`] hands out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Id {
    User,
    Group,
}

/// Accounts, sessions and the tree of nodes, as the server keeps them. The permission rules are
/// provided methods, so every store enforces the same ones; a store only reads and writes.
///
/// A store frees a file's content once no node refers to it: when its last node is deleted, or
/// replaced by one showing another file.
pub trait Store {
    fn node(&self, path: &str) -> Option<Node>;
    /// The nodes directly inside `path`, by path.
    fn children(&self, path: &str) -> Vec<(String, Node)>;
    /// `path` and every node below it, by path.
    fn subtree(&self, path: &str) -> Vec<(String, Node)>;
    fn set_node(&mut self, path: &str, node: &Node);
    fn delete_node(&mut self, path: &str);

    fn user(&self, name: &str) -> Option<User>;
    fn user_by_id(&self, id: u32) -> Option<(String, User)>;
    fn users(&self) -> Vec<(String, User)>;
    /// Insert or replace the account with this user's id, under `name`.
    fn set_user(&mut self, name: &str, user: &User);
    fn delete_user(&mut self, name: &str);

    fn group(&self, name: &str) -> Option<u32>;
    fn group_name(&self, id: u32) -> Option<String>;
    fn groups(&self) -> Vec<(String, u32)>;
    fn set_group(&mut self, name: &str, id: u32);
    fn delete_group(&mut self, name: &str);

    fn session(&self, key: &str) -> Option<Session>;
    fn set_session(&mut self, key: &str, session: &Session);
    fn delete_session(&mut self, key: &str);
    /// Revoke every session of `user`.
    fn delete_sessions(&mut self, user: u32);
    fn delete_expired_sessions(&mut self, now: u64);

    fn next_id(&mut self, kind: Id) -> u32;

    fn username(&self, id: u32) -> String {
        if id == GUEST {
            "guest".into()
        } else {
            self.user_by_id(id)
                .map(|(n, _)| n)
                .unwrap_or_else(|| id.to_string())
        }
    }
    fn groupname(&self, id: u32) -> String {
        self.group_name(id).unwrap_or_else(|| id.to_string())
    }
    fn can_sudo(&self, who: &Identity) -> bool {
        who.id == 0 || self.group("wheel").is_some_and(|g| who.groups.contains(&g))
    }
    /// Whether `who` may search every directory above `path`, from the root down: what lies
    /// below a directory it may not search is never looked at, not even whether it exists.
    fn traverse(&self, who: &Identity, path: &str) -> Result<(), String> {
        let mut at = String::from("/");
        let mut parts = parent(path).split('/').filter(|part| !part.is_empty());
        loop {
            let node = self.node(&at).ok_or("Parent directory does not exist")?;
            if !node.directory || !who.allows(&node, EXEC) {
                return Err(format!("Permission denied: cannot traverse {at}"));
            }
            let Some(part) = parts.next() else {
                break;
            };
            if at != "/" {
                at.push('/');
            }
            at.push_str(part);
        }
        Ok(())
    }
    fn access(&self, who: &Identity, path: &str, bits: u16) -> Result<Node, String> {
        self.traverse(who, path)?;
        let node = self.node(path).ok_or("File or directory does not exist")?;
        if !who.allows(&node, bits) {
            return Err(format!("Permission denied: {path}"));
        }
        Ok(node)
    }
    fn writable_parent(&self, who: &Identity, path: &str) -> Result<(), String> {
        let node = self.access(who, parent(path), WRITE | EXEC)?;
        if !node.directory {
            return Err("Parent path is not a directory".into());
        }
        Ok(())
    }
    fn mkdir(&mut self, who: &Identity, path: &str, parents: bool) -> Result<(), String> {
        let exists = || format!("cannot create directory '{path}': it already exists");
        if path == "/" {
            self.access(who, "/", EXEC)?;
            return if parents { Ok(()) } else { Err(exists()) };
        }
        let mut at = String::new();
        // Validate every component before mutating, including mkdir -p through existing directories.
        let mut pending = Vec::new();
        for part in path.trim_start_matches('/').split('/') {
            at.push('/');
            at.push_str(part);
            if let Some(node) = self.node(&at) {
                self.access(who, &at, EXEC)?;
                if !node.directory {
                    return Err("A file with this name already exists in the path".into());
                }
            } else {
                if !parents && at != path {
                    return Err("Parent directory does not exist; use mkdir -p".into());
                }
                if pending.is_empty() {
                    self.writable_parent(who, &at)?;
                }
                pending.push(at.clone());
            }
        }
        // As mkdir does without -p, a directory that exists already is an error.
        if pending.is_empty() && !parents {
            return Err(exists());
        }
        for path in pending {
            self.set_node(&path, &Node::directory(who.id, who.group, 0o755));
        }
        Ok(())
    }
    fn remove(&mut self, who: &Identity, path: &str, recursive: bool) -> Result<(), String> {
        if path == "/" {
            return Err("Cannot remove the root directory".into());
        }
        self.writable_parent(who, path)?;
        let node = self.node(path).ok_or("File or directory does not exist")?;
        if node.directory && !recursive {
            return Err("This is a directory; use rm -r DIR".into());
        }
        let nodes = self.subtree(path);
        // rm -r needs to enumerate and modify every directory being emptied.
        for (p, n) in &nodes {
            if n.directory {
                self.access(who, p, READ | WRITE | EXEC)?;
            }
        }
        for (p, _) in nodes {
            self.delete_node(&p);
        }
        Ok(())
    }
    fn chmod(
        &mut self,
        who: &Identity,
        path: &str,
        mode: &str,
        recursive: bool,
    ) -> Result<(), String> {
        self.access(who, path, 0)?;
        let nodes = if recursive {
            self.subtree(path)
        } else {
            vec![(path.into(), self.access(who, path, 0)?)]
        };
        let mut changes = Vec::new();
        for (p, mut n) in nodes {
            self.access(who, &p, 0)?;
            if who.id != 0 && who.id != n.owner {
                return Err("Only the owner or root can chmod".into());
            }
            n.mode = change_mode(n.mode, mode, n.directory)?;
            changes.push((p, n));
        }
        for (p, n) in changes {
            self.set_node(&p, &n);
        }
        Ok(())
    }
    fn chown(
        &mut self,
        who: &Identity,
        path: &str,
        owner: Option<u32>,
        group: Option<u32>,
        recursive: bool,
    ) -> Result<(), String> {
        self.access(who, path, 0)?;
        let nodes = if recursive {
            self.subtree(path)
        } else {
            vec![(path.into(), self.access(who, path, 0)?)]
        };
        for (p, n) in &nodes {
            self.access(who, p, 0)?;
            if who.id != 0
                && (who.id != n.owner
                    || owner.is_some_and(|id| id != n.owner)
                    || group.is_some_and(|g| !who.groups.contains(&g)))
            {
                return Err("Only root can change ownership; owners may change the group to one they belong to".into());
            }
        }
        for (p, mut n) in nodes {
            if let Some(id) = owner {
                n.owner = id;
            }
            if let Some(id) = group {
                n.group = id;
            }
            self.set_node(&p, &n);
        }
        Ok(())
    }
    /// The nodes below the directory `root` that `who` may list, by path: every directory on
    /// the way must be readable and searchable. Also how many directories hid their contents.
    fn walk(
        &self,
        who: &Identity,
        root: &str,
        max_depth: Option<usize>,
    ) -> Result<(Vec<(String, Node)>, usize), String> {
        let node = self.access(who, root, READ | EXEC)?;
        if !node.directory {
            return Err(format!("Not a directory: {root}"));
        }
        let depth_of = |path: &str| {
            path[root.len()..]
                .split('/')
                .filter(|p| !p.is_empty())
                .count()
        };
        let mut listable = BTreeSet::from([root.to_string()]);
        let mut found = Vec::new();
        let mut hidden = 0;
        // Parents come before their children, by path.
        for (path, node) in self.subtree(root) {
            if path == root
                || !listable.contains(parent(&path))
                || max_depth.is_some_and(|d| depth_of(&path) > d)
            {
                continue;
            }
            if node.directory {
                if who.allows(&node, READ | EXEC) {
                    listable.insert(path.clone());
                } else {
                    hidden += 1;
                }
            }
            found.push((path, node));
        }
        Ok((found, hidden))
    }
    /// Move `src` to `dst` with its owner and mode, as rename does: both parents need w+x, and
    /// a directory changing parent needs w itself. An existing file at `dst` is replaced if
    /// `replace`, and skipped otherwise, returning false; a directory is never replaced.
    fn rename(
        &mut self,
        who: &Identity,
        src: &str,
        dst: &str,
        replace: bool,
    ) -> Result<bool, String> {
        if src == "/" {
            return Err("Cannot move the root directory".into());
        }
        if src == dst {
            return Err(format!("'{src}' and '{dst}' are the same file"));
        }
        if dst.starts_with(&format!("{src}/")) {
            return Err(format!("Cannot move '{src}' into itself"));
        }
        self.writable_parent(who, src)?;
        let node = self.node(src).ok_or("File or directory does not exist")?;
        self.writable_parent(who, dst)?;
        if node.directory && parent(src) != parent(dst) {
            self.access(who, src, WRITE)?;
        }
        if let Some(existing) = self.node(dst) {
            if existing.directory {
                return Err(format!("Cannot overwrite the directory '{dst}'"));
            }
            if node.directory {
                return Err(format!(
                    "Cannot overwrite the file '{dst}' with a directory"
                ));
            }
            if !replace {
                return Ok(false);
            }
        }
        let nodes = self.subtree(src);
        let mut moved = Vec::new();
        for (p, n) in &nodes {
            let target = format!("{dst}{}", &p[src.len()..]);
            resolve("/", &target)?;
            moved.push((target, n.clone()));
        }
        // Write the new paths first, so no file is ever left without a node.
        for (p, n) in moved {
            self.set_node(&p, &n);
        }
        for (p, _) in nodes {
            self.delete_node(&p);
        }
        Ok(true)
    }
    /// Copy `src` to `dst`, sharing its content: no bytes are copied. Directories need `recursive`
    /// and merge into existing ones. New nodes belong to `who`, with the source's mode less the
    /// usual umask (022); an existing file keeps its owner and mode, and is replaced only if
    /// `replace` (it needs w), and skipped otherwise. Returns the paths skipped.
    fn copy(
        &mut self,
        who: &Identity,
        src: &str,
        dst: &str,
        recursive: bool,
        replace: bool,
    ) -> Result<Vec<String>, String> {
        let node = self.access(who, src, READ)?;
        if node.directory && !recursive {
            return Err(format!("-r not specified; omitting directory '{src}'"));
        }
        if src == dst {
            return Err(format!("'{src}' and '{dst}' are the same file"));
        }
        if dst.starts_with(&format!("{src}/")) {
            return Err(format!("Cannot copy '{src}' into itself"));
        }
        let nodes = if node.directory {
            self.walk(who, src, None)
                .map(|(nodes, _)| nodes)
                .map(|mut nodes| {
                    nodes.insert(0, (src.to_string(), node.clone()));
                    nodes
                })?
        } else {
            vec![(src.to_string(), node)]
        };
        // Validate everything before writing anything.
        let mut writes = Vec::new();
        let mut skipped = Vec::new();
        let mut created = BTreeSet::new();
        for (path, source) in &nodes {
            if source.directory {
                self.access(who, path, READ | EXEC)?;
            } else {
                self.access(who, path, READ)?;
            }
            let target = format!("{dst}{}", &path[src.len()..]);
            resolve("/", &target)?;
            // Only where `who` may look is it told what is there, also deep in a merge: in a
            // directory this copy creates, nothing is.
            if !created.contains(parent(&target)) {
                self.traverse(who, &target)?;
            }
            match self.node(&target) {
                Some(existing) if existing.directory && source.directory => {}
                Some(existing) if existing.directory || source.directory => {
                    return Err(format!(
                        "Cannot overwrite '{target}' with a {}",
                        if source.directory {
                            "directory"
                        } else {
                            "file"
                        }
                    ));
                }
                Some(existing) => {
                    if !replace {
                        skipped.push(target);
                        continue;
                    }
                    self.access(who, &target, WRITE)?;
                    writes.push((
                        target,
                        Node {
                            file: source.file.clone(),
                            size: source.size,
                            ..existing
                        },
                    ));
                }
                None => {
                    // A directory this copy creates is the copier's to write into.
                    if !created.contains(parent(&target)) {
                        self.writable_parent(who, &target)?;
                    }
                    if source.directory {
                        created.insert(target.clone());
                    }
                    writes.push((
                        target,
                        Node {
                            owner: who.id,
                            group: who.group,
                            mode: source.mode & 0o755,
                            ..source.clone()
                        },
                    ));
                }
            }
        }
        for (path, node) in writes {
            self.set_node(&path, &node);
        }
        Ok(skipped)
    }
    fn move_home(&mut self, old: &str, new: &str) -> Result<(), String> {
        if old == "/"
            || new == "/"
            || old == new
            || new.starts_with(&format!("{old}/"))
            || old.starts_with(&format!("{new}/"))
        {
            return Err("Home move paths must differ and cannot contain each other".into());
        }
        if self.node(new).is_some() {
            return Err("Destination already exists; the move will not overwrite it".into());
        }
        let n = self.node(old).ok_or("Original home does not exist")?;
        if !n.directory {
            return Err("Original home is not a directory".into());
        }
        let nodes = self.subtree(old);
        let mut moved = Vec::new();
        for (p, n) in &nodes {
            let target = format!("{new}{}", &p[old.len()..]);
            resolve("/", &target)?;
            moved.push((target, n.clone()));
        }
        self.mkdir(&Identity::root(), parent(new), true)?;
        // Write the new paths first, so no file is ever left without a node.
        for (p, n) in moved {
            self.set_node(&p, &n);
        }
        for (p, _) in nodes {
            self.delete_node(&p);
        }
        Ok(())
    }
}

/// A [`Store`] in memory, for tests and tools.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemStore {
    pub users: BTreeMap<String, User>,
    pub groups: BTreeMap<String, u32>,
    pub nodes: BTreeMap<String, Node>,
    pub sessions: BTreeMap<String, Session>,
    pub next_user: u32,
    pub next_group: u32,
    /// Files no node shows any more, in the order they were freed.
    pub freed: Vec<String>,
}
impl MemStore {
    /// A filesystem with only `root` and the system groups, and `/` as its tree.
    pub fn new(root: User) -> Self {
        Self {
            users: BTreeMap::from([("root".into(), root)]),
            groups: BTreeMap::from([
                ("root".into(), 0),
                ("wheel".into(), 27),
                ("nogroup".into(), GUEST),
            ]),
            nodes: BTreeMap::from([("/".into(), Node::directory(0, 0, 0o755))]),
            sessions: BTreeMap::new(),
            next_user: 1000,
            next_group: 1000,
            freed: Vec::new(),
        }
    }
    fn release(&mut self, file: Option<String>) {
        if let Some(file) = file
            && !self
                .nodes
                .values()
                .any(|n| n.file.as_deref() == Some(&file))
        {
            self.freed.push(file);
        }
    }
}
impl Store for MemStore {
    fn node(&self, path: &str) -> Option<Node> {
        self.nodes.get(path).cloned()
    }
    fn children(&self, path: &str) -> Vec<(String, Node)> {
        self.subtree(path)
            .into_iter()
            .filter(|(p, _)| p != path && parent(p) == path)
            .collect()
    }
    fn subtree(&self, path: &str) -> Vec<(String, Node)> {
        let prefix = if path == "/" {
            "/".into()
        } else {
            format!("{path}/")
        };
        self.nodes
            .iter()
            .filter(|(p, _)| *p == path || p.starts_with(&prefix))
            .map(|(p, n)| (p.clone(), n.clone()))
            .collect()
    }
    fn set_node(&mut self, path: &str, node: &Node) {
        let old = self.nodes.insert(path.into(), node.clone());
        if let Some(old) = old
            && old.file != node.file
        {
            self.release(old.file);
        }
    }
    fn delete_node(&mut self, path: &str) {
        if let Some(old) = self.nodes.remove(path) {
            self.release(old.file);
        }
    }
    fn user(&self, name: &str) -> Option<User> {
        self.users.get(name).cloned()
    }
    fn user_by_id(&self, id: u32) -> Option<(String, User)> {
        self.users
            .iter()
            .find(|(_, u)| u.id == id)
            .map(|(n, u)| (n.clone(), u.clone()))
    }
    fn users(&self) -> Vec<(String, User)> {
        self.users
            .iter()
            .map(|(n, u)| (n.clone(), u.clone()))
            .collect()
    }
    fn set_user(&mut self, name: &str, user: &User) {
        self.users.retain(|_, u| u.id != user.id);
        self.users.insert(name.into(), user.clone());
    }
    fn delete_user(&mut self, name: &str) {
        self.users.remove(name);
    }
    fn group(&self, name: &str) -> Option<u32> {
        self.groups.get(name).copied()
    }
    fn group_name(&self, id: u32) -> Option<String> {
        self.groups
            .iter()
            .find(|(_, g)| **g == id)
            .map(|(n, _)| n.clone())
    }
    fn groups(&self) -> Vec<(String, u32)> {
        self.groups.iter().map(|(n, g)| (n.clone(), *g)).collect()
    }
    fn set_group(&mut self, name: &str, id: u32) {
        self.groups.insert(name.into(), id);
    }
    fn delete_group(&mut self, name: &str) {
        self.groups.remove(name);
    }
    fn session(&self, key: &str) -> Option<Session> {
        self.sessions.get(key).cloned()
    }
    fn set_session(&mut self, key: &str, session: &Session) {
        self.sessions.insert(key.into(), session.clone());
    }
    fn delete_session(&mut self, key: &str) {
        self.sessions.remove(key);
    }
    fn delete_sessions(&mut self, user: u32) {
        self.sessions.retain(|_, s| s.user != user);
    }
    fn delete_expired_sessions(&mut self, now: u64) {
        self.sessions.retain(|_, s| s.expires > now);
    }
    fn next_id(&mut self, kind: Id) -> u32 {
        let next = match kind {
            Id::User => &mut self.next_user,
            Id::Group => &mut self.next_group,
        };
        *next += 1;
        *next - 1
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && name != "guest"
}
pub fn mode_text(mode: u16, directory: bool) -> String {
    let mut out = String::from(if directory { "d" } else { "-" });
    for shift in [6, 3, 0] {
        for (bit, c) in [(4, 'r'), (2, 'w'), (1, 'x')] {
            out.push(if mode & (bit << shift) != 0 { c } else { '-' });
        }
    }
    out
}
pub fn change_mode(mut mode: u16, spec: &str, directory: bool) -> Result<u16, String> {
    if (spec.len() == 3 || spec.len() == 4) && spec.bytes().all(|c| (b'0'..=b'7').contains(&c)) {
        let value = u16::from_str_radix(spec, 8).unwrap();
        if value <= 0o777 {
            return Ok(value);
        }
        return Err(
            "Only rwx modes 000-777 are supported; special permission bits are unavailable".into(),
        );
    }
    for clause in spec.split(',') {
        let chars: Vec<char> = clause.chars().collect();
        let mut i = 0;
        let mut mask = 0;
        while i < chars.len() {
            mask |= match chars[i] {
                'u' => 0o700,
                'g' => 0o070,
                'o' => 0o007,
                'a' => 0o777,
                _ => break,
            };
            i += 1;
        }
        if mask == 0 {
            mask = 0o777;
        }
        let mut operated = false;
        while i < chars.len() {
            let op = chars[i];
            if !['+', '-', '='].contains(&op) {
                return Err("chmod mode: 640 or u+rw,g-x,o=r".into());
            }
            i += 1;
            let mut bits = 0;
            while i < chars.len() && !['+', '-', '='].contains(&chars[i]) {
                bits |= match chars[i] {
                    'r' => 0o444,
                    'w' => 0o222,
                    'x' => 0o111,
                    'X' => {
                        if directory || mode & 0o111 != 0 {
                            0o111
                        } else {
                            0
                        }
                    }
                    'u' => ((mode >> 6) & 7) * 0o111,
                    'g' => ((mode >> 3) & 7) * 0o111,
                    'o' => (mode & 7) * 0o111,
                    _ => return Err("Unsupported chmod permission bit".into()),
                };
                i += 1;
            }
            let bits = bits & mask;
            mode = match op {
                '+' => mode | bits,
                '-' => mode & !bits,
                _ => (mode & !mask) | bits,
            };
            operated = true;
        }
        if !operated {
            return Err("chmod requires an operator".into());
        }
    }
    Ok(mode)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> MemStore {
        MemStore::new(User {
            id: 0,
            group: 0,
            groups: BTreeSet::new(),
            home: "/root".into(),
            locked: false,
            salt: String::new(),
            hash: String::new(),
        })
    }
    fn file(owner: u32, mode: u16, content: &str) -> Node {
        Node::file(owner, owner, mode, content.into(), 1)
    }
    #[test]
    fn unix_mode_precedence_and_root_override() {
        let owner = Identity {
            id: 1,
            group: 2,
            groups: BTreeSet::from([2]),
        };
        let n = Node::directory(1, 2, 0o077);
        assert!(!owner.allows(&n, READ));
        assert!(Identity::guest().allows(&n, READ));
        assert!(Identity::root().allows(&Node::directory(9, 9, 0), 7));
    }
    #[test]
    fn symbolic_and_numeric_modes() {
        assert_eq!(change_mode(0o640, "o+w,g-x", false).unwrap(), 0o642);
        assert_eq!(change_mode(0o777, "u=rw,go=", false).unwrap(), 0o600);
        assert_eq!(change_mode(0o600, "g=u,o=g", false).unwrap(), 0o666);
        assert_eq!(change_mode(0o644, "a+X", false).unwrap(), 0o644);
        assert_eq!(change_mode(0o644, "a+X", true).unwrap(), 0o755);
        assert!(change_mode(0o644, "4755", true).is_err());
        assert_eq!(mode_text(0o750, true), "drwxr-x---");
    }
    #[test]
    fn traversal_cannot_be_bypassed_by_readable_child() {
        let mut c = store();
        c.set_node("/private", &Node::directory(0, 0, 0o700));
        c.set_node("/private/readme.txt", &file(0, 0o644, "readme"));
        assert!(
            c.access(&Identity::guest(), "/private/readme.txt", READ)
                .is_err()
        );
        assert!(
            c.access(&Identity::root(), "/private/readme.txt", READ)
                .is_ok()
        );
        // Whether something exists below a directory one cannot search does not show.
        c.set_node("/private/inner", &Node::directory(0, 0, 0o755));
        for path in ["/private/inner/x", "/private/missing/x"] {
            assert_eq!(
                c.access(&Identity::guest(), path, READ).unwrap_err(),
                "Permission denied: cannot traverse /private",
                "{path}"
            );
        }
    }
    #[test]
    fn mkdir_without_parents_refuses_what_exists() {
        let mut c = store();
        c.mkdir(&Identity::root(), "/a", false).unwrap();
        assert!(c.mkdir(&Identity::root(), "/a", false).is_err());
        assert!(c.mkdir(&Identity::root(), "/", false).is_err());
        assert!(c.mkdir(&Identity::root(), "/a", true).is_ok());
        assert!(c.mkdir(&Identity::root(), "/a/b/c", false).is_err());
        assert!(c.mkdir(&Identity::root(), "/a/b/c", true).is_ok());
    }
    #[test]
    fn deletion_checks_parent_and_every_descendant_before_mutation() {
        let mut c = store();
        c.nodes.get_mut("/").unwrap().mode = 0o777;
        c.set_node("/shared", &Node::directory(0, 0, 0o777));
        c.set_node("/shared/private", &Node::directory(0, 0, 0o700));
        c.set_node("/shared/a.txt", &file(0, 0o644, "a"));
        assert!(c.remove(&Identity::guest(), "/shared", true).is_err());
        assert!(c.nodes.contains_key("/shared/a.txt"));
        assert!(c.freed.is_empty());
        assert!(c.remove(&Identity::root(), "/shared", true).is_ok());
        assert_eq!(c.nodes.len(), 1);
        assert_eq!(c.freed, ["a"]);
    }
    #[test]
    fn content_is_freed_only_when_no_node_shows_it() {
        let mut c = store();
        c.set_node("/a.txt", &file(0, 0o644, "shared"));
        c.set_node("/b.txt", &file(0, 0o644, "shared"));
        c.delete_node("/a.txt");
        assert!(c.freed.is_empty());
        // Replacing a node's content frees the old content, but rewriting its mode does not.
        c.set_node("/b.txt", &file(0, 0o600, "shared"));
        assert!(c.freed.is_empty());
        c.set_node("/b.txt", &file(0, 0o600, "new"));
        assert_eq!(c.freed, ["shared"]);
    }
    #[test]
    fn children_and_subtrees_follow_path_boundaries() {
        let mut c = store();
        for path in ["/a", "/a/b", "/ab"] {
            c.set_node(path, &Node::directory(0, 0, 0o755));
        }
        c.set_node("/a/b/spéctrum.txt", &file(0, 0o644, "s"));
        let paths =
            |nodes: Vec<(String, Node)>| nodes.into_iter().map(|(p, _)| p).collect::<Vec<_>>();
        assert_eq!(paths(c.children("/")), ["/a", "/ab"]);
        assert_eq!(paths(c.children("/a")), ["/a/b"]);
        assert_eq!(paths(c.subtree("/a")), ["/a", "/a/b", "/a/b/spéctrum.txt"]);
        assert_eq!(c.subtree("/").len(), 5);
    }
    #[test]
    fn home_move_preserves_content_and_metadata() {
        let mut c = store();
        c.mkdir(&Identity::root(), "/home/alice", true).unwrap();
        c.set_node("/home/alice/a.txt", &file(1000, 0o600, "immutable"));
        c.move_home("/home/alice", "/homes/alice").unwrap();
        assert!(!c.nodes.contains_key("/home/alice"));
        assert_eq!(
            c.nodes["/homes/alice/a.txt"].file.as_deref(),
            Some("immutable")
        );
        assert_eq!(c.nodes["/homes/alice/a.txt"].mode, 0o600);
        assert!(c.freed.is_empty());
        assert!(c.move_home("/homes/alice", "/homes/alice/sub").is_err());
    }
    fn alice() -> Identity {
        Identity {
            id: 1000,
            group: 1000,
            groups: BTreeSet::from([1000]),
        }
    }
    /// /home/alice (alice's), holding a.txt, docs/b.md and a private directory.
    fn home() -> MemStore {
        let mut c = store();
        c.set_node("/home", &Node::directory(0, 0, 0o755));
        c.set_node("/home/alice", &Node::directory(1000, 1000, 0o755));
        c.set_node("/home/alice/a.txt", &file(1000, 0o664, "a"));
        c.set_node("/home/alice/docs", &Node::directory(1000, 1000, 0o755));
        c.set_node("/home/alice/docs/b.md", &file(1000, 0o644, "b"));
        c.set_node("/home/alice/private", &Node::directory(1000, 1000, 0o700));
        c.set_node("/home/alice/private/c.txt", &file(1000, 0o644, "c"));
        c
    }
    #[test]
    fn walks_show_only_what_listings_would() {
        let c = home();
        let paths = |who: &Identity| {
            let (nodes, hidden) = c.walk(who, "/home/alice", None).unwrap();
            (
                nodes.into_iter().map(|(p, _)| p).collect::<Vec<_>>(),
                hidden,
            )
        };
        assert_eq!(paths(&alice()).0.len(), 5);
        let (seen, hidden) = paths(&Identity::guest());
        assert_eq!(
            seen,
            [
                "/home/alice/a.txt",
                "/home/alice/docs",
                "/home/alice/docs/b.md",
                "/home/alice/private"
            ]
        );
        assert_eq!(hidden, 1);
        let (shallow, _) = c.walk(&alice(), "/home/alice", Some(1)).unwrap();
        assert_eq!(shallow.len(), 3);
        assert!(c.walk(&alice(), "/home/alice/a.txt", None).is_err());
    }
    #[test]
    fn renames_move_trees_and_follow_the_rules_of_rename() {
        let mut c = home();
        assert!(
            c.rename(&alice(), "/home/alice/docs", "/home/alice/notes", false)
                .unwrap()
        );
        assert!(c.nodes.contains_key("/home/alice/notes/b.md"));
        assert!(!c.nodes.contains_key("/home/alice/docs"));
        assert!(c.freed.is_empty());
        // An existing file is replaced, or skipped; its old content is freed when replaced.
        assert!(
            !c.rename(
                &alice(),
                "/home/alice/a.txt",
                "/home/alice/notes/b.md",
                false
            )
            .unwrap()
        );
        assert!(
            c.rename(
                &alice(),
                "/home/alice/a.txt",
                "/home/alice/notes/b.md",
                true
            )
            .unwrap()
        );
        assert_eq!(c.nodes["/home/alice/notes/b.md"].file.as_deref(), Some("a"));
        assert_eq!(c.freed, ["b"]);
        for (src, dst) in [
            ("/home/alice/notes", "/home/alice/notes/inner"),
            ("/home/alice/notes", "/home/alice/private"),
            ("/home/alice/notes/b.md", "/home/alice/notes/b.md"),
        ] {
            assert!(
                c.rename(&alice(), src, dst, true).is_err(),
                "{src} -> {dst}"
            );
        }
        // Moving needs w+x on both parents.
        assert!(
            c.rename(&alice(), "/home/alice/notes/b.md", "/home/b.md", true)
                .is_err()
        );
        assert!(
            c.rename(
                &Identity::guest(),
                "/home/alice/notes/b.md",
                "/home/alice/b.md",
                true
            )
            .is_err()
        );
    }
    #[test]
    fn copies_share_content_merge_directories_and_keep_existing_files_modes() {
        let mut c = home();
        let bob = Identity {
            id: 1001,
            group: 1001,
            groups: BTreeSet::from([1001]),
        };
        c.set_node("/tmp", &Node::directory(0, 0, 0o777));
        // A directory needs -r; bob may not read alice's private directory.
        assert!(
            c.copy(&bob, "/home/alice/docs", "/tmp/docs", false, true)
                .is_err()
        );
        assert!(
            c.copy(&bob, "/home/alice", "/tmp/alice", true, true)
                .is_err()
        );
        assert!(!c.nodes.contains_key("/tmp/alice"));
        assert!(
            c.copy(&bob, "/home/alice/docs", "/tmp/docs", true, true)
                .unwrap()
                .is_empty()
        );
        let copied = &c.nodes["/tmp/docs/b.md"];
        assert_eq!(
            (copied.owner, copied.mode, copied.file.as_deref()),
            (1001, 0o644, Some("b"))
        );
        assert_eq!(c.nodes["/tmp/docs"].owner, 1001);
        // The group write bit is masked, as umask 022 would.
        assert!(
            c.copy(&bob, "/home/alice/a.txt", "/tmp/docs/a.txt", false, true)
                .is_ok()
        );
        assert_eq!(c.nodes["/tmp/docs/a.txt"].mode, 0o644);
        // Copying again merges, replaces files keeping their mode, or skips them with -n.
        c.nodes.get_mut("/tmp/docs/b.md").unwrap().mode = 0o600;
        c.set_node("/home/alice/docs/new.md", &file(1000, 0o644, "new"));
        let skipped = c
            .copy(&bob, "/home/alice/docs", "/tmp/docs", true, false)
            .unwrap();
        assert_eq!(skipped, ["/tmp/docs/b.md"]);
        assert!(c.nodes.contains_key("/tmp/docs/new.md"));
        c.set_node("/home/alice/docs/b.md", &file(1000, 0o644, "b2"));
        c.copy(&bob, "/home/alice/docs", "/tmp/docs", true, true)
            .unwrap();
        assert_eq!(c.nodes["/tmp/docs/b.md"].file.as_deref(), Some("b2"));
        assert_eq!(c.nodes["/tmp/docs/b.md"].mode, 0o600);
        // Removing the original keeps a copy's content.
        c.remove(&Identity::root(), "/home/alice/docs", true)
            .unwrap();
        assert!(!c.freed.contains(&"b2".to_string()));
        assert!(
            c.copy(&alice(), "/home/alice", "/home/alice/inner", true, true)
                .is_err()
        );
        // A merge says nothing of what lies below a directory bob may not search: there or not,
        // the copy is refused alike, and nothing is skipped or written.
        c.set_node("/tmp/docs/private", &Node::directory(0, 0, 0o700));
        c.set_node("/home/alice/docs", &Node::directory(1000, 1000, 0o755));
        c.set_node(
            "/home/alice/docs/private",
            &Node::directory(1000, 1000, 0o755),
        );
        for leaf in [true, false] {
            if leaf {
                c.set_node("/tmp/docs/private/probe.md", &file(0, 0o644, "probe"));
                c.set_node("/home/alice/docs/private/probe.md", &file(1000, 0o644, "p"));
            } else {
                c.delete_node("/tmp/docs/private/probe.md");
            }
            for replace in [true, false] {
                assert_eq!(
                    c.copy(&bob, "/home/alice/docs", "/tmp/docs", true, replace),
                    Err("Permission denied: cannot traverse /tmp/docs/private".to_string())
                );
            }
        }
    }
    #[test]
    fn ids_and_accounts() {
        let mut c = store();
        assert_eq!(c.next_id(Id::User), 1000);
        assert_eq!(c.next_id(Id::User), 1001);
        assert_eq!(c.next_id(Id::Group), 1000);
        let mut alice = c.user("root").unwrap();
        alice.id = 1000;
        c.set_user("alice", &alice);
        c.delete_user("alice");
        c.set_user("ally", &alice);
        assert_eq!(c.user_by_id(1000).unwrap().0, "ally");
        assert_eq!(c.username(GUEST), "guest");
        assert_eq!(c.groupname(27), "wheel");
        assert_eq!(c.groupname(4242), "4242");
    }
}
