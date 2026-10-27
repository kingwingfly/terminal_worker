//! Archive jobs, as the authority sees them: zip, unzip and tar start one, whose work an
//! ArchiveRunner Durable Object does with a container, asking the authority, as the job's
//! account, for each file it reads or writes.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terminal_worker_core::archive::{self, ENTRIES_MAX, Job as Command};
use terminal_worker_core::permissions::*;
use terminal_worker_core::{MAX_FILE, kind};

use crate::commands::{self, command_path};
use crate::db::{Db, Job};

/// The commands that start jobs.
pub const ARCHIVERS: [&str; 3] = ["zip", "unzip", "tar"];

/// An archive a job reads.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub path: String,
    pub file: String,
    pub size: u64,
}
/// A member of an archive a job writes, as the files were when it started.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Input {
    pub name: String,
    /// Its content; none for a directory.
    pub file: Option<String>,
    pub size: u64,
    pub mode: u32,
    pub owner: String,
    pub group: String,
    pub uid: u64,
    pub gid: u64,
    /// Whether to store it as it is, as for content already compressed.
    pub store: bool,
}
/// What a job does.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Spec {
    pub command: Command,
    pub source: Option<Source>,
    /// Where extracted members go.
    pub directory: Option<String>,
    /// The archive written, and whether it replaces a file.
    pub output: Option<String>,
    #[serde(default)]
    pub replace: bool,
    #[serde(default)]
    pub inputs: Vec<Input>,
    /// The date every member written gets, so that rebuilding gives the same archive.
    pub mtime: u64,
    /// unzip's answers, path by path, to whether to replace an existing file.
    #[serde(default)]
    pub decisions: Option<BTreeMap<String, bool>>,
}
impl Spec {
    pub fn of(job: &Job) -> Result<Spec, String> {
        serde_json::from_str(&job.spec).map_err(|e| e.to_string())
    }
}

/// The identity a job acts as: root if started with sudo, a guest if started by one, otherwise
/// its account as it is now.
pub fn actor(db: &Db, job: &Job) -> Result<Identity, String> {
    if job.actor == 0 {
        return Ok(Identity::root());
    }
    if job.actor == GUEST {
        return Ok(Identity::guest());
    }
    match db.user_by_id(job.actor) {
        Some((_, user)) if !user.locked => Ok(Identity::of(&user)),
        _ => Err("The account that started this job is locked or deleted".into()),
    }
}

/// A complete file at `path` that `who` may read: an archive to extract or list.
fn source(db: &Db, who: &Identity, cmd: &str, path: &str) -> Result<Source, String> {
    let node = db
        .access(who, path, READ)
        .map_err(|e| format!("{cmd}: {e}"))?;
    let file = node
        .file
        .and_then(|id| db.file(&id))
        .ok_or_else(|| format!("{cmd}: {path}: This is a directory, not an archive"))?;
    if !file.complete() {
        return Err(format!(
            "{cmd}: {path} is still uploading; try again once it completes"
        ));
    }
    Ok(Source {
        path: path.into(),
        file: file.id,
        size: file.size,
    })
}

/// The members of an archive made of `paths`, as they are now; warnings for what is left out.
/// Relative paths are found in `directory` if given, as tar -C works in it, and stored as typed.
#[allow(clippy::too_many_arguments)]
fn inputs(
    db: &Db,
    who: &Identity,
    command: &commands::Command,
    cmd: &str,
    paths: &[String],
    recursive: bool,
    output: &str,
    directory: Option<&str>,
) -> Result<(Vec<Input>, Vec<String>), String> {
    let within;
    let command = match directory {
        Some(typed) => {
            let dir = command_path(db, who, command, typed).map_err(|e| format!("{cmd}: {e}"))?;
            let node = db
                .access(who, &dir, EXEC)
                .map_err(|e| format!("{cmd}: {typed}: {e}"))?;
            if !node.directory {
                return Err(format!("{cmd}: {typed}: Cannot open: Not a directory"));
            }
            within = commands::Command {
                args: Vec::new(),
                cwd: dir,
                sudo: command.sudo,
                home: command.home.clone(),
            };
            &within
        }
        None => command,
    };
    let mut members = Vec::new();
    let mut warnings = Vec::new();
    let mut names = BTreeSet::new();
    let entry = |db: &Db, name: String, node: &Node| Input {
        store: matches!(kind(&name), "image" | "video" | "audio" | "pdf" | "archive"),
        name,
        file: node.file.clone().filter(|_| !node.directory),
        size: if node.directory { 0 } else { node.size },
        mode: u32::from(node.mode),
        owner: db.username(node.owner),
        group: db.groupname(node.group),
        uid: u64::from(node.owner),
        gid: u64::from(node.group),
    };
    for typed in paths {
        let path = match command_path(db, who, command, typed)
            .and_then(|p| db.access(who, &p, READ).map(|n| (p, n)))
        {
            Ok(found) => found,
            Err(e) => {
                warnings.push(format!("{cmd}: {typed}: {e}"));
                continue;
            }
        };
        let (path, node) = path;
        // Stored as typed, or, for an absolute or home path, without its leading slash.
        let base = if typed.starts_with('/') || typed.starts_with('~') {
            archive::stored_name(&path)
        } else {
            archive::stored_name(typed)
        };
        let mut add = |name: String, node: &Node, members: &mut Vec<Input>| {
            if !name.is_empty() && names.insert(name.clone()) {
                members.push(entry(db, name, node));
            }
        };
        if !node.directory {
            if path == output {
                warnings.push(format!("{cmd}: {typed} is the archive itself; left out"));
                continue;
            }
            if let Some(file) = node.file.as_deref().and_then(|id| db.file(id))
                && !file.complete()
            {
                warnings.push(format!("{cmd}: {typed} is still uploading; left out"));
                continue;
            }
            add(base, &node, &mut members);
            continue;
        }
        add(base.clone(), &node, &mut members);
        if !recursive {
            continue;
        }
        let (found, hidden) = db
            .walk(who, &path, None)
            .map_err(|e| format!("{cmd}: {e}"))?;
        if hidden > 0 {
            warnings.push(format!(
                "{cmd}: {typed}: {hidden} director{} could not be read; left out",
                if hidden == 1 { "y" } else { "ies" }
            ));
        }
        for (p, n) in found {
            let relative = p[path.len()..].trim_start_matches('/');
            let name = if base.is_empty() {
                relative.to_string()
            } else {
                format!("{base}/{relative}")
            };
            if !n.directory {
                if p == output {
                    continue;
                }
                if !who.allows(&n, READ) {
                    warnings.push(format!("{cmd}: {p}: Permission denied; left out"));
                    continue;
                }
                if let Some(file) = n.file.as_deref().and_then(|id| db.file(id))
                    && !file.complete()
                {
                    warnings.push(format!("{cmd}: {p} is still uploading; left out"));
                    continue;
                }
            }
            add(name, &n, &mut members);
        }
        if members.len() > ENTRIES_MAX {
            return Err(format!(
                "{cmd}: more than {ENTRIES_MAX} members; archive less at once"
            ));
        }
    }
    let total: u64 = members.iter().map(|m| m.size).sum();
    if total > MAX_FILE {
        return Err(format!(
            "{cmd}: the files come to more than {} MB, the most an archive may hold",
            MAX_FILE / 1_000_000
        ));
    }
    if members.is_empty() {
        return Err(format!("{cmd}: nothing to do"));
    }
    Ok((members, warnings))
}

/// Start the archive job `command` asks for, as `who`; it is the reply's `job`.
pub fn start(
    db: &mut Db,
    who: &Identity,
    owner: u32,
    command: &commands::Command,
) -> Result<Value, String> {
    let (cmd, args) = command.args.split_first().ok_or("Missing command")?;
    let parsed = match cmd.as_str() {
        "zip" => archive::zip_args(args)?,
        "unzip" => archive::unzip_args(args)?,
        _ => archive::tar_args(args)?,
    };
    let path = |db: &Db, value: &str| command_path(db, who, command, value);
    let now = crate::auth::now();
    let mut spec = Spec {
        command: parsed.clone(),
        source: None,
        directory: None,
        output: None,
        replace: false,
        inputs: Vec::new(),
        mtime: now,
        decisions: None,
    };
    let mut warnings = Vec::new();
    match &parsed {
        Command::Create {
            format,
            archive: name,
            paths,
            recursive,
            directory,
            ..
        } => {
            let output = path(db, name)?;
            // Only where `who` may look is it told what is there.
            db.traverse(who, &output)
                .map_err(|e| format!("{cmd}: {e}"))?;
            match db.node(&output) {
                Some(node) if node.directory => {
                    return Err(format!("{cmd}: {output} is a directory"));
                }
                Some(_) if *format == archive::Format::Zip => {
                    return Err(format!(
                        "zip: {output} exists; this terminal does not update archives, so remove it or choose another name"
                    ));
                }
                Some(_) => {
                    db.access(who, &output, WRITE)
                        .map_err(|e| format!("{cmd}: {e}"))?;
                    spec.replace = true;
                }
                None => db
                    .writable_parent(who, &output)
                    .map_err(|e| format!("{cmd}: {e}"))?,
            }
            let (members, notes) = inputs(
                db,
                who,
                command,
                cmd,
                paths,
                *recursive,
                &output,
                directory.as_deref(),
            )?;
            spec.inputs = members;
            spec.output = Some(output);
            warnings = notes;
        }
        Command::Extract {
            zip,
            archive: name,
            directory,
            ..
        } => {
            spec.source = Some(source(db, who, cmd, &path(db, name)?)?);
            let dest = path(db, directory.as_deref().unwrap_or("."))?;
            // Only where `who` may look is it told what is there; unzip -d creates what is
            // missing on the way.
            if let Err(e) = db.traverse(who, &dest)
                && commands::is_permission_error(&e)
            {
                return Err(format!("{cmd}: {e}"));
            }
            match db.node(&dest) {
                Some(node) if node.directory => {
                    db.access(who, &dest, WRITE | EXEC)
                        .map_err(|e| format!("{cmd}: {e}"))?;
                }
                Some(_) => return Err(format!("{cmd}: {dest} is not a directory")),
                // unzip -d creates its directory; tar -C needs one.
                None if *zip => db
                    .mkdir(who, &dest, true)
                    .map_err(|e| format!("{cmd}: {e}"))?,
                None => {
                    return Err(format!(
                        "tar: {dest}: Cannot open: No such file or directory"
                    ));
                }
            }
            spec.directory = Some(dest);
        }
        Command::List { archive: name, .. } => {
            spec.source = Some(source(db, who, cmd, &path(db, name)?)?);
        }
    }
    let spec_value = serde_json::to_value(&spec).map_err(|e| e.to_string())?;
    let id = db.create_job(owner, who.id, cmd, &spec_value, now);
    if let Some(source) = &spec.source {
        db.pin(&id, &source.file);
    }
    for input in &spec.inputs {
        if let Some(file) = &input.file {
            db.pin(&id, file);
        }
    }
    db.add_lines(
        &id,
        &warnings.into_iter().map(|w| (true, w)).collect::<Vec<_>>(),
    );
    // A job left waiting, for an answer or a runner, is cancelled after a week.
    db.set_expiry("job", &id, now + 7 * 24 * 60 * 60);
    Ok(json!({"job": id, "kind": cmd}))
}

/// The most lines one view of a job carries.
const LINES_MAX: u32 = 500;

/// What a job's owner sees of it: its state, progress, question and new lines, a page of them,
/// and whether more follow, which a job that has ended may still have.
pub fn view(db: &Db, job: &Job, since: u64) -> Value {
    let mut lines = db.lines(&job.id, since, LINES_MAX + 1);
    let more = lines.len() > LINES_MAX as usize;
    lines.truncate(LINES_MAX as usize);
    let lines: Vec<Value> = lines
        .into_iter()
        .map(|(seq, err, text)| json!([seq, err, text]))
        .collect();
    json!({
        "id": job.id, "kind": job.kind, "state": job.state, "created": job.created,
        "progress": serde_json::from_str::<Value>(&job.progress).unwrap_or_default(),
        "question": job.question.as_deref().and_then(|q| serde_json::from_str::<Value>(q).ok()),
        "lines": lines, "more": more,
    })
}

/// End a job: let go of the files it read, and keep what it says.
pub fn finish(db: &mut Db, job: &str, state: &str) {
    db.set_job(job, crate::db::JobColumn::State, Some(state));
    db.set_job(job, crate::db::JobColumn::Question, None);
    db.unpin(job);
    db.clear_expiry("job", job);
}
