//! /dev (owner's request, 2026-10-28): presentations others follow, and live sound others hear.
//! `present FILE` makes /dev/present/NAME, a symbolic link to the deck or PDF it shows, owned by
//! the presenter; `stream` makes /dev/snd/NAME, whose sound goes live through Cloudflare Realtime
//! (sound.rs): a link to the recording `stream FILE` makes, or, kept nowhere (`stream -n NAME`),
//! a file of its own. Whoever may read the file follows or hears it (`open /dev/present/NAME`,
//! `open /dev/snd/NAME`), guests too: their pages hear each slide the presenter's account shows
//! (store.rs heard), and a presentation's listeners hear its presenter's sound with it: the one
//! of the same name, or the latest. A listener its presenter lets speak (`allow`) is heard by
//! all of them too, as a voice of the presentation, the presenter's, which they end.
//! /dev and its directories are root's, there from the start (`ensure`); only the authority
//! writes in them, and `rm` of a device stops it (`remove`).
use crate::auth;
use crate::db::{Db, Device, Grant};
use serde_json::{Value, json};
use terminal_worker_core::acl::{self, Spec};
use terminal_worker_core::devices::{PRESENT, SOUND, default_name, device_path};
use terminal_worker_core::{CHUNK_SIZE, kind, parent, permissions::*};

/// How long a presentation lasts after its last turn, in seconds, unless stopped before.
pub const PRESENT_TTL: u64 = 12 * 60 * 60;
/// How long a live sound lasts unheard from its page (sound.mjs keeps it every half minute).
pub const SOUND_IDLE: u64 = 2 * 60;
/// How many live sounds an account may send at once: each is a Realtime session, and a name in
/// /dev/snd no other account may take.
pub const SOUNDS_MAX: usize = 3;
/// How many listeners may speak in a presentation at once ...
pub const VOICES_MAX: usize = 2;
/// ... and how long one let speak has to start, in seconds.
pub const GRANT_TTL: u64 = 2 * 60;

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
/// making, or kept nowhere; NAME is the account's by default (owner's request, 2026-10-28), so
/// that one sound goes with all its presentations; `who` not guest.
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
    let name = name.map_or_else(|| db.username(who.id), str::to_string);
    let device = device_path(SOUND, &name)?;
    free(db, who, &device)?;
    let sending = db
        .sounds_of(who.id)
        .into_iter()
        .filter(|other| *other != device)
        .count();
    if sending >= SOUNDS_MAX {
        return Err(format!(
            "An account sends at most {SOUNDS_MAX} live sounds at once; stop one first"
        ));
    }
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
    db.started_sound(device);
    let version = db.device(device).map(|made| version(&made));
    Ok(json!({ "device": device, "version": version }))
}

/// Let the listener `speaker`, who asked (`ask`, store.rs), speak in the presentation
/// `presentation`, as its presenter `who`: a token, for their page alone, which sends their
/// voice once (`start_voice`), soon.
pub fn allow(
    db: &mut Db,
    who: &Identity,
    presentation: &str,
    speaker: &str,
    ask: &str,
) -> Result<String, String> {
    let device = db
        .device(presentation)
        .filter(|device| parent(&device.name) == PRESENT)
        .ok_or_else(|| format!("{presentation}: No such device; the presentation ended"))?;
    if device.owner != who.id {
        return Err(format!(
            "{presentation}: Permission denied: it is another account's"
        ));
    }
    let token = auth::random_token();
    db.set_grant(&Grant {
        token: token.clone(),
        presentation: presentation.to_string(),
        speaker: speaker.to_string(),
        ask: ask.to_string(),
        device: None,
    });
    db.set_expiry("grant", &token, auth::now() + GRANT_TTL);
    Ok(token)
}

/// The grant `token`, while it may still start a voice: unused, in time, and its presentation
/// still there; and that presentation.
fn usable(db: &Db, token: &str) -> Result<(Grant, Device), String> {
    let expired = || "Not allowed to speak, or no longer: ask again".to_string();
    let grant = db
        .grant(token)
        .filter(|grant| grant.device.is_none())
        .ok_or_else(expired)?;
    if db
        .expiry("grant", token)
        .is_none_or(|due| auth::now() > due)
    {
        return Err(expired());
    }
    let presentation = db
        .device(&grant.presentation)
        .ok_or_else(|| format!("{}: the presentation ended", grant.presentation))?;
    Ok((grant, presentation))
}

/// The voice the grant `token` sends: /dev/snd/PRESENTATION-SPEAKER, numbered if taken.
pub fn voice_name(db: &Db, token: &str) -> Result<String, String> {
    let (grant, presentation) = usable(db, token)?;
    if db.voices_for(&presentation.name).len() >= VOICES_MAX {
        return Err(format!(
            "At most {VOICES_MAX} listeners speak at once; wait for one to finish"
        ));
    }
    let base = presentation.name.rsplit('/').next().unwrap_or_default();
    let stem = format!("{base}-{}", grant.speaker);
    let mut device = device_path(SOUND, &stem)?;
    let mut number = 2;
    while db.device(&device).is_some() || db.node(&device).is_some() {
        device = device_path(SOUND, &format!("{stem}-{number}"))?;
        number += 1;
    }
    Ok(device)
}

/// Make the voice `device`, which `media` hears, as the grant `token` lets: the presenter's,
/// a link to the file presented, so that whoever follows the presentation hears it; its page
/// keeps it (`keep`), and ends it (`stop`), by the token.
pub fn start_voice(db: &mut Db, token: &str, device: &str, media: &str) -> Result<Value, String> {
    let (mut grant, presentation) = usable(db, token)?;
    if voice_name(db, token)? != device {
        return Err(format!("{device}: Device or resource busy"));
    }
    let presenter = db
        .user_by_id(presentation.owner)
        .map(|(_, user)| Identity::of(&user))
        .ok_or("The presenter's account is gone")?;
    let mut media: Value = serde_json::from_str(media).unwrap_or_default();
    media["for"] = json!(presentation.name);
    media["speaker"] = json!(grant.speaker);
    media["ask"] = json!(grant.ask);
    let link = Node::symlink(presenter.id, presenter.group, &presentation.path);
    make(
        db,
        &presenter,
        device,
        link,
        &presentation.path,
        Some(media.to_string()),
    );
    grant.device = Some(device.to_string());
    db.set_grant(&grant);
    db.clear_expiry("grant", token);
    db.started_sound(device);
    let version = db.device(device).map(|made| version(&made));
    Ok(json!({ "device": device, "version": version }))
}

/// Whether the grant `token` is the one the voice `device` was started by.
fn speaking(db: &Db, token: Option<&str>, device: &str) -> bool {
    token
        .and_then(|token| db.grant(token))
        .is_some_and(|grant| grant.device.as_deref() == Some(device))
}

/// What a voice is said to be, to whoever hears of it: who speaks, in which presentation, for
/// which ask.
pub fn voice(device: &Device) -> Option<Value> {
    let media: Value = serde_json::from_str(device.media.as_deref()?).ok()?;
    let presentation = media["for"].as_str()?;
    Some(json!({
        "device": device.name, "presentation": presentation, "speaker": media["speaker"],
        "ask": media["ask"], "version": version(device),
    }))
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
/// is still that one (`version`): `who`'s, or the voice the grant `token` started.
pub fn keep(
    db: &mut Db,
    who: &Identity,
    name: &str,
    version: &str,
    token: Option<&str>,
) -> Result<Value, String> {
    let device = device_path(SOUND, name)?;
    match db.device(&device) {
        Some(made)
            if (made.owner == who.id || speaking(db, token, &device))
                && self::version(&made) == version =>
        {
            db.set_expiry("device", &device, auth::now() + SOUND_IDLE);
            Ok(json!({ "device": device }))
        }
        _ => Err(format!("{device}: No such device; it does not exist")),
    }
}

/// Stop the device `name` of `directory`, a presentation or a live sound, as its account or
/// root, or, a voice, as the page the grant `token` let send it; given a `version`
/// (`version`), only while it is still that one: a page ending the sound it sent ends nothing
/// that another page of the account has taken the name over with.
pub fn stop(
    db: &mut Db,
    who: &Identity,
    directory: &str,
    name: &str,
    version: Option<&str>,
    token: Option<&str>,
) -> Result<Value, String> {
    let device = device_path(directory, name)?;
    let Some(made) = db.device(&device) else {
        return Err(format!("{device}: No such device; it does not exist"));
    };
    if made.owner != who.id && who.id != 0 && !speaking(db, token, &device) {
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
/// deadline; its followers hear so. A presentation's voices end with it.
pub fn end(db: &mut Db, name: &str) {
    if db.node(name).is_some_and(|node| !node.directory) {
        db.delete_node(name);
    }
    db.delete_device(name);
    db.clear_expiry("device", name);
    if parent(name) == PRESENT {
        for voice in db.voices_for(name) {
            end(db, &voice.name);
        }
    }
    db.delete_grants_of(name);
}

/// What `rm` does to the device at `path` (owner's request, 2026-10-28): it stops it, as `stop`
/// does, which its account, or root, may. None if `path` is no device.
pub fn remove(db: &mut Db, who: &Identity, path: &str) -> Option<Result<Value, String>> {
    let directory = parent(path);
    if directory != PRESENT && directory != SOUND {
        return None;
    }
    db.device(path)?;
    let name = path.rsplit('/').next().unwrap_or_default();
    Some(stop(db, who, directory, name, None, None))
}

/// What the account `who` presents of the file at `path`, for its pages' bar: who it is (none
/// for guest, who presents nothing), its presentations of it, the listeners speaking in them, and those asking to (`asks`, as the
/// lines say: presentation, ask, name).
pub fn presented(db: &Db, who: &Identity, path: &str, asks: &[(String, String, String)]) -> Value {
    let devices: Vec<Device> = if who.id == GUEST {
        Vec::new()
    } else {
        db.presenting(who.id, path)
            .into_iter()
            .filter(|device| parent(&device.name) == PRESENT)
            .collect()
    };
    let voices: Vec<Value> = devices
        .iter()
        .flat_map(|device| db.voices_for(&device.name))
        .filter_map(|device| voice(&device))
        .collect();
    let asks: Vec<Value> = asks
        .iter()
        .filter(|(presentation, ..)| devices.iter().any(|device| device.name == *presentation))
        .map(|(presentation, id, name)| json!({ "presentation": presentation, "id": id, "name": name }))
        .collect();
    let devices: Vec<Value> = devices
        .iter()
        .map(|device| json!({ "device": device.name, "version": version(device) }))
        .collect();
    let user = (who.id != GUEST).then(|| db.username(who.id));
    json!({ "user": user, "devices": devices, "voices": voices, "asks": asks })
}

/// The presentation at `name`, as `who` follows it: the file it shows, of what kind, whose it
/// is, whether `who` turns it too (its presenter's account), where it is, its sound (`sound`)
/// and the voices of the listeners speaking in it, those `who` may hear. Following needs read
/// permission on the file.
pub fn attend(db: &Db, who: &Identity, name: &str) -> Result<Value, String> {
    let device = followed(db, who, name)?;
    let owner = db.username(device.owner);
    let voices: Vec<Value> = db
        .voices_for(&device.name)
        .into_iter()
        .filter(|voice| followed(db, who, &voice.name).is_ok())
        .filter_map(|device| voice(&device))
        .collect();
    Ok(json!({
        "device": device.name, "path": device.path, "kind": kind(&device.path), "owner": owner,
        "controller": who.id == device.owner, "place": db.place(device.owner, &device.path),
        "sound": sound(db, who, &device), "voices": voices, "version": version(&device),
    }))
}

/// The live sound the listeners of the presentation `presentation` hear with it, of those `who`
/// may hear: its presenter's of the same name, or else the latest it started (owner's choice,
/// 2026-10-28).
pub fn sound(db: &Db, who: &Identity, presentation: &Device) -> Option<String> {
    let base = presentation.name.rsplit('/').next().unwrap_or_default();
    let same = format!("{SOUND}/{base}");
    let mut sounds = db.sounds_by(presentation.owner);
    sounds.sort_by_key(|sound| sound.name != same);
    sounds
        .into_iter()
        .find(|sound| followed(db, who, &sound.name).is_ok())
        .map(|sound| sound.name)
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
