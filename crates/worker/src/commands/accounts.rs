//! Accounts: whoami, id, groups, users, useradd, usermod, userdel, groupadd, groupmod and
//! groupdel. Those that change accounts are root's; a signed-out useradd NAME goes to
//! /api/register instead, through the terminal's dialog.

use terminal_worker_core::cli::{accounts as args, parse};
use terminal_worker_core::parent;
use terminal_worker_core::permissions::Id as Next;
use terminal_worker_core::permissions::*;

use super::{
    Context, ServerCommand, absolute_home, group, groups, named_group, protect_home, show_id,
};

/// The groups that cannot be renamed or deleted.
const SYSTEM_GROUPS: [&str; 3] = ["root", "wheel", "guest"];

/// Refuse anyone but root.
fn root(ctx: &Context) -> Result<(), String> {
    if ctx.who.id != 0 {
        return Err("Root permission required; use sudo".into());
    }
    Ok(())
}

pub struct Whoami;
impl ServerCommand for Whoami {
    fn name(&self) -> &'static str {
        "whoami"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        parse::<args::Whoami>(ctx.args)?;
        Ok(ctx.db.username(ctx.who.id))
    }
}

/// Who `user` names, or the caller.
fn identity(ctx: &Context, user: &Option<String>) -> Result<Identity, String> {
    Ok(match user {
        Some(name) => Identity::of(&ctx.db.user(name).ok_or("User does not exist")?),
        None => ctx.who.clone(),
    })
}

pub struct Id;
impl ServerCommand for Id {
    fn name(&self) -> &'static str {
        "id"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let id = parse::<args::Id>(ctx.args)?;
        Ok(show_id(ctx.db, &identity(ctx, &id.user)?))
    }
}

pub struct Groups;
impl ServerCommand for Groups {
    fn name(&self) -> &'static str {
        "groups"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let groups = parse::<args::Groups>(ctx.args)?;
        Ok(identity(ctx, &groups.user)?
            .groups
            .iter()
            .map(|g| ctx.db.groupname(*g))
            .collect::<Vec<_>>()
            .join(" "))
    }
}

pub struct Users;
impl ServerCommand for Users {
    fn name(&self) -> &'static str {
        "users"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        parse::<args::Users>(ctx.args)?;
        if ctx.who.id != 0 {
            return Err("Only root can list accounts".into());
        }
        Ok(ctx
            .db
            .users()
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
            .join("\n"))
    }
}

pub struct Useradd;
impl ServerCommand for Useradd {
    fn name(&self) -> &'static str {
        "useradd"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let useradd = parse::<args::Useradd>(ctx.args)?;
        if ctx.who.id == GUEST {
            return Err(
                "useradd: signed out, useradd NAME makes an account of your own; options need root"
                    .into(),
            );
        }
        root(ctx)?;
        let db = &mut *ctx.db;
        let name = useradd.name.as_str();
        if !valid_name(name) || db.user(name).is_some() {
            return Err(
                "Username is invalid or already exists (start with a lowercase letter; up to 32 lowercase letters, digits, underscores, or hyphens)".into(),
            );
        }
        let home = absolute_home(useradd.home.as_deref().unwrap_or(&format!("/home/{name}")))?;
        protect_home(db, &home, u32::MAX)?;
        let extra = groups(db, useradd.groups.as_deref().unwrap_or(""))?;
        let gid = match &useradd.group {
            Some(g) => group(db, g)?,
            None => {
                if db.group(name).is_some() {
                    return Err("Group already exists; use -g to choose the primary group".into());
                }
                let gid = db.next_id(Next::Group);
                db.set_group(name, gid);
                gid
            }
        };
        let uid = db.next_id(Next::User);
        if useradd.create_home {
            if db.node(&home).is_some() {
                return Err("Home already exists; existing content cannot be taken over".into());
            }
            db.mkdir(&Identity::root(), parent(&home), true)?;
            db.set_node(&home, &Node::directory(uid, gid, 0o700));
        }
        db.set_user(
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
        Ok(format!(
            "Created user {name}; use passwd {name} to set a password before signing in"
        ))
    }
}

pub struct Usermod;
impl ServerCommand for Usermod {
    fn name(&self) -> &'static str {
        "usermod"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let usermod = parse::<args::Usermod>(ctx.args)?;
        root(ctx)?;
        let db = &mut *ctx.db;
        let name = usermod.user.as_str();
        if name == "guest" {
            return Err("The guest account cannot be changed".into());
        }
        let mut user = db.user(name).ok_or("User does not exist")?;
        if user.id == 0 {
            return Err(
                "The root identity is fixed; use passwd root to change its password".into(),
            );
        }
        let changes = usermod.groups.is_some()
            || usermod.group.is_some()
            || usermod.home.is_some()
            || usermod.rename.is_some()
            || usermod.lock
            || usermod.unlock;
        if !changes {
            return Err(
                "usermod requires an update option; use passwd to change a password".into(),
            );
        }
        if let Some(g) = &usermod.group {
            user.group = group(db, g)?;
        }
        if let Some(g) = &usermod.groups {
            let new = groups(db, g)?;
            if usermod.append {
                user.groups.extend(new);
            } else {
                user.groups = new;
            }
        }
        if let Some(home) = &usermod.home {
            let home = absolute_home(home)?;
            protect_home(db, &home, user.id)?;
            if usermod.move_home {
                db.move_home(&user.home, &home)?;
            }
            user.home = home;
        }
        if usermod.lock {
            user.locked = true;
        }
        if usermod.unlock {
            if user.hash.is_empty() {
                return Err("Set a password with passwd first".into());
            }
            user.locked = false;
        }
        let new = usermod.rename.as_deref().unwrap_or(name);
        if new != name && (!valid_name(new) || db.user(new).is_some()) {
            return Err("New username is invalid or already exists".into());
        }
        if user.locked {
            db.delete_sessions(user.id);
        }
        db.delete_user(name);
        db.set_user(new, &user);
        Ok(format!("Updated user {new}"))
    }
}

pub struct Userdel;
impl ServerCommand for Userdel {
    fn name(&self) -> &'static str {
        "userdel"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let userdel = parse::<args::Userdel>(ctx.args)?;
        root(ctx)?;
        let db = &mut *ctx.db;
        let name = userdel.user.as_str();
        let user = db.user(name).ok_or("User does not exist")?;
        if user.id == 0 || user.id == GUEST {
            return Err(format!("Cannot delete {name}"));
        }
        if userdel.remove && db.node(&user.home).is_some() {
            protect_home(db, &user.home, user.id)?;
            db.remove(&Identity::root(), &user.home, true)?;
        }
        db.delete_user(name);
        db.delete_sessions(user.id);
        // Its presentations end with it, and their followers hear so; where it showed decks is
        // forgotten, as a new account may take its uid.
        for device in db.devices_of(user.id) {
            crate::devices::end(db, &device);
        }
        db.delete_places(user.id);
        Ok(format!("Deleted user {name}"))
    }
}

pub struct Groupadd;
impl ServerCommand for Groupadd {
    fn name(&self) -> &'static str {
        "groupadd"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let groupadd = parse::<args::Groupadd>(ctx.args)?;
        root(ctx)?;
        let name = groupadd.group.as_str();
        if !valid_name(name) || ctx.db.group(name).is_some() {
            return Err("Group name is invalid or already exists".into());
        }
        let gid = ctx.db.next_id(Next::Group);
        ctx.db.set_group(name, gid);
        Ok(format!("Created group {name}"))
    }
}

pub struct Groupmod;
impl ServerCommand for Groupmod {
    fn name(&self) -> &'static str {
        "groupmod"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let groupmod = parse::<args::Groupmod>(ctx.args)?;
        root(ctx)?;
        let (name, new) = (groupmod.old.as_str(), groupmod.new.as_str());
        if SYSTEM_GROUPS.contains(&name) {
            return Err("Cannot rename a system group".into());
        }
        let gid = named_group(ctx.db, name)?;
        if !valid_name(new) || ctx.db.group(new).is_some() {
            return Err("Group name is invalid or already exists".into());
        }
        ctx.db.delete_group(name);
        ctx.db.set_group(new, gid);
        Ok(format!("Group {name} renamed to {new}"))
    }
}

pub struct Groupdel;
impl ServerCommand for Groupdel {
    fn name(&self) -> &'static str {
        "groupdel"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let groupdel = parse::<args::Groupdel>(ctx.args)?;
        root(ctx)?;
        let name = groupdel.group.as_str();
        if SYSTEM_GROUPS.contains(&name) {
            return Err("Cannot delete a system group".into());
        }
        let gid = named_group(ctx.db, name)?;
        if ctx.db.users().iter().any(|(_, u)| u.group == gid) {
            return Err("This group is still an account's primary group".into());
        }
        ctx.db.delete_group(name);
        for (n, mut u) in ctx.db.users() {
            if u.groups.remove(&gid) {
                ctx.db.set_user(&n, &u);
            }
        }
        Ok(format!("Deleted group {name}"))
    }
}
