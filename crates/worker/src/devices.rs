//! /dev (owner's request, 2026-10-28): presentations others follow, and live sound others hear.
//! `present FILE` makes /dev/present/NAME, a symbolic link to the deck or PDF it shows, owned by
//! the presenter; `stream` makes /dev/snd/NAME, whose sound goes live through Cloudflare Realtime
//! (sound.rs): a link to the recording `stream FILE` makes, or, kept nowhere (`stream -n NAME`),
//! a file of its own. Whoever may read the file follows or hears it (`open /dev/present/NAME`,
//! `open /dev/snd/NAME`), guests too: their pages hear each slide the presenter's account shows
//! (store.rs heard), and a presentation's listeners hear the sound of the same name with it.
//! /dev and its directories are root's, there from the start (`ensure`); only the authority
//! writes in them.
use crate::auth;
use crate::db::{Db, Device};
use serde_json::{Value, json};
use terminal_worker_core::acl::{self, Spec};
use terminal_worker_core::devices::{PRESENT, SOUND, default_name, device_path};
use terminal_worker_core::{CHUNK_SIZE, kind, permissions::*};

/// How long a presentation lasts after its last turn, in seconds, unless stopped before.
pub const PRESENT_TTL: u64 = 12 * 60 * 60;
/// How long a live sound lasts unheard from its page (sound.mjs keeps it every half minute).
pub const SOUND_IDLE: u64 = 2 * 60;

/// Make /dev and its directories, root's, where they are not: they are there from the start
/// (store.rs), and made again if removed.
pub fn ensure(db: &mut Db) {
    for directory in ["/dev", PRESENT, SOUND] {
        if db.node(directory).is_none() {
            db.set_node(directory, &Node::directory(0, 0, 0o755));
        }
    }
}

/// Present the file at `path` as `who`, as the device `name` (the file's by default): it must be
/// a deck, Markdown or a PDF that `who` may read, and `who` someone, not guest, whom everyone
/// signed out is. A presentation of the same name that `who` (or root) had is taken over.
pub fn start(db: &mut Db, who: &Identity, path: &str, name: Option<&str>) -> Result<Value, String> {
    if who.id == GUEST {
        return Err("Permission denied: sign in to present; guest is everyone signed out, who could all turn your slides".into());
    }
    if !matches!(kind(path), "slides" | "markdown" | "pdf") {
        return Err(format!(
            "{path}: present shows slides (*.slides.md or Markdown) and PDFs"
        ));
    }
    let real = db.physical(who, path, true)?;
    let node = db.access(who, &real, READ)?;
    if node.directory {
        return Err(format!("{path}: This is a directory"));
    }
    let device = device_path(
        PRESENT,
        &name.map_or_else(|| default_name(path), str::to_string),
    )?;
    free(db, who, &device)?;
    let link = Node::symlink(who.id, who.group, path);
    make(db, who, &device, link, path, None);
    // Where it is, from the start: where the account showed it last, or its first slide.
    let place = db.place(who.id, path).unwrap_or((0, 0));
    db.set_place(who.id, path, place);
    Ok(json!({ "device": device, "place": place }))
}

/// The live sound /dev/snd/NAME that `who` may make: of the recording at `path`, one `who` is
/// making (by default, its name), or kept nowhere, as NAME; `who` not guest.
pub fn sound_name(
    db: &Db,
    who: &Identity,
    path: Option<&str>,
    name: Option<&str>,
) -> Result<String, String> {
    if who.id == GUEST {
        return Err(
            "Permission denied: sign in to send live sound; guest is everyone signed out".into(),
        );
    }
    if let Some(path) = path {
        let real = db.physical(who, path, true)?;
        let node = db.access(who, &real, READ)?;
        let recording = node.file.as_deref().and_then(|id| db.file(id));
        if !recording.is_some_and(|file| file.recording() && file.uploader == who.id) {
            return Err(format!("{path}: not a recording this account is making"));
        }
    }
    let name = match (name, path) {
        (Some(name), _) => name.to_string(),
        (None, Some(path)) => default_name(path),
        (None, None) => return Err("Name the live sound: stream -n NAME".into()),
    };
    let device = device_path(SOUND, &name)?;
    free(db, who, &device)?;
    Ok(device)
}

/// Make the live sound `device`, which `media` hears (sound.rs): of the recording at `path`, a
/// link to it, whose permissions say who may hear it; or, kept nowhere, a file of its own,
/// `who`'s, made as a file is (the umask), with the ACL `entries` (setfacl -m) given.
pub fn start_sound(
    db: &mut Db,
    who: &Identity,
    path: Option<&str>,
    device: &str,
    media: &str,
    entries: Option<&str>,
) -> Result<Value, String> {
    if sound_name(db, who, path, Some(device))? != device {
        return Err(format!("{device}: Invalid device name"));
    }
    let media = Some(media.to_string());
    match path {
        // Linked to the recording where it is, as its upload knows it (`File::target`, the
        // path it shows at), so that its end finds the device, also through a link.
        Some(path) => {
            let real = db.physical(who, path, true)?;
            let link = Node::symlink(who.id, who.group, &real);
            make(db, who, device, link, &real, media);
        }
        None => {
            let content = db.create_file(0, CHUNK_SIZE, None, who.id, device, None, auth::now());
            db.finish_file(&content);
            ensure(db);
            let directory = db.node(SOUND).ok_or("/dev/snd does not exist")?;
            let mut own = created(who, &directory, 0o666, false, UMASK);
            own.file = Some(content);
            make(db, who, device, own, device, media);
            if let Some(entries) = entries {
                grant(db, who, device, entries)?;
            }
        }
    }
    let version = db.device(device).map(|made| version(&made));
    Ok(json!({ "device": device, "version": version }))
}

/// Give the node at `path`, `who`'s, the ACL entries `entries`, as setfacl -m does: what a new
/// recording, or a live sound kept nowhere, is made with (stream -m).
pub fn grant(db: &mut Db, who: &Identity, path: &str, entries: &str) -> Result<(), String> {
    let specs = {
        let user = |name: &str| db.user(name).map(|u| u.id);
        let group = |name: &str| db.group(name);
        acl::parse_specs(entries, true, &user, &group).map_err(|e| format!("stream -m: {e}"))?
    };
    if specs.iter().any(|spec| spec.default) {
        return Err("stream -m: default entries (d:) are a directory's".into());
    }
    let mut node = db
        .node(path)
        .ok_or_else(|| format!("{path}: does not exist"))?;
    if node.owner != who.id && who.id != 0 {
        return Err(format!("stream -m: {path}: Operation not permitted"));
    }
    let mut current = node.entries();
    let specs: Vec<&Spec> = specs.iter().collect();
    acl::modify(&mut current, &specs, node.mode & 0o111 != 0, false);
    node.set_entries(&current);
    db.set_node(path, &node);
    Ok(())
}

/// Whether `who` may make the device `device`: it is free, or `who`'s (or root's), or its
/// account is no more.
fn free(db: &Db, who: &Identity, device: &str) -> Result<(), String> {
    match (db.device(device), db.node(device)) {
        (Some(other), _)
            if other.owner != who.id && who.id != 0 && db.user_by_id(other.owner).is_some() =>
        {
            Err(format!(
                "{device}: Device or resource busy: it is another account's; name yours with -n"
            ))
        }
        (None, Some(_)) => Err(format!("{device}: File exists")),
        _ => Ok(()),
    }
}

/// Make the device `device`, the node `node`, `who`'s, for the file at `path` (its own path, for
/// a live sound kept nowhere), with what its live sound is heard by, if it is one; it takes over
/// one of that name, which `free` said `who` may. A live sound lasts while its page is heard
/// (`SOUND_IDLE`); a presentation, until stopped or long unturned.
fn make(db: &mut Db, who: &Identity, device: &str, node: Node, path: &str, media: Option<String>) {
    // Taken over to show another file, as another account, or another sound: its followers
    // follow it again, as what they followed is not what it is.
    if db
        .device(device)
        .is_some_and(|old| old.path != path || old.owner != who.id || old.media != media)
    {
        db.replaced_device(device);
    }
    if db.node(device).is_some() {
        db.delete_node(device);
    }
    ensure(db);
    db.set_node(device, &node);
    let lasts = if media.is_some() {
        SOUND_IDLE
    } else {
        PRESENT_TTL
    };
    db.set_device(
        &Device {
            name: device.to_string(),
            owner: who.id,
            path: path.to_string(),
            media,
        },
        auth::now(),
    );
    db.set_expiry("device", device, auth::now() + lasts);
}

/// Keep the live sound `name` of `directory` going, as its page says it still sends it, while it
/// is still that one (`version`).
pub fn keep(db: &mut Db, who: &Identity, name: &str, version: &str) -> Result<Value, String> {
    let device = device_path(SOUND, name)?;
    match db.device(&device) {
        Some(made) if made.owner == who.id && self::version(&made) == version => {
            db.set_expiry("device", &device, auth::now() + SOUND_IDLE);
            Ok(json!({ "device": device }))
        }
        _ => Err(format!("{device}: No such device; it does not exist")),
    }
}

/// Stop the device `name` of `directory`, a presentation or a live sound, as its account or
/// root; given a `version` (`version`), only while it is still that one: a page ending the
/// sound it sent ends nothing that another page of the account has taken the name over with.
pub fn stop(
    db: &mut Db,
    who: &Identity,
    directory: &str,
    name: &str,
    version: Option<&str>,
) -> Result<Value, String> {
    let device = device_path(directory, name)?;
    let Some(made) = db.device(&device) else {
        return Err(format!("{device}: No such device; it does not exist"));
    };
    if made.owner != who.id && who.id != 0 {
        return Err(format!(
            "{device}: Permission denied: it is another account's"
        ));
    }
    if version.is_some_and(|version| version != self::version(&made)) {
        return Ok(json!({ "device": device, "kept": true }));
    }
    end(db, &device);
    Ok(json!({ "device": device }))
}

/// Remove the device `name`: its node (a link, or a live sound's own file), its row, its
/// deadline; its followers hear so.
pub fn end(db: &mut Db, name: &str) {
    if db.node(name).is_some_and(|node| !node.directory) {
        db.delete_node(name);
    }
    db.delete_device(name);
    db.clear_expiry("device", name);
}

/// The presentation at `name`, as `who` follows it: the file it shows, of what kind, whose it
/// is, whether `who` turns it too (its presenter's account), where it is, and its sound, the
/// live sound of the same name by the same account, if `who` may hear it. Following needs read
/// permission on the file.
pub fn attend(db: &Db, who: &Identity, name: &str) -> Result<Value, String> {
    let device = followed(db, who, name)?;
    let owner = db.username(device.owner);
    let base = device.name.rsplit('/').next().unwrap_or_default();
    let sound = followed(db, who, &format!("{SOUND}/{base}"))
        .ok()
        .filter(|sound| sound.owner == device.owner && sound.media.is_some())
        .map(|sound| sound.name);
    Ok(json!({
        "device": device.name, "path": device.path, "kind": kind(&device.path), "owner": owner,
        "controller": who.id == device.owner, "place": db.place(device.owner, &device.path),
        "sound": sound, "version": version(&device),
    }))
}

/// What a device is, as an opaque word that changes exactly when a takeover replaces it (`make`):
/// its account, the file it shows, its sound. A page compares the one it followed with the one
/// it is told on following again, to know whether what it shows is still that device's.
pub fn version(device: &Device) -> String {
    use sha2::{Digest, Sha256};
    let what = format!(
        "{}\n{}\n{}",
        device.owner,
        device.path,
        device.media.as_deref().unwrap_or_default()
    );
    Sha256::digest(what.as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The device `name`, if `who` may follow or hear it: it exists, its node is still there, `who`
/// may search its directory and read what decides who may: the file it links to, or, for a live
/// sound kept nowhere, itself.
pub fn followed(db: &Db, who: &Identity, name: &str) -> Result<Device, String> {
    let missing = || format!("{name}: No such device; it does not exist");
    let device = db.device(name).ok_or_else(missing)?;
    let node = db.node(name).ok_or_else(missing)?;
    let own = device.path == device.name;
    let there = if own {
        !node.is_link() && !node.directory
    } else {
        node.link.as_ref() == Some(&device.path)
    };
    if !there {
        return Err(missing());
    }
    let directory = terminal_worker_core::parent(name);
    db.access(who, &db.physical(who, directory, true)?, EXEC)?;
    let real = if own {
        device.name.clone()
    } else {
        db.physical(who, &device.path, true)?
    };
    db.access(who, &real, READ)?;
    Ok(device)
}
