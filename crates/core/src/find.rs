//! fd's options and matching, as sharkdp/fd has them for the options listed. The server walks
//! the tree; this decides what it prints, and what fd -x or -X runs.
use regex::{Regex, RegexBuilder};

/// What `fd` looks for, and what it does with what it finds.
#[derive(Debug, Default)]
pub struct Find {
    pattern: Option<Regex>,
    /// Whether the pattern matches the whole path rather than the name.
    full_path: bool,
    pub hidden: bool,
    /// `f` files, `d` directories, `x` executable files; any of them, or all if empty.
    types: Vec<char>,
    extensions: Vec<String>,
    pub max_depth: Option<usize>,
    pub absolute: bool,
    /// Whether results end with NUL rather than a newline.
    pub null: bool,
    /// The directories to search, as typed; the working directory if none.
    pub roots: Vec<String>,
    /// The command after `-x` (each result) or `-X` (all results at once).
    pub exec: Option<(bool, Vec<String>)>,
}

const USAGE: &str = "Usage: fd [-H] [-t f|d|l|x] [-e EXT] [-d N] [-g|-F] [-i|-s] [-p] [-a] [-0] [PATTERN] [PATH...] [-x|-X COMMAND...]";

/// A glob as a regular expression matching all of a name: `*` and `?` within a name, `**`
/// across directories.
fn glob(pattern: &str) -> String {
    let mut out = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                out.push_str(".*");
            }
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            '[' => {
                out.push('[');
                if chars.peek() == Some(&'!') {
                    chars.next();
                    out.push('^');
                }
                for c in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                    if c == '\\' || c == '[' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push(']');
            }
            c => out.push_str(&regex::escape(&c.to_string())),
        }
    }
    out.push('$');
    out
}

/// Parse `fd`'s arguments.
pub fn parse(args: &[String]) -> Result<Find, String> {
    let mut find = Find::default();
    let (mut glob_mode, mut fixed, mut case) = (false, false, None);
    let mut operands = Vec::new();
    let mut args = args.iter();
    let value = |args: &mut std::slice::Iter<String>, option: &str| {
        args.next()
            .cloned()
            .ok_or_else(|| format!("fd: {option} needs a value"))
    };
    let mut end = false;
    while let Some(arg) = args.next() {
        if end || !arg.starts_with('-') || arg == "-" {
            operands.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => end = true,
            "-H" | "--hidden" => find.hidden = true,
            "-g" | "--glob" => glob_mode = true,
            "-F" | "--fixed-strings" => fixed = true,
            "-i" | "--ignore-case" => case = Some(false),
            "-s" | "--case-sensitive" => case = Some(true),
            "-p" | "--full-path" => find.full_path = true,
            "-a" | "--absolute-path" => find.absolute = true,
            "-0" | "--print0" => find.null = true,
            "-t" | "--type" => {
                let kind = value(&mut args, arg)?;
                match kind.as_str() {
                    "f" | "file" => find.types.push('f'),
                    "d" | "directory" => find.types.push('d'),
                    "l" | "symlink" => find.types.push('l'),
                    "x" | "executable" => find.types.push('x'),
                    _ => return Err(format!("fd: unsupported type {kind}; use f, d, l or x")),
                }
            }
            "-e" | "--extension" => find.extensions.push(
                value(&mut args, arg)?
                    .trim_start_matches('.')
                    .to_lowercase(),
            ),
            "-d" | "--max-depth" => {
                let depth = value(&mut args, arg)?;
                find.max_depth = Some(
                    depth
                        .parse()
                        .map_err(|_| format!("fd: invalid depth {depth}"))?,
                );
            }
            "-x" | "--exec" | "-X" | "--exec-batch" => {
                let template: Vec<String> = args
                    .by_ref()
                    .take_while(|a| a.as_str() != ";")
                    .cloned()
                    .collect();
                if template.is_empty() {
                    return Err(format!("fd: {arg} needs a command"));
                }
                find.exec = Some((matches!(arg.as_str(), "-X" | "--exec-batch"), template));
            }
            _ => return Err(format!("fd: unsupported option {arg}\n{USAGE}")),
        }
    }
    let mut operands = operands.into_iter();
    if let Some(pattern) = operands.next().filter(|p| !p.is_empty()) {
        let source = if glob_mode {
            glob(&pattern)
        } else if fixed {
            regex::escape(&pattern)
        } else {
            pattern.clone()
        };
        // Smart case: a pattern without capitals ignores case.
        let sensitive = case.unwrap_or_else(|| pattern.chars().any(char::is_uppercase));
        find.pattern = Some(
            RegexBuilder::new(&source)
                .case_insensitive(!sensitive)
                .build()
                .map_err(|e| format!("fd: invalid pattern: {e}"))?,
        );
    }
    find.roots = operands.collect();
    Ok(find)
}

impl Find {
    /// Whether the node at `path`, below the searched directory `root`, is a result.
    /// Whether `path`, below `root`, is found: a directory, a symbolic link if `link`, or else a
    /// file, with `mode`.
    pub fn matches(&self, root: &str, path: &str, directory: bool, link: bool, mode: u16) -> bool {
        let below = path
            .strip_prefix(root)
            .unwrap_or(path)
            .trim_start_matches('/');
        if !self.hidden && below.split('/').any(|part| part.starts_with('.')) {
            return false;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        if !self.types.is_empty()
            && !self.types.iter().any(|t| match t {
                'f' => !directory && !link,
                'd' => directory,
                'l' => link,
                _ => !directory && !link && mode & 0o111 != 0,
            })
        {
            return false;
        }
        if !self.extensions.is_empty() {
            let lower = name.to_lowercase();
            if directory
                || !self
                    .extensions
                    .iter()
                    .any(|e| lower.ends_with(&format!(".{e}")))
            {
                return false;
            }
        }
        match &self.pattern {
            Some(pattern) => pattern.is_match(if self.full_path { path } else { name }),
            None => true,
        }
    }
    /// How a result prints: absolute with `-a`; otherwise below the root as it was typed, or
    /// relative to `cwd` when no root was typed.
    pub fn display(&self, typed: Option<&str>, root: &str, cwd: &str, path: &str) -> String {
        if self.absolute {
            return path.into();
        }
        let below = path
            .strip_prefix(root)
            .unwrap_or(path)
            .trim_start_matches('/');
        match typed {
            Some(typed) if typed != "." && typed != "./" => {
                format!("{}/{below}", typed.trim_end_matches('/'))
            }
            Some(_) => below.into(),
            None if root == cwd => below.into(),
            None => path.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn find(args: &[&str]) -> Find {
        parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn patterns_types_extensions_and_hidden_names() {
        let f = find(&["spéc"]);
        assert!(f.matches("/", "/docs/SPÉCTRUM.txt", false, false, 0o644));
        assert!(!find(&["Spéc"]).matches("/", "/docs/spéctrum.txt", false, false, 0o644));
        assert!(find(&["-i", "Spéc"]).matches("/", "/spéctrum", false, false, 0o644));
        let md = find(&["-e", "MD", "-t", "f"]);
        assert!(md.matches("/", "/a/notes.md", false, false, 0o644));
        assert!(!md.matches("/", "/a/notes.md", true, false, 0o755));
        assert!(!md.matches("/", "/a/notes.mdx", false, false, 0o644));
        assert!(find(&["-t", "x"]).matches("/", "/run.sh", false, false, 0o755));
        assert!(!find(&["-t", "x"]).matches("/", "/run.sh", false, false, 0o644));
        assert!(!find(&[]).matches("/home", "/home/.cache/a", false, false, 0o644));
        assert!(find(&["-H"]).matches("/home", "/home/.cache/a", false, false, 0o644));
        // A hidden directory searched by name is not hidden itself.
        assert!(find(&[]).matches("/home/.config", "/home/.config/a", false, false, 0o644));
        assert!(find(&["-p", "^/src/.*\\.rs$"]).matches("/", "/src/a/b.rs", false, false, 0o644));
    }

    #[test]
    fn globs_and_fixed_strings() {
        let g = find(&["-g", "*.rs"]);
        assert!(g.matches("/", "/src/main.rs", false, false, 0o644));
        assert!(!g.matches("/", "/src/main.rs.bak", false, false, 0o644));
        assert!(find(&["-g", "a?[0-9]"]).matches("/", "/ab1", false, false, 0o644));
        assert!(find(&["-g", "[!a]*"]).matches("/", "/b", false, false, 0o644));
        assert!(find(&["-F", "a.b"]).matches("/", "/xa.by", false, false, 0o644));
        assert!(!find(&["-F", "a.b"]).matches("/", "/axb", false, false, 0o644));
    }

    #[test]
    fn roots_exec_and_display() {
        let f = find(&["-e", "txt", ".", "docs", "notes", "-X", "wc", "-l"]);
        assert_eq!(f.roots, ["docs", "notes"]);
        assert_eq!(f.exec, Some((true, vec!["wc".into(), "-l".into()])));
        let f = find(&["x", "-x", "rm", "{}", ";"]);
        assert_eq!(f.exec, Some((false, vec!["rm".into(), "{}".into()])));
        assert_eq!(f.display(None, "/home/a", "/home/a", "/home/a/b/c"), "b/c");
        assert_eq!(
            f.display(Some("docs/"), "/home/a/docs", "/home/a", "/home/a/docs/x"),
            "docs/x"
        );
        assert_eq!(f.display(Some("."), "/home/a", "/home/a", "/home/a/x"), "x");
        assert_eq!(find(&["-a"]).display(None, "/h", "/h", "/h/x"), "/h/x");
        for bad in [
            &["-t", "z"][..],
            &["-x"],
            &["-d", "deep"],
            &["--nope"],
            &["("],
        ] {
            assert!(parse(&bad.iter().map(|a| a.to_string()).collect::<Vec<_>>()).is_err());
        }
    }
}
