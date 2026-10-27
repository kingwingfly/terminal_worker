//! Permissions: chmod, chown, chgrp, getfacl, setfacl, getfattr and setfattr.

use clap::Parser;

use super::{Manual, Section};

/// change permissions
///
/// MODE is octal (640, 2775, 1777) or symbolic: u, g, o or a, then +, - or =, then r, w, x, X (x
/// for directories and what is already executable), s (setuid for u, setgid for g) or t
/// (sticky); clauses join with commas (u=rw,go=).
///
/// Directory r lists, w changes entries, x lets through. File r reads, w writes; x is kept and
/// shown, but nothing runs. setgid on a directory gives what is made in it the directory's group,
/// and the bit to new directories. Sticky on a directory lets only an entry's owner, the
/// directory's owner or root remove or rename it, as on /guest. As GNU chmod does, a short octal
/// mode keeps a directory's setuid and setgid bits; five digits (00755) clear them.
///
/// With an ACL, the group bits are its mask, which bounds every named entry. A link has no mode:
/// chmod changes what it points to, and -R passes over links.
#[derive(Parser, Debug)]
#[command(name = "chmod")]
pub struct Chmod {
    /// everything below too
    #[arg(short = 'R', long = "recursive")]
    pub recursive: bool,
    #[arg(value_name = "MODE", allow_hyphen_values = true)]
    pub mode: String,
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<String>,
}
impl Manual for Chmod {
    const SECTION: Section = Section::Permissions;
    const NEEDS: &'static str = "to own PATH, or be root. Without root, setgid needs the file's group to be one of yours, or is dropped.";
    const EXAMPLES: &'static [(&'static str, &'static str)] = &[
        ("chmod 700 ~/notes", "yours alone"),
        ("chmod o+r report.pdf", "others may read"),
        ("chmod -R u+rwX,go-rwx ~/private", "a tree"),
        ("chmod g+s team", "new files take the group"),
    ];
    const SEE: &'static [&'static str] = &["chown", "setfacl", "ls"];
}

/// change owner and group
///
/// Root may change the owner and group. An owner may change only the group, to one of their own.
/// Changing who owns a file clears its setuid bit, and setgid if its group may execute it.
/// Without -h, a link's file changes; with -h, or met below with -R, the link itself.
#[derive(Parser, Debug)]
#[command(name = "chown")]
pub struct Chown {
    /// everything below too
    #[arg(short = 'R', long = "recursive")]
    pub recursive: bool,
    /// the link itself
    #[arg(short = 'h', long = "no-dereference")]
    pub no_dereference: bool,
    #[arg(value_name = "USER[:GROUP]")]
    pub owner: String,
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<String>,
}
impl Manual for Chown {
    const SECTION: Section = Section::Permissions;
    const NEEDS: &'static str = "root, or owning PATH and being in GROUP.";
    const EXAMPLES: &'static [(&'static str, &'static str)] = &[
        ("chown alice:research /shared", "owner and group"),
        ("chown -R :research ~/notes", "group only"),
    ];
    const SEE: &'static [&'static str] = &["chgrp", "chmod"];
}

/// change group
///
/// Gives PATH the group GROUP, by name or number: chown :GROUP PATH. Accounts guests make are in
/// group guest, so chgrp guest shares a file with every guest by its group bits.
#[derive(Parser, Debug)]
#[command(name = "chgrp")]
pub struct Chgrp {
    /// everything below too
    #[arg(short = 'R', long = "recursive")]
    pub recursive: bool,
    /// the link itself
    #[arg(short = 'h', long = "no-dereference")]
    pub no_dereference: bool,
    #[arg(value_name = "GROUP")]
    pub group: String,
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<String>,
}
impl Manual for Chgrp {
    const SECTION: Section = Section::Permissions;
    const NEEDS: &'static str = "root, or owning PATH and being in GROUP.";
    const EXAMPLES: &'static [(&'static str, &'static str)] = &[
        ("chgrp guest notes.md", "share with guests"),
        ("chgrp -R team project", "a tree"),
    ];
    const SEE: &'static [&'static str] = &["chown", "chmod"];
}

/// show access control lists
///
/// Shows each FILE's ACL as getfacl does: a header (file, owner, group, flags for setuid, setgid
/// and sticky), then user::, user:NAME:, group::, group:NAME:, mask:: and other:: entries, and a
/// directory's default: entries. An entry the mask limits shows its #effective permissions.
///
/// Without an ACL beyond the mode, the three base entries are the mode.
#[derive(Parser, Debug)]
#[command(name = "getfacl")]
pub struct Getfacl {
    /// access ACL only
    #[arg(short = 'a', long = "access")]
    pub access: bool,
    /// default ACL only
    #[arg(short = 'd', long = "default")]
    pub default: bool,
    /// no header
    #[arg(short = 'c', long = "omit-header")]
    pub omit_header: bool,
    /// numbers, not names
    #[arg(short = 'n', long = "numeric")]
    pub numeric: bool,
    /// keep the leading /
    #[arg(short = 'p', long = "absolute-names")]
    pub absolute: bool,
    /// everything below too
    #[arg(short = 'R', long = "recursive")]
    pub recursive: bool,
    /// a link itself
    #[arg(short = 'P', long = "physical")]
    pub physical: bool,
    #[arg(required = true, value_name = "FILE")]
    pub files: Vec<String>,
}
impl Manual for Getfacl {
    const SECTION: Section = Section::Permissions;
    const NEEDS: &'static str = "x on the directories on the way.";
    const EXAMPLES: &'static [(&'static str, &'static str)] = &[
        ("getfacl report.md", "one file"),
        ("getfacl -d /guest/team", "what new files will get"),
    ];
    const SEE: &'static [&'static str] = &["setfacl", "ls", "chmod"];
}

/// change access control lists
///
/// Grants named users and groups permissions beyond the owner, group and other classes, as POSIX
/// ACLs do. ENTRIES are comma-separated: u:NAME:rwx, g:NAME:r-x, u::rw, g::r, o::-, m::rx (the
/// mask), each with d: for a directory's default ACL; permissions are letters from rwxX- or a
/// digit.
///
/// Access is decided as on Linux: the owner by the owner's bits; a named user by that entry; then
/// the owning group and named groups you are in, any of which may allow; else others. Every entry
/// but the owner's and others' is bounded by the mask, which setfacl recomputes as the union of
/// the group entries unless one is given or -n.
///
/// A directory's default ACL is given to what is made in it, instead of the umask: new files
/// and directories take it, and new directories keep it. ls -l marks an ACL with +.
#[derive(Parser, Debug)]
#[command(
    name = "setfacl",
    override_usage = "setfacl [-d] [-R] [-n|--mask] {-m|-x|--set} ENTRIES FILE...\n       setfacl [-R] {-b|-k} FILE...",
    group(clap::ArgGroup::new("change").required(true).multiple(true).args(["modify", "remove", "set", "remove_all", "remove_default", "modify_file", "remove_file"]))
)]
pub struct Setfacl {
    /// add or change entries
    #[arg(short = 'm', long = "modify", value_name = "ENTRIES")]
    pub modify: Option<String>,
    /// remove entries (names only, no permissions)
    #[arg(short = 'x', long = "remove", value_name = "ENTRIES")]
    pub remove: Option<String>,
    /// replace the whole ACL
    #[arg(long = "set", value_name = "ENTRIES")]
    pub set: Option<String>,
    /// entries to add, from a file (not supported here)
    #[arg(short = 'M', long = "modify-file", value_name = "FILE", hide = true)]
    pub modify_file: Option<String>,
    /// entries to remove, from a file (not supported here)
    #[arg(short = 'X', long = "remove-file", value_name = "FILE", hide = true)]
    pub remove_file: Option<String>,
    /// remove all extended entries
    #[arg(short = 'b', long = "remove-all")]
    pub remove_all: bool,
    /// remove the default ACL
    #[arg(short = 'k', long = "remove-default")]
    pub remove_default: bool,
    /// the entries are default ones
    #[arg(short = 'd', long = "default")]
    pub default: bool,
    /// everything below too
    #[arg(short = 'R', long = "recursive")]
    pub recursive: bool,
    /// do not recompute the mask
    #[arg(short = 'n', long = "no-mask")]
    pub no_mask: bool,
    /// recompute the mask (the default)
    #[arg(long = "mask")]
    pub mask: bool,
    #[arg(required = true, value_name = "FILE")]
    pub files: Vec<String>,
}
impl Manual for Setfacl {
    const SECTION: Section = Section::Permissions;
    const NEEDS: &'static str = "to own FILE, or be root.";
    const EXAMPLES: &'static [(&'static str, &'static str)] = &[
        ("setfacl -m u:bob:rw notes.md", "bob may read and write"),
        (
            "setfacl -m d:g:team:rwx,g:team:rwx shared",
            "the team, now and for new files",
        ),
        ("setfacl -x u:bob notes.md", "take it back"),
        ("setfacl -b notes.md", "back to the mode"),
    ];
    const SEE: &'static [&'static str] = &["getfacl", "chmod", "chgrp"];
}

/// show extended attributes
///
/// Shows the names of each FILE's extended attributes, user.* ones (root sees trusted.* too); -d
/// shows their values, -n one attribute. Values are written as text in quotes, or as -e says.
#[derive(Parser, Debug)]
#[command(name = "getfattr")]
pub struct Getfattr {
    /// names and values
    #[arg(short = 'd', long = "dump")]
    pub dump: bool,
    /// one attribute
    #[arg(short = 'n', long = "name", value_name = "NAME")]
    pub name: Option<String>,
    /// names matching a regular expression (- for all)
    #[arg(
        short = 'm',
        long = "match",
        value_name = "PATTERN",
        allow_hyphen_values = true
    )]
    pub pattern: Option<String>,
    /// text, hex (0x...) or base64 (0s...)
    #[arg(short = 'e', long = "encoding", value_name = "ENC", value_parser = ["text", "hex", "base64"])]
    pub encoding: Option<String>,
    /// a link itself
    #[arg(short = 'h', long = "no-dereference")]
    pub no_dereference: bool,
    /// everything below too
    #[arg(short = 'R', long = "recursive")]
    pub recursive: bool,
    /// keep the leading /
    #[arg(long = "absolute-names")]
    pub absolute: bool,
    #[arg(required = true, value_name = "FILE")]
    pub files: Vec<String>,
}
impl Manual for Getfattr {
    const SECTION: Section = Section::Permissions;
    const NEEDS: &'static str = "r on FILE.";
    const EXAMPLES: &'static [(&'static str, &'static str)] = &[
        ("getfattr -d report.md", "every user. attribute"),
        ("getfattr -n user.status report.md", "one"),
    ];
    const SEE: &'static [&'static str] = &["setfattr"];
}

/// set extended attributes
///
/// Sets, or with -x removes, an extended attribute: user.NAME for anyone who may write FILE (not
/// on links, and on a sticky directory only its owner), trusted.NAME for root. VALUE is text,
/// "quoted text" with \ escapes, 0x and hexadecimal, or 0s and base64; up to 4 KB of attributes a
/// file. They stay with the file through mv and its other names, and cp -a copies them.
#[derive(Parser, Debug)]
#[command(
    name = "setfattr",
    override_usage = "setfattr -n NAME [-v VALUE] [-h] FILE...\n       setfattr -x NAME [-h] FILE...",
    group(clap::ArgGroup::new("attribute").required(true).args(["name", "remove"]))
)]
pub struct Setfattr {
    /// the attribute to set
    #[arg(short = 'n', long = "name", value_name = "NAME")]
    pub name: Option<String>,
    /// its value (empty without)
    #[arg(
        short = 'v',
        long = "value",
        value_name = "VALUE",
        allow_hyphen_values = true,
        requires = "name"
    )]
    pub value: Option<String>,
    /// the attribute to remove
    #[arg(short = 'x', long = "remove", value_name = "NAME")]
    pub remove: Option<String>,
    /// a link itself
    #[arg(short = 'h', long = "no-dereference")]
    pub no_dereference: bool,
    #[arg(required = true, value_name = "FILE")]
    pub files: Vec<String>,
}
impl Manual for Setfattr {
    const SECTION: Section = Section::Permissions;
    const NEEDS: &'static str = "w on FILE for user.*; root for trusted.*.";
    const EXAMPLES: &'static [(&'static str, &'static str)] = &[
        (
            "setfattr -n user.status -v reviewed report.md",
            "tag a file",
        ),
        ("setfattr -x user.status report.md", "untag it"),
    ];
    const SEE: &'static [&'static str] = &["getfattr"];
}
