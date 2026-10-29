//! The commands the server runs, each a [`ServerCommand`] registered by name in [`COMMANDS`],
//! a module a section: files, permissions, accounts and archives. Their arguments, and their
//! help, are their definitions in [`terminal_worker_core::cli`].

pub(crate) mod accounts;
mod archives;
mod files;

use crate::db::Db;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use terminal_worker_core::{Entry, permissions::*, resolve};

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

/// `name` in the directory `dir`.
pub fn joined(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
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

/// A command the server runs.
pub trait ServerCommand: Sync {
    /// Its name, as [`terminal_worker_core::cli`] defines it.
    fn name(&self) -> &'static str;
    /// Run it, as `ctx.who`: what it writes, with what it gathers in `ctx` on the way.
    fn run(&self, ctx: &mut Context) -> Result<String, String>;
}

/// What a command runs with, and gathers.
pub struct Context<'a> {
    pub db: &'a mut Db,
    /// Who it acts as: root under sudo.
    pub who: &'a Identity,
    /// The account signed in, whose a job is.
    pub owner: u32,
    pub command: &'a Command,
    /// Its arguments, after its name.
    pub args: &'a [String],
    /// What failed of what a command does several of (paths, mostly): each is reported, and
    /// what did not fail is kept.
    pub errors: Vec<String>,
    /// Whether any of those succeeded.
    pub succeeded: bool,
    /// A listing, and whether it is long, which the terminal shows as a view.
    pub listing: Option<(Vec<Entry>, bool)>,
    /// fd's results, one by one.
    pub results: Option<Vec<String>>,
    /// The files mv and cp -i did not replace, which the terminal asks about.
    pub existing: Vec<Value>,
    /// The whole reply, for a command that answers more than text, as a job does.
    pub reply: Option<Value>,
}
impl Context<'_> {
    /// `value` as a path, a link at its end followed (stat(2)).
    pub fn path(&self, value: &str) -> Result<String, String> {
        command_path(self.db, self.who, self.command, value)
    }
    /// `value` as a name, a link at its end itself (lstat(2)).
    pub fn entry(&self, value: &str) -> Result<String, String> {
        entry_path(self.db, self.who, self.command, value)
    }
    /// Note how one of several things went: what failed is reported, and the rest kept.
    pub fn each<T>(&mut self, result: Result<T, String>) -> Option<T> {
        match result {
            Ok(value) => {
                self.succeeded = true;
                Some(value)
            }
            Err(e) => {
                self.errors.push(e);
                None
            }
        }
    }
}

/// Every command the server runs.
pub static COMMANDS: &[&dyn ServerCommand] = &[
    &files::Cp,
    &accounts::Whoami,
    &accounts::Id,
    &accounts::Groups,
    &accounts::Users,
    &accounts::Useradd,
    &accounts::Usermod,
    &accounts::Userdel,
    &accounts::Groupadd,
    &accounts::Groupmod,
    &accounts::Groupdel,
    &archives::Job("zip"),
    &archives::Job("unzip"),
    &archives::Job("tar"),
    &archives::Job("faststart"),
];

/// The command named `name`, if the server runs it.
pub fn find(name: &str) -> Option<&'static dyn ServerCommand> {
    COMMANDS.iter().copied().find(|c| c.name() == name)
}

/// Run `command` as `who` for the account `owner`.
pub fn execute(db: &mut Db, who: &Identity, owner: u32, command: Command) -> Result<Value, String> {
    let (name, args) = command.args.split_first().ok_or("Missing command")?;
    let Some(runner) = find(name) else {
        return Err(
            "Unsupported server command; sudo supports file and account management commands. Use su root to browse restricted content".into(),
        );
    };
    let mut ctx = Context {
        db,
        who,
        owner,
        command: &command,
        args,
        errors: Vec::new(),
        succeeded: false,
        listing: None,
        results: None,
        existing: Vec::new(),
        reply: None,
    };
    let output = runner.run(&mut ctx)?;
    if let Some(reply) = ctx.reply {
        return Ok(reply);
    }
    if !ctx.errors.is_empty() && !ctx.succeeded && ctx.results.is_none() {
        return Err(ctx.errors.join("\n"));
    }
    let mut value = json!({"output": output});
    if !ctx.errors.is_empty() {
        value["errors"] = json!(ctx.errors);
    }
    if let Some((entries, long)) = ctx.listing {
        value["entries"] = json!(entries);
        value["long"] = json!(long);
    }
    if let Some(results) = ctx.results {
        value["results"] = json!(results);
    }
    if !ctx.existing.is_empty() {
        value["existing"] = json!(ctx.existing);
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
