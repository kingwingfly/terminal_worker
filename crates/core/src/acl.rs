//! POSIX access control lists, as Linux keeps them and setfacl and getfacl show and change them.
//!
//! A node's access ACL is its mode, for the owner (`user::`) and others (`other::`), plus, once
//! extended, the owning group's entry (`group::`) and those of named users and groups. An
//! extended ACL has a mask (`mask::`), which bounds every entry but the owner's and others':
//! the mode's group bits are the mask then, so chmod changes the mask, as on Linux. A directory
//! may also have a default ACL, which what is created in it takes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::permissions::{EXEC, READ, WRITE};

/// The entries of an extended access ACL that the mode does not hold.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acl {
    /// The owning group's permissions, `group::`.
    pub group: u16,
    #[serde(default)]
    pub users: BTreeMap<u32, u16>,
    #[serde(default)]
    pub groups: BTreeMap<u32, u16>,
}

/// A whole ACL, every entry as getfacl lists it: an access ACL taken apart from its mode, or a
/// directory's default ACL, which is kept so.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entries {
    pub user: u16,
    pub group: u16,
    pub other: u16,
    #[serde(default)]
    pub mask: Option<u16>,
    #[serde(default)]
    pub users: BTreeMap<u32, u16>,
    #[serde(default)]
    pub groups: BTreeMap<u32, u16>,
}

impl Entries {
    /// The access ACL of a node with `mode` and extended entries `acl`.
    pub fn access(mode: u16, acl: Option<&Acl>) -> Self {
        let class = (mode >> 3) & 7;
        match acl {
            Some(acl) => Self {
                user: (mode >> 6) & 7,
                group: acl.group,
                other: mode & 7,
                mask: Some(class),
                users: acl.users.clone(),
                groups: acl.groups.clone(),
            },
            None => Self {
                user: (mode >> 6) & 7,
                group: class,
                other: mode & 7,
                ..Self::default()
            },
        }
    }
    /// Whether it says more than a mode can.
    pub fn extended(&self) -> bool {
        !self.users.is_empty() || !self.groups.is_empty() || self.mask.is_some()
    }
    /// The mask that grants what the owning group and the named entries have, as setfacl
    /// recalculates it; none for an ACL without named entries.
    pub fn recalculate_mask(&mut self) {
        if self.users.is_empty() && self.groups.is_empty() {
            self.mask = None;
            return;
        }
        let union = self
            .users
            .values()
            .chain(self.groups.values())
            .fold(self.group, |all, p| all | p);
        self.mask = Some(union);
    }
    /// As the mode's permission bits, and the extended entries, if it is extended. An ACL whose
    /// mask grants the owning group what it has, and names nobody, is a mode.
    pub fn split(&self) -> (u16, Option<Acl>) {
        let folded = self.users.is_empty()
            && self.groups.is_empty()
            && self.mask.is_none_or(|mask| mask == self.group);
        let class = if folded {
            self.group
        } else {
            self.mask.unwrap_or(self.group)
        };
        let mode = (self.user << 6) | (class << 3) | self.other;
        let acl = (!folded).then(|| Acl {
            group: self.group,
            users: self.users.clone(),
            groups: self.groups.clone(),
        });
        (mode, acl)
    }
    /// What the entry for a named user or group, or the owning group, grants under the mask.
    pub fn effective(&self, perms: u16) -> u16 {
        perms & self.mask.unwrap_or(7)
    }
}

/// What a node created in a directory with the default ACL `default` gets, asked for with
/// `requested` permission bits: its mode's bits and access ACL, the umask playing no part, as
/// POSIX says. The requested bits bound the owner's, the mask's (or the group's, without one)
/// and others' entries.
pub fn inherited(default: &Entries, requested: u16) -> (u16, Option<Acl>) {
    let mut entries = default.clone();
    entries.user &= (requested >> 6) & 7;
    entries.other &= requested & 7;
    match &mut entries.mask {
        Some(mask) => *mask &= (requested >> 3) & 7,
        None => entries.group &= (requested >> 3) & 7,
    }
    entries.split()
}

/// The tag of an ACL entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tag {
    /// The owner, without an id, or a named user.
    User(Option<u32>),
    /// The owning group, without an id, or a named group.
    Group(Option<u32>),
    Mask,
    Other,
}

/// Permissions in an entry setfacl is given: `X` is execute only for a directory, or for what
/// some class may already execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perms {
    pub bits: u16,
    pub conditional_exec: bool,
}

/// An entry setfacl is given: `[d[efault]:]u[ser]:[NAME][:PERMS]`, and so on for `g[roup]`,
/// `m[ask]` and `o[ther]`; without its permissions, for -x.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub default: bool,
    pub tag: Tag,
    pub perms: Option<Perms>,
}

/// Parse `rwx`, `r-x`, `rX`, `-` or an octal digit.
pub fn parse_perms(text: &str) -> Result<Perms, String> {
    if let [digit @ b'0'..=b'7'] = text.as_bytes() {
        return Ok(Perms {
            bits: u16::from(digit - b'0'),
            conditional_exec: false,
        });
    }
    let mut perms = Perms {
        bits: 0,
        conditional_exec: false,
    };
    for c in text.chars() {
        match c {
            'r' => perms.bits |= READ,
            'w' => perms.bits |= WRITE,
            'x' => perms.bits |= EXEC,
            'X' => perms.conditional_exec = true,
            '-' => {}
            _ => return Err(format!("Invalid permissions: {text}")),
        }
    }
    Ok(perms)
}

/// Parse the entries of `text`, separated by commas or new lines, naming users and groups by
/// name or number through `user` and `group`. `perms` says whether the entries carry
/// permissions, as for -m and --set, or not, as for -x.
pub fn parse_specs(
    text: &str,
    perms: bool,
    user: &dyn Fn(&str) -> Option<u32>,
    group: &dyn Fn(&str) -> Option<u32>,
) -> Result<Vec<Spec>, String> {
    let mut specs = Vec::new();
    for entry in text
        .split([',', '\n'])
        .map(str::trim)
        .filter(|e| !e.is_empty() && !e.starts_with('#'))
    {
        let invalid = || format!("Invalid ACL entry: {entry}");
        let mut fields: Vec<&str> = entry.split(':').collect();
        let default = matches!(fields.first(), Some(&"d" | &"default"));
        if default {
            fields.remove(0);
        }
        let (kind, qualifier, rest): (&str, &str, Vec<&str>) = match fields.as_slice() {
            [kind, qualifier, rest @ ..] => (kind, qualifier, rest.to_vec()),
            // m and o may leave out their empty qualifier: m:rw, o:r, o::r.
            [kind] if !perms => (kind, "", Vec::new()),
            _ => return Err(invalid()),
        };
        let (tag, rest) = match kind {
            "u" | "user" => (
                Tag::User(id_of(qualifier, user).map_err(|e| format!("{e}: {entry}"))?),
                rest,
            ),
            "g" | "group" => (
                Tag::Group(id_of(qualifier, group).map_err(|e| format!("{e}: {entry}"))?),
                rest,
            ),
            // m:rwx and m::rwx alike.
            "m" | "mask" | "o" | "other" => {
                let tag = if kind.starts_with('m') {
                    Tag::Mask
                } else {
                    Tag::Other
                };
                match (qualifier, rest.as_slice()) {
                    ("", _) => (tag, rest),
                    (given, []) => (tag, vec![given]),
                    _ => return Err(invalid()),
                }
            }
            _ => return Err(invalid()),
        };
        let perms = match (perms, rest.as_slice()) {
            (true, [text]) => Some(parse_perms(text)?),
            (false, []) | (false, [""]) => None,
            _ => return Err(invalid()),
        };
        specs.push(Spec {
            default,
            tag,
            perms,
        });
    }
    Ok(specs)
}

fn id_of(name: &str, lookup: &dyn Fn(&str) -> Option<u32>) -> Result<Option<u32>, String> {
    if name.is_empty() {
        return Ok(None);
    }
    lookup(name)
        .or_else(|| name.parse().ok())
        .map(Some)
        .ok_or_else(|| "Invalid user or group name".to_string())
}

/// `rwx`, `r-x`, ... for `perms`.
pub fn perms_text(perms: u16) -> String {
    [(READ, 'r'), (WRITE, 'w'), (EXEC, 'x')]
        .iter()
        .map(|&(bit, c)| if perms & bit != 0 { c } else { '-' })
        .collect()
}

/// Change `entries` as `specs` say, with `allowing_exec` for `X`: setfacl -m. The mask is
/// recalculated unless one is given, or `keep_mask` (-n).
pub fn modify(entries: &mut Entries, specs: &[&Spec], allowing_exec: bool, keep_mask: bool) {
    let mut masked = false;
    for spec in specs {
        let Some(perms) = spec.perms else { continue };
        let bits = perms.bits
            | if perms.conditional_exec && allowing_exec {
                EXEC
            } else {
                0
            };
        match spec.tag {
            Tag::User(None) => entries.user = bits,
            Tag::User(Some(id)) => {
                entries.users.insert(id, bits);
            }
            Tag::Group(None) => entries.group = bits,
            Tag::Group(Some(id)) => {
                entries.groups.insert(id, bits);
            }
            Tag::Mask => {
                entries.mask = Some(bits);
                masked = true;
            }
            Tag::Other => entries.other = bits,
        }
    }
    // Recalculated unless given or kept; an ACL that names someone needs one regardless.
    let needed =
        entries.mask.is_none() && (!entries.users.is_empty() || !entries.groups.is_empty());
    if !masked && !keep_mask || needed {
        entries.recalculate_mask();
    }
}

/// Remove the named entries, or the mask, `specs` name: setfacl -x.
pub fn remove(entries: &mut Entries, specs: &[&Spec], keep_mask: bool) -> Result<(), String> {
    let mut masked = false;
    for spec in specs {
        match spec.tag {
            Tag::User(Some(id)) => {
                entries.users.remove(&id);
            }
            Tag::Group(Some(id)) => {
                entries.groups.remove(&id);
            }
            Tag::Mask => {
                entries.mask = None;
                masked = true;
            }
            _ => {
                return Err(
                    "The owner's, the owning group's and others' entries cannot be removed".into(),
                );
            }
        }
    }
    if masked && (!entries.users.is_empty() || !entries.groups.is_empty()) {
        return Err("An ACL with named entries needs its mask".into());
    }
    if !masked && !keep_mask {
        entries.recalculate_mask();
    }
    Ok(())
}

/// Replace `entries` with `specs`, which must give the owner's, the owning group's and others'
/// entries: setfacl --set.
pub fn set(specs: &[&Spec], allowing_exec: bool) -> Result<Entries, String> {
    for (tag, name) in [
        (Tag::User(None), "user::"),
        (Tag::Group(None), "group::"),
        (Tag::Other, "other::"),
    ] {
        if !specs.iter().any(|s| s.tag == tag) {
            return Err(format!("--set needs a {name} entry"));
        }
    }
    let mut entries = Entries::default();
    modify(&mut entries, specs, allowing_exec, false);
    Ok(entries)
}

/// getfacl's lines for `entries`, prefixed `default:` if `default`; with the permissions a mask
/// takes away noted, as `#effective:`.
pub fn lines(
    entries: &Entries,
    default: bool,
    user: &dyn Fn(u32) -> String,
    group: &dyn Fn(u32) -> String,
) -> Vec<String> {
    let prefix = if default { "default:" } else { "" };
    let effective = |text: String, perms: u16| {
        let allowed = entries.effective(perms);
        if entries.mask.is_some() && allowed != perms {
            format!("{text}\t#effective:{}", perms_text(allowed))
        } else {
            text
        }
    };
    let mut out = vec![format!("{prefix}user::{}", perms_text(entries.user))];
    for (id, perms) in &entries.users {
        out.push(effective(
            format!("{prefix}user:{}:{}", user(*id), perms_text(*perms)),
            *perms,
        ));
    }
    out.push(effective(
        format!("{prefix}group::{}", perms_text(entries.group)),
        entries.group,
    ));
    for (id, perms) in &entries.groups {
        out.push(effective(
            format!("{prefix}group:{}:{}", group(*id), perms_text(*perms)),
            *perms,
        ));
    }
    if let Some(mask) = entries.mask {
        out.push(format!("{prefix}mask::{}", perms_text(mask)));
    }
    out.push(format!("{prefix}other::{}", perms_text(entries.other)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(name: &str) -> Option<u32> {
        match name {
            "bob" => Some(1001),
            "team" => Some(2000),
            _ => None,
        }
    }

    #[test]
    fn specs_parse_as_setfacl_reads_them() {
        let specs = parse_specs(
            "u:bob:rw-,g:team:rX,d:user::rwx,m::r,o:-",
            true,
            &names,
            &names,
        )
        .unwrap();
        assert_eq!(specs[0].tag, Tag::User(Some(1001)));
        assert_eq!(specs[0].perms.unwrap().bits, 6);
        assert!(specs[1].perms.unwrap().conditional_exec);
        assert!(specs[2].default && specs[2].tag == Tag::User(None));
        assert_eq!(specs[3].tag, Tag::Mask);
        assert_eq!(
            (specs[4].tag.clone(), specs[4].perms.unwrap().bits),
            (Tag::Other, 0)
        );
        assert_eq!(
            parse_specs("u:1234:5", true, &names, &names).unwrap()[0].tag,
            Tag::User(Some(1234))
        );
        assert!(parse_specs("u:nobody-here:r", true, &names, &names).is_err());
        assert!(parse_specs("u:bob:rwq", true, &names, &names).is_err());
        let removed = parse_specs("u:bob,g:team,m", false, &names, &names).unwrap();
        assert_eq!(removed.len(), 3);
    }

    #[test]
    fn the_mask_bounds_named_entries_and_chmod_moves_it() {
        // -rw-r--r--, then setfacl -m u:bob:rw: the mask grows to rw, as the group bits show.
        let mut entries = Entries::access(0o644, None);
        let specs = parse_specs("u:bob:rw", true, &names, &names).unwrap();
        modify(
            &mut entries,
            &specs.iter().collect::<Vec<_>>(),
            false,
            false,
        );
        let (mode, acl) = entries.split();
        assert_eq!(mode, 0o664);
        let acl = acl.unwrap();
        assert_eq!((acl.group, acl.users[&1001]), (4, 6));
        // chmod 640 sets the mask to r: bob's rw is effectively r.
        let entries = Entries::access(0o640, Some(&acl));
        assert_eq!(entries.effective(entries.users[&1001]), 4);
        let shown = lines(&entries, false, &|id| id.to_string(), &|id| id.to_string());
        assert_eq!(
            shown,
            [
                "user::rw-",
                "user:1001:rw-\t#effective:r--",
                "group::r--",
                "mask::r--",
                "other::---"
            ]
        );
        // Removing the only named entry leaves a mode.
        let mut entries = Entries::access(0o664, Some(&acl));
        let specs = parse_specs("u:bob", false, &names, &names).unwrap();
        remove(&mut entries, &specs.iter().collect::<Vec<_>>(), false).unwrap();
        assert_eq!(entries.split(), (0o644, None));
    }

    #[test]
    fn a_default_acl_is_what_new_nodes_take() {
        let default = Entries {
            user: 7,
            group: 5,
            other: 0,
            mask: Some(7),
            users: BTreeMap::from([(1001, 7)]),
            groups: BTreeMap::new(),
        };
        // A file asks for rw-rw-rw-: no execute anywhere, and no umask.
        let (mode, acl) = inherited(&default, 0o666);
        assert_eq!(mode, 0o660);
        assert_eq!(acl.unwrap().users[&1001], 7);
        // Without a mask, the group's entry is what the requested bits bound.
        let plain = Entries {
            user: 7,
            group: 7,
            other: 5,
            ..Entries::default()
        };
        assert_eq!(inherited(&plain, 0o755), (0o755, None));
    }

    #[test]
    fn set_needs_the_base_entries() {
        let all = parse_specs("u::rw,g::r,o::-,u:bob:r", true, &names, &names).unwrap();
        let entries = set(&all.iter().collect::<Vec<_>>(), false).unwrap();
        assert_eq!(entries.mask, Some(4));
        let partial = parse_specs("u::rw,o::-", true, &names, &names).unwrap();
        assert!(set(&partial.iter().collect::<Vec<_>>(), false).is_err());
    }
}
