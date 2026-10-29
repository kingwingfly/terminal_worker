//! /bin: the programs the page runs as processes of their own, built for wasm32-wasip1 (uutils'
//! coreutils, crates/coreutils, one binary as busybox is), in its WASI engine (web/wasi/). The
//! shell's own commands of the same names come first, as a shell's builtins do; `/bin/NAME` runs
//! /bin's, and `coreutils NAME` too.

/// The utilities /bin's binary has, as crates/coreutils builds them: uutils', and `syscall`,
/// which makes the kernel's own calls through the engine's `terminal.sys`.
pub const PROGRAMS: &[&str] = &[
    "arch",
    "base32",
    "base64",
    "basename",
    "basenc",
    "cat",
    "cksum",
    "b2sum",
    "md5sum",
    "nice",
    "sha1sum",
    "sha224sum",
    "sha256sum",
    "sha384sum",
    "sha512sum",
    "comm",
    "cp",
    "csplit",
    "cut",
    "date",
    "dd",
    "dir",
    "dircolors",
    "dirname",
    "echo",
    "expand",
    "expr",
    "factor",
    "false",
    "fmt",
    "fold",
    "head",
    "hostid",
    "join",
    "link",
    "ln",
    "ls",
    "mkdir",
    "mktemp",
    "mv",
    "nl",
    "numfmt",
    "od",
    "paste",
    "pathchk",
    "pr",
    "printenv",
    "printf",
    "ptx",
    "pwd",
    "readlink",
    "realpath",
    "rm",
    "rmdir",
    "seq",
    "shred",
    "shuf",
    "sleep",
    "sort",
    "split",
    "sum",
    "tail",
    "tee",
    "test",
    "touch",
    "tr",
    "true",
    "truncate",
    "tsort",
    "tty",
    "uname",
    "unexpand",
    "uniq",
    "unlink",
    "vdir",
    "wc",
    "yes",
    "nproc",
    "syscall",
];

/// Those that may change files, after which the shell looks at its directory again.
const WRITING: &[&str] = &[
    "cp", "mv", "rm", "rmdir", "mkdir", "touch", "ln", "link", "unlink", "truncate", "shred",
    "split", "csplit", "tee", "dd", "mktemp",
];

/// The utility of /bin that `name` runs, as typed: `/bin/NAME`, or a utility's name.
pub fn program(name: &str) -> Option<&'static str> {
    let name = name.strip_prefix("/bin/").unwrap_or(name);
    PROGRAMS.iter().copied().find(|program| *program == name)
}

/// Whether the utility `name` may change files.
pub fn writes(name: &str) -> bool {
    WRITING.contains(&name)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_list_is_the_binary_s() {
        let main = include_str!("../../coreutils/src/main.rs");
        let listed = main
            .split("pub const UTILITIES: &[&str] = &[")
            .nth(1)
            .and_then(|rest| rest.split("];").next())
            .unwrap();
        let built: Vec<&str> = listed
            .split(',')
            .map(|name| name.trim().trim_matches('"'))
            .filter(|name| !name.is_empty())
            .collect();
        assert_eq!(built, super::PROGRAMS);
        assert_eq!(super::program("/bin/ls"), Some("ls"));
        assert_eq!(super::program("zip"), None);
    }
}
