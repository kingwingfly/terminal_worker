//! zip, unzip and tar: what each is asked to do, and where an archive's members may go. The
//! archives themselves are read and written in a container (crates/archiver).
use serde::{Deserialize, Serialize};

use crate::resolve;

/// The most members an archive may have, ...
pub const ENTRIES_MAX: usize = 10_000;
/// ... and the most bytes they may expand to, together.
pub const EXPANDED_MAX: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Zip,
    Tar,
    /// A tar archive in gzip.
    Tgz,
}

/// What extraction does with a file that exists already.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Existing {
    /// Ask, as unzip does without -o or -n.
    Ask,
    /// Replace it, as tar does, and unzip -o.
    Replace,
    /// Keep it silently, as unzip -n and tar --skip-old-files do.
    Skip,
    /// Keep it, reporting each one as an error, as tar -k does.
    Keep,
}

/// One zip, unzip or tar command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "lowercase")]
pub enum Job {
    /// Write `paths` into the new archive `archive`; relative ones are found in `directory`, if
    /// given (tar -C), and stored relative to it.
    Create {
        format: Format,
        archive: String,
        paths: Vec<String>,
        recursive: bool,
        verbose: bool,
        #[serde(default)]
        directory: Option<String>,
        /// Store symbolic and hard links as links, as tar does unless -h, and zip with -y;
        /// otherwise what they lead to.
        #[serde(default)]
        links: bool,
    },
    /// Write the members of `archive`, or those named in `members`, into `directory`.
    Extract {
        /// Zip for unzip; for tar, whatever the archive is, tar or gzipped tar.
        zip: bool,
        archive: String,
        directory: Option<String>,
        existing: Existing,
        members: Vec<String>,
        verbose: bool,
    },
    /// List the members of `archive`.
    List {
        zip: bool,
        archive: String,
        verbose: bool,
    },
}

/// The format a new archive named `name` gets: gzipped tar for .tgz and .tar.gz names.
fn tar_format(name: &str, gzip: bool) -> Format {
    let lower = name.to_lowercase();
    if gzip || lower.ends_with(".tgz") || lower.ends_with(".tar.gz") {
        Format::Tgz
    } else {
        Format::Tar
    }
}

/// `zip [-r] [-q] [-v] [--] ARCHIVE PATH...`
pub fn zip_args(args: &[String]) -> Result<Job, String> {
    let (mut recursive, mut verbose, mut links) = (false, true, false);
    let mut operands = Vec::new();
    let mut end = false;
    for arg in args {
        match arg.as_str() {
            "--" if !end => end = true,
            flags if !end && flags.starts_with('-') && flags.len() > 1 => {
                for flag in flags[1..].chars() {
                    match flag {
                        'r' | 'R' => recursive = true,
                        'y' => links = true,
                        'q' => verbose = false,
                        'v' => verbose = true,
                        _ => return Err(format!("zip: unsupported option -{flag}; see man zip")),
                    }
                }
            }
            _ => operands.push(arg.clone()),
        }
    }
    if operands.len() < 2 {
        return Err("Usage: zip [-r] [-y] [-q] ARCHIVE.zip PATH...".into());
    }
    let mut archive = operands.remove(0);
    // As Info-ZIP does, a name without an extension gets .zip.
    if !archive.rsplit('/').next().unwrap_or_default().contains('.') {
        archive.push_str(".zip");
    }
    Ok(Job::Create {
        format: Format::Zip,
        archive,
        paths: operands,
        recursive,
        verbose,
        directory: None,
        links,
    })
}

/// `unzip [-l] [-o|-n] [-q] [-d DIR] [--] ARCHIVE [MEMBER...]`
pub fn unzip_args(args: &[String]) -> Result<Job, String> {
    let (mut list, mut verbose, mut existing) = (false, true, Existing::Ask);
    let mut directory = None;
    let mut operands = Vec::new();
    let mut args = args.iter();
    let mut end = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" if !end => end = true,
            "-d" if !end => {
                directory = Some(args.next().cloned().ok_or("unzip: -d needs a directory")?)
            }
            flags if !end && flags.starts_with('-') && flags.len() > 1 => {
                for flag in flags[1..].chars() {
                    match flag {
                        'l' => list = true,
                        'o' => existing = Existing::Replace,
                        'n' => existing = Existing::Skip,
                        'q' => verbose = false,
                        _ => {
                            return Err(format!(
                                "unzip: unsupported option -{flag}; see man unzip"
                            ));
                        }
                    }
                }
            }
            _ => operands.push(arg.clone()),
        }
    }
    if operands.is_empty() {
        return Err("Usage: unzip [-l] [-o|-n] [-q] [-d DIR] ARCHIVE.zip [MEMBER...]".into());
    }
    let archive = operands.remove(0);
    Ok(if list {
        Job::List {
            zip: true,
            archive,
            verbose: true,
        }
    } else {
        Job::Extract {
            zip: true,
            archive,
            directory,
            existing,
            members: operands,
            verbose,
        }
    })
}

/// `tar -c|-x|-t [-z] [-v] -f ARCHIVE [-C DIR] [-k|--skip-old-files|--overwrite] [PATH...]`,
/// with bundled flags (`-czf`) and old-style ones (`czf`), whose values follow in order.
pub fn tar_args(args: &[String]) -> Result<Job, String> {
    let mut mode = None;
    let (mut gzip, mut verbose, mut existing) = (false, false, Existing::Replace);
    let mut links = true;
    let (mut file, mut directory) = (None, None);
    let mut operands = Vec::new();
    let mut rest: std::collections::VecDeque<String> = args.iter().cloned().collect();
    let set_mode = |mode: &mut Option<char>, m: char| {
        if mode.is_some_and(|old| old != m) {
            return Err("tar: use one of -c, -x and -t".to_string());
        }
        *mode = Some(m);
        Ok(())
    };
    let mut first = true;
    let mut end = false;
    while let Some(arg) = rest.pop_front() {
        let old_style = first && !arg.starts_with('-') && !arg.is_empty();
        first = false;
        if end {
            operands.push(arg);
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let mut value = || {
                value
                    .clone()
                    .or_else(|| rest.pop_front())
                    .ok_or(format!("tar: --{name} needs a value"))
            };
            match name {
                "" => end = true,
                "create" => set_mode(&mut mode, 'c')?,
                "extract" | "get" => set_mode(&mut mode, 'x')?,
                "list" => set_mode(&mut mode, 't')?,
                "gzip" | "gunzip" | "ungzip" => gzip = true,
                "verbose" => verbose = true,
                "dereference" => links = false,
                "keep-old-files" => existing = Existing::Keep,
                "skip-old-files" => existing = Existing::Skip,
                "overwrite" => existing = Existing::Replace,
                "file" => file = Some(value()?),
                "directory" => directory = Some(value()?),
                _ => return Err(format!("tar: unsupported option --{name}; see man tar")),
            }
            continue;
        }
        let flags = match arg.strip_prefix('-') {
            Some(flags) if !flags.is_empty() => flags.to_string(),
            _ if old_style => arg.clone(),
            _ => {
                operands.push(arg);
                continue;
            }
        };
        for flag in flags.chars() {
            match flag {
                'c' | 'x' | 't' => set_mode(&mut mode, flag)?,
                'z' => gzip = true,
                'v' => verbose = true,
                'h' => links = false,
                'k' => existing = Existing::Keep,
                'f' | 'C' => {
                    let value = rest
                        .pop_front()
                        .ok_or(format!("tar: -{flag} needs a value"))?;
                    if flag == 'f' {
                        file = Some(value);
                    } else {
                        directory = Some(value);
                    }
                }
                _ => return Err(format!("tar: unsupported option -{flag}; see man tar")),
            }
        }
    }
    let usage = "Usage: tar -c|-x|-t [-z] [-v] [-h] -f ARCHIVE [-C DIR] [PATH...]";
    let archive = file.ok_or_else(|| {
        format!("tar: name the archive with -f; this terminal has no tape drive\n{usage}")
    })?;
    match mode {
        Some('c') => {
            if operands.is_empty() {
                return Err(
                    "tar: refusing to create an empty archive; name what to put in it".into(),
                );
            }
            // -C is where the paths are found, and what their names are relative to, as GNU
            // tar works in it.
            Ok(Job::Create {
                format: tar_format(&archive, gzip),
                archive,
                paths: operands,
                recursive: true,
                verbose,
                directory,
                links,
            })
        }
        Some('x') => Ok(Job::Extract {
            zip: false,
            archive,
            directory,
            existing,
            members: operands,
            verbose,
        }),
        Some(_) => Ok(Job::List {
            zip: false,
            archive,
            verbose,
        }),
        None => Err(format!("tar: use one of -c, -x and -t\n{usage}")),
    }
}

/// Where the member `name` of an archive goes in `directory`, or why it is skipped. A leading
/// `/` is removed, as Info-ZIP and GNU tar remove it; the second value says whether one was.
pub fn member_path(directory: &str, name: &str) -> Result<(String, bool), String> {
    if name.chars().any(char::is_control) {
        return Err("its name contains a control character".into());
    }
    if name.contains('\\') {
        return Err("its name contains a backslash".into());
    }
    let relative = name.trim_start_matches('/');
    let stripped = relative.len() != name.len();
    let mut parts = Vec::new();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err("its name contains '..'".into()),
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err("its name is empty".into());
    }
    let path = resolve(directory, &parts.join("/"))?;
    // Without '..', the path stays inside; check anyway.
    if !path.starts_with(&format!("{}/", directory.trim_end_matches('/'))) && directory != "/" {
        return Err("it would be written outside the directory".into());
    }
    Ok((path, stripped))
}

/// The name an archive stores for the input typed as `typed`: without a leading `/` or `./`,
/// and without anything up to its last `..`, as GNU tar removes it, so that no name the archive
/// stores climbs out of where it is extracted.
pub fn stored_name(typed: &str) -> String {
    let parts: Vec<&str> = typed
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let after = parts.iter().rposition(|p| *p == "..").map_or(0, |i| i + 1);
    parts[after..].join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(line: &str) -> Vec<String> {
        crate::split_command(line).unwrap()
    }

    #[test]
    fn zip_and_unzip_arguments() {
        assert_eq!(
            zip_args(&args("-rq notes docs 'a b.md'")).unwrap(),
            Job::Create {
                format: Format::Zip,
                archive: "notes.zip".into(),
                paths: vec!["docs".into(), "a b.md".into()],
                recursive: true,
                verbose: false,
                directory: None,
                links: false,
            }
        );
        assert!(matches!(
            zip_args(&args("-ry site site")).unwrap(),
            Job::Create { links: true, .. }
        ));
        assert!(matches!(
            tar_args(&args("-chf a.tar docs")).unwrap(),
            Job::Create { links: false, .. }
        ));
        assert!(zip_args(&args("only.zip")).is_err());
        assert!(zip_args(&args("-x a.zip b")).is_err());
        assert_eq!(
            unzip_args(&args("-o -d out a.zip one.txt")).unwrap(),
            Job::Extract {
                zip: true,
                archive: "a.zip".into(),
                directory: Some("out".into()),
                existing: Existing::Replace,
                members: vec!["one.txt".into()],
                verbose: true,
            }
        );
        assert!(matches!(
            unzip_args(&args("-l a.zip")).unwrap(),
            Job::List { zip: true, .. }
        ));
        assert!(matches!(
            unzip_args(&args("-nq a.zip")).unwrap(),
            Job::Extract {
                existing: Existing::Skip,
                verbose: false,
                ..
            }
        ));
        assert!(unzip_args(&args("-d")).is_err());
    }

    #[test]
    fn tar_takes_bundled_old_style_and_long_options() {
        for line in [
            "-czf a.tgz docs",
            "czf a.tgz docs",
            "-c -z -f a.tgz docs",
            "--create --gzip --file=a.tgz docs",
            "-cf a.tar.gz docs",
        ] {
            assert_eq!(
                tar_args(&args(line)).unwrap(),
                Job::Create {
                    format: Format::Tgz,
                    archive: if line.contains("tar.gz") {
                        "a.tar.gz"
                    } else {
                        "a.tgz"
                    }
                    .into(),
                    paths: vec!["docs".into()],
                    recursive: true,
                    verbose: false,
                    directory: None,
                    links: true,
                },
                "{line}"
            );
        }
        assert_eq!(
            tar_args(&args("-xvkf a.tar -C 'out dir' one")).unwrap(),
            Job::Extract {
                zip: false,
                archive: "a.tar".into(),
                directory: Some("out dir".into()),
                existing: Existing::Keep,
                members: vec!["one".into()],
                verbose: true,
            }
        );
        assert!(matches!(
            tar_args(&args("-xf a.tar --skip-old-files")).unwrap(),
            Job::Extract {
                existing: Existing::Skip,
                ..
            }
        ));
        assert!(matches!(
            tar_args(&args("tf a.tar")).unwrap(),
            Job::List { verbose: false, .. }
        ));
        assert_eq!(
            tar_args(&args("-cf a.tar -C src x /abs")).unwrap(),
            Job::Create {
                format: Format::Tar,
                archive: "a.tar".into(),
                paths: vec!["x".into(), "/abs".into()],
                recursive: true,
                verbose: false,
                directory: Some("src".into()),
                links: true,
            }
        );
        for bad in [
            "-cx -f a",
            "-c docs",
            "-cf",
            "-cf a.tar",
            "-q -f a",
            "--nope -f a",
            "docs",
        ] {
            assert!(tar_args(&args(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn members_stay_inside_their_directory() {
        assert_eq!(
            member_path("/home/a/out", "docs/spéctrum.md"),
            Ok(("/home/a/out/docs/spéctrum.md".into(), false))
        );
        assert_eq!(
            member_path("/x", "/etc/passwd"),
            Ok(("/x/etc/passwd".into(), true))
        );
        assert_eq!(member_path("/x", "./a//b/"), Ok(("/x/a/b".into(), false)));
        assert_eq!(member_path("/", "a"), Ok(("/a".into(), false)));
        for bad in [
            "../evil",
            "a/../../b",
            "",
            "/",
            "a\\b",
            "a\nb",
            "a/.directory/b",
        ] {
            assert!(member_path("/x", bad).is_err(), "{bad:?}");
        }
        assert!(member_path("/x", &"d/".repeat(20)).is_err());
        assert_eq!(stored_name("/home/a/docs/"), "home/a/docs");
        assert_eq!(stored_name("../../x/./y"), "x/y");
        assert_eq!(stored_name("./notes.md"), "notes.md");
        assert_eq!(stored_name("docs/../notes.md"), "notes.md");
        assert_eq!(stored_name("a/../../b/c"), "b/c");
    }
}
