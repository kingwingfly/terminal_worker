//! /dev (owner's request, 2026-10-28): presentations others follow, and live sound others hear.
//! `present FILE` makes /dev/present/NAME, a symbolic link to the deck or PDF it shows, owned by
//! the presenter; `stream FILE` makes /dev/snd/NAME, a link to the recording it makes, whose
//! sound goes live through Cloudflare Realtime (sound.rs). Whoever may read the file follows or
//! hears it (`open /dev/present/NAME`, `open /dev/snd/NAME`), guests too: their pages hear each
//! slide the presenter's account shows (store.rs heard), and a presentation's listeners hear the
//! sound of the same name with it. /dev and its directories are root's, made when first needed;
//! only the authority writes in them.
use crate::auth;
use crate::db::{Db, Device};
use serde_json::{Value, json};
use terminal_worker_core::devices::{PRESENT, SOUND, default_name, device_path};
use terminal_worker_core::{kind, permissions::*};

/// How long a presentation lasts after its last turn, in seconds, unless stopped before.
pub const PRESENT_TTL: u64 = 12 * 60 * 60;

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
    make(db, who, &device, path, None)?;
    // Where it is, from the start: where the account showed it last, or its first slide.
    let place = db.place(who.id, path).unwrap_or((0, 0));
    db.set_place(who.id, path, place);
    Ok(json!({ "device": device, "place": place }))
}

/// The live sound /dev/snd/NAME (the recording's name by default) that `who` may make of the
/// recording at `path`: one `who` is making, and `who` not guest.
pub fn sound_name(
    db: &Db,
    who: &Identity,
    path: &str,
    name: Option<&str>,
) -> Result<String, String> {
    if who.id == GUEST {
        return Err(
            "Permission denied: sign in to send live sound; guest is everyone signed out".into(),
        );
    }
    let real = db.physical(who, path, true)?;
    let node = db.access(who, &real, READ)?;
    let recording = node.file.as_deref().and_then(|id| db.file(id));
    if !recording.is_some_and(|file| file.recording() && file.uploader == who.id) {
        return Err(format!("{path}: not a recording this account is making"));
    }
    let device = device_path(
        SOUND,
        &name.map_or_else(|| default_name(path), str::to_string),
    )?;
    free(db, who, &device)?;
    Ok(device)
}

/// Make the live sound `device` of the recording at `path`, which `media` hears (sound.rs).
pub fn start_sound(
    db: &mut Db,
    who: &Identity,
    path: &str,
    device: &str,
    media: &str,
) -> Result<Value, String> {
    if sound_name(db, who, path, Some(device))? != device {
        return Err(format!("{device}: Invalid device name"));
    }
    // Linked to the recording where it is, as its upload knows it (`File::target`, the path
    // it shows at), so that its heartbeat and its end find the device, also through a link.
    let real = db.physical(who, path, true)?;
    make(db, who, device, &real, Some(media.to_string()))?;
    Ok(json!({ "device": device }))
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

/// Make the device `device`, `who`'s link to the file at `path`, with what its live sound is
/// heard by, if it is one; it takes over one of that name that `who` may take. A live sound
/// lasts while its recording's page is heard (api::RECORDING_IDLE); a presentation, until
/// stopped or long unturned.
fn make(
    db: &mut Db,
    who: &Identity,
    device: &str,
    path: &str,
    media: Option<String>,
) -> Result<(), String> {
    free(db, who, device)?;
    if db.node(device).is_some() {
        db.delete_node(device);
    }
    for directory in ["/dev", terminal_worker_core::parent(device)] {
        if db.node(directory).is_none() {
            db.set_node(directory, &Node::directory(0, 0, 0o755));
        }
    }
    db.set_node(device, &Node::symlink(who.id, who.group, path));
    let lasts = if media.is_some() {
        crate::api::RECORDING_IDLE
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
    Ok(())
}

/// Stop the device `name` of `directory`, a presentation or a live sound, as its account or
/// root.
pub fn stop(db: &mut Db, who: &Identity, directory: &str, name: &str) -> Result<Value, String> {
    let device = device_path(directory, name)?;
    let Some(made) = db.device(&device) else {
        return Err(format!("{device}: No such device; it does not exist"));
    };
    if made.owner != who.id && who.id != 0 {
        return Err(format!(
            "{device}: Permission denied: it is another account's"
        ));
    }
    end(db, &device);
    Ok(json!({ "device": device }))
}

/// Remove the device `name`: its link, its row, its deadline; its followers hear so.
pub fn end(db: &mut Db, name: &str) {
    if db.node(name).is_some_and(|node| node.is_link()) {
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
        "sound": sound,
    }))
}

/// The device `name`, if `who` may follow or hear it: it exists, its link is still there, `who`
/// may search its directory, and read the file it shows.
pub fn followed(db: &Db, who: &Identity, name: &str) -> Result<Device, String> {
    let missing = || format!("{name}: No such device; it does not exist");
    let device = db.device(name).ok_or_else(missing)?;
    if db.node(name).and_then(|node| node.link) != Some(device.path.clone()) {
        return Err(missing());
    }
    let directory = terminal_worker_core::parent(name);
    db.access(who, &db.physical(who, directory, true)?, EXEC)?;
    let real = db.physical(who, &device.path, true)?;
    db.access(who, &real, READ)?;
    Ok(device)
}
