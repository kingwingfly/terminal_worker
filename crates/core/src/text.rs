//! The text filters pipelines join: cat, head, tail, grep, wc, sort and uniq. They read
//! standard input, or the files named, and write text, as their GNU namesakes do for the options
//! listed. Nothing here reads a file: the terminal reads them, and hands over their text.
use regex::{Regex, RegexBuilder};

/// The commands that are filters.
pub const FILTERS: [&str; 7] = ["cat", "head", "tail", "grep", "wc", "sort", "uniq"];

/// A filter, and the files it reads instead of standard input, if any; `-` is standard input.
#[derive(Debug)]
pub struct Filter {
    pub kind: Kind,
    pub files: Vec<String>,
}
#[derive(Debug)]
pub enum Kind {
    Cat,
    /// The first `lines` lines, or with `-n -N` all but the last `lines`.
    Head {
        lines: usize,
        all_but: bool,
    },
    /// The last `lines` lines, or with `-n +N` those from line `lines` on.
    Tail {
        lines: usize,
        from_start: bool,
    },
    Grep {
        pattern: Regex,
        invert: bool,
        number: bool,
        count: bool,
    },
    Wc {
        lines: bool,
        words: bool,
        bytes: bool,
    },
    Sort {
        reverse: bool,
        numeric: bool,
        unique: bool,
    },
    Uniq {
        count: bool,
    },
}

/// The options and operands of `args`, as short flags from `flags`, which may be combined
/// (`-in`), and options with a value from `values` (`-n 5`, `-n5`). `--` ends options.
fn options(
    command: &str,
    args: &[String],
    flags: &str,
    values: &str,
) -> Result<(Vec<(char, Option<String>)>, Vec<String>), String> {
    let mut found = Vec::new();
    let mut operands = Vec::new();
    let mut args = args.iter();
    let mut end = false;
    while let Some(arg) = args.next() {
        if end || arg == "-" || !arg.starts_with('-') {
            operands.push(arg.clone());
            continue;
        }
        if arg == "--" {
            end = true;
            continue;
        }
        let mut letters = arg[1..].char_indices();
        while let Some((at, letter)) = letters.next() {
            if values.contains(letter) {
                let rest = &arg[1 + at + letter.len_utf8()..];
                let value = if rest.is_empty() {
                    args.next()
                        .cloned()
                        .ok_or_else(|| format!("{command}: -{letter} needs a value"))?
                } else {
                    rest.to_string()
                };
                found.push((letter, Some(value)));
                break;
            } else if flags.contains(letter) {
                found.push((letter, None));
            } else if command != "grep" && letter.is_ascii_digit() && values.contains('n') {
                // head -5 is head -n 5.
                found.push(('n', Some(arg[1..].to_string())));
                break;
            } else {
                return Err(format!(
                    "{command}: unsupported option -{letter}; see man {command}"
                ));
            }
        }
    }
    Ok((found, operands))
}
fn count(command: &str, value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("{command}: invalid number of lines: {value}"))
}

/// Parse the filter `command` with `args`.
pub fn parse(command: &str, args: &[String]) -> Result<Filter, String> {
    let has = |found: &[(char, Option<String>)], c: char| found.iter().any(|(f, _)| *f == c);
    let (kind, files) = match command {
        "cat" => {
            let (_, files) = options(command, args, "", "")?;
            (Kind::Cat, files)
        }
        "head" | "tail" => {
            let (found, files) = options(command, args, "", "n")?;
            // A sign counts from the other end: head -n -N, tail -n +N.
            let (lines, sign) = match found.last() {
                Some((_, Some(value))) => match value.strip_prefix(['+', '-']) {
                    Some(digits) => (count(command, digits)?, value.chars().next()),
                    None => (count(command, value)?, None),
                },
                _ => (10, None),
            };
            (
                if command == "head" {
                    Kind::Head {
                        lines,
                        all_but: sign == Some('-'),
                    }
                } else {
                    Kind::Tail {
                        lines,
                        from_start: sign == Some('+'),
                    }
                },
                files,
            )
        }
        "grep" => {
            let (found, mut operands) = options(command, args, "ivncFE", "e")?;
            let pattern = match found.iter().find(|(f, _)| *f == 'e') {
                Some((_, Some(pattern))) => pattern.clone(),
                _ if operands.is_empty() => {
                    return Err("Usage: grep [-i] [-v] [-n] [-c] [-F] PATTERN [FILE...]".into());
                }
                _ => operands.remove(0),
            };
            let source = if has(&found, 'F') {
                regex::escape(&pattern)
            } else {
                pattern
            };
            let pattern = RegexBuilder::new(&source)
                .case_insensitive(has(&found, 'i'))
                .build()
                .map_err(|e| format!("grep: invalid pattern: {e}"))?;
            (
                Kind::Grep {
                    pattern,
                    invert: has(&found, 'v'),
                    number: has(&found, 'n'),
                    count: has(&found, 'c'),
                },
                operands,
            )
        }
        "wc" => {
            let (found, files) = options(command, args, "lwc", "")?;
            let any = !found.is_empty();
            (
                Kind::Wc {
                    lines: !any || has(&found, 'l'),
                    words: !any || has(&found, 'w'),
                    bytes: !any || has(&found, 'c'),
                },
                files,
            )
        }
        "sort" => {
            let (found, files) = options(command, args, "rnu", "")?;
            (
                Kind::Sort {
                    reverse: has(&found, 'r'),
                    numeric: has(&found, 'n'),
                    unique: has(&found, 'u'),
                },
                files,
            )
        }
        "uniq" => {
            let (found, files) = options(command, args, "c", "")?;
            if files.len() > 1 {
                return Err("uniq: reads one input; write its output with a pipe".into());
            }
            (
                Kind::Uniq {
                    count: has(&found, 'c'),
                },
                files,
            )
        }
        _ => return Err(format!("{command} is not a filter")),
    };
    Ok(Filter { kind, files })
}

fn joined<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}
/// The leading number of a line, as sort -n reads it; lines without one sort first.
fn leading_number(line: &str) -> f64 {
    let line = line.trim_start();
    let end = line
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || (i == 0 && c == '-')))
        .map_or(line.len(), |(i, _)| i);
    line[..end].parse().unwrap_or(f64::NEG_INFINITY)
}

/// The output and exit status of `filter` on `inputs`: each file's name and text, in order, or
/// standard input alone, named `-`.
pub fn apply(filter: &Filter, inputs: &[(String, String)]) -> (String, i32) {
    let several = inputs.len() > 1;
    match &filter.kind {
        Kind::Cat => (inputs.iter().map(|(_, t)| t.as_str()).collect(), 0),
        Kind::Head { .. } | Kind::Tail { .. } => {
            let mut out = String::new();
            for (i, (name, text)) in inputs.iter().enumerate() {
                if several {
                    if i > 0 {
                        out.push('\n');
                    }
                    out.push_str(&format!("==> {name} <==\n"));
                }
                let lines: Vec<&str> = text.lines().collect();
                let chosen = match filter.kind {
                    Kind::Head { lines: n, all_but } if all_but => {
                        &lines[..lines.len().saturating_sub(n)]
                    }
                    Kind::Head { lines: n, .. } => &lines[..lines.len().min(n)],
                    // tail -n +0 is tail -n +1, the whole input.
                    Kind::Tail {
                        lines: n,
                        from_start,
                    } if from_start => &lines[n.saturating_sub(1).min(lines.len())..],
                    Kind::Tail { lines: n, .. } => &lines[lines.len().saturating_sub(n)..],
                    _ => unreachable!(),
                };
                out.push_str(&joined(chosen.iter().copied()));
            }
            (out, 0)
        }
        Kind::Grep {
            pattern,
            invert,
            number,
            count,
        } => {
            let mut out = String::new();
            let mut matched = false;
            for (name, text) in inputs {
                let prefix = if several {
                    format!("{name}:")
                } else {
                    String::new()
                };
                let mut found = 0;
                for (index, line) in text.lines().enumerate() {
                    if pattern.is_match(line) == *invert {
                        continue;
                    }
                    found += 1;
                    if !count {
                        out.push_str(&prefix);
                        if *number {
                            out.push_str(&format!("{}:", index + 1));
                        }
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                if *count {
                    out.push_str(&format!("{prefix}{found}\n"));
                }
                matched |= found > 0;
            }
            (out, if matched { 0 } else { 1 })
        }
        Kind::Wc {
            lines,
            words,
            bytes,
        } => {
            let counts: Vec<(Vec<usize>, &str)> = inputs
                .iter()
                .map(|(name, text)| {
                    let mut c = Vec::new();
                    if *lines {
                        c.push(text.matches('\n').count());
                    }
                    if *words {
                        c.push(text.split_whitespace().count());
                    }
                    if *bytes {
                        c.push(text.len());
                    }
                    (c, name.as_str())
                })
                .collect();
            let mut rows = counts.clone();
            if several {
                let total = (0..counts[0].0.len())
                    .map(|i| counts.iter().map(|(c, _)| c[i]).sum())
                    .collect();
                rows.push((total, "total"));
            }
            let width = rows
                .iter()
                .flat_map(|(c, _)| c.iter().map(|n| n.to_string().len()))
                .max()
                .unwrap_or(1);
            let mut out = String::new();
            for (c, name) in rows {
                let numbers: Vec<String> = c.iter().map(|n| format!("{n:>width$}")).collect();
                out.push_str(&numbers.join(" "));
                if name != "-" {
                    out.push(' ');
                    out.push_str(name);
                }
                out.push('\n');
            }
            (out, 0)
        }
        Kind::Sort {
            reverse,
            numeric,
            unique,
        } => {
            let mut lines: Vec<&str> = inputs.iter().flat_map(|(_, t)| t.lines()).collect();
            if *numeric {
                lines.sort_by(|a, b| {
                    leading_number(a)
                        .total_cmp(&leading_number(b))
                        .then_with(|| a.cmp(b))
                });
            } else {
                lines.sort();
            }
            if *unique {
                lines.dedup_by(|a, b| {
                    if *numeric {
                        leading_number(a) == leading_number(b)
                    } else {
                        a == b
                    }
                });
            }
            if *reverse {
                lines.reverse();
            }
            (joined(lines), 0)
        }
        Kind::Uniq { count } => {
            let mut out = String::new();
            let mut lines = inputs.iter().flat_map(|(_, t)| t.lines()).peekable();
            while let Some(line) = lines.next() {
                let mut n = 1;
                while lines.peek() == Some(&line) {
                    lines.next();
                    n += 1;
                }
                if *count {
                    out.push_str(&format!("{n:>7} "));
                }
                out.push_str(line);
                out.push('\n');
            }
            (out, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(command: &str, args: &[&str], inputs: &[(&str, &str)]) -> (String, i32) {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let filter = parse(command, &args).unwrap();
        let inputs: Vec<(String, String)> = inputs
            .iter()
            .map(|(n, t)| (n.to_string(), t.to_string()))
            .collect();
        apply(&filter, &inputs)
    }
    const LINES: &str = "beta\nalpha\nGamma\nalpha\nspéctrum 10\nspéctrum 9\n";

    #[test]
    fn head_tail_and_cat() {
        assert_eq!(
            run("head", &["-n", "2"], &[("-", LINES)]).0,
            "beta\nalpha\n"
        );
        assert_eq!(run("head", &["-2"], &[("-", LINES)]).0, "beta\nalpha\n");
        assert_eq!(run("tail", &["-n1"], &[("-", LINES)]).0, "spéctrum 9\n");
        // tail -n +N starts at line N; head -n -N leaves out the last N.
        assert_eq!(
            run("tail", &["-n", "+5"], &[("-", LINES)]).0,
            "spéctrum 10\nspéctrum 9\n"
        );
        assert_eq!(run("tail", &["-n+0"], &[("-", LINES)]).0, LINES);
        assert_eq!(run("tail", &["-n", "+9"], &[("-", LINES)]).0, "");
        assert_eq!(
            run("tail", &["-n", "-1"], &[("-", LINES)]).0,
            "spéctrum 9\n"
        );
        assert_eq!(
            run("head", &["-n", "-4"], &[("-", LINES)]).0,
            "beta\nalpha\n"
        );
        assert_eq!(run("head", &["-n", "-9"], &[("-", LINES)]).0, "");
        assert_eq!(run("head", &["-n", "+1"], &[("-", LINES)]).0, "beta\n");
        assert!(parse("tail", &["-n".into(), "+x".into()]).is_err());
        assert_eq!(
            run("head", &["-n", "1"], &[("a", "1\n2\n"), ("b", "3")]).0,
            "==> a <==\n1\n\n==> b <==\n3\n"
        );
        assert_eq!(run("cat", &[], &[("a", "x"), ("b", "y\n")]).0, "xy\n");
        assert!(parse("head", &["-n".into(), "x".into()]).is_err());
        assert!(parse("cat", &["-z".into()]).is_err());
    }

    #[test]
    fn grep_selects_lines_and_reports_matches() {
        assert_eq!(
            run("grep", &["-in", "GAMMA|beta"], &[("-", LINES)]),
            ("1:beta\n3:Gamma\n".into(), 0)
        );
        assert_eq!(run("grep", &["-c", "alpha"], &[("-", LINES)]).0, "2\n");
        assert_eq!(run("grep", &["-v", "a"], &[("-", "a\nb\n")]).0, "b\n");
        assert_eq!(
            run("grep", &["-F", "a.b"], &[("-", "a.b\naxb\n")]).0,
            "a.b\n"
        );
        assert_eq!(
            run("grep", &["nothing"], &[("-", LINES)]),
            (String::new(), 1)
        );
        assert_eq!(
            run(
                "grep",
                &["-i", "SPÉCTRUM 9"],
                &[("x.txt", LINES), ("y.txt", "")]
            )
            .0,
            "x.txt:spéctrum 9\n"
        );
        assert!(parse("grep", &[]).is_err());
        assert!(parse("grep", &["(".into()]).is_err());
    }

    #[test]
    fn wc_sort_and_uniq() {
        assert_eq!(run("wc", &["-l"], &[("-", LINES)]).0, "6\n");
        assert_eq!(run("wc", &[], &[("-", "a b\nc\n")]).0, "2 3 6\n");
        assert_eq!(
            run("wc", &["-l"], &[("a", "1\n"), ("b", "1\n2\n")]).0,
            "1 a\n2 b\n3 total\n"
        );
        assert_eq!(
            run("sort", &["-u"], &[("-", LINES)]).0,
            "Gamma\nalpha\nbeta\nspéctrum 10\nspéctrum 9\n"
        );
        assert_eq!(
            run("sort", &["-n"], &[("-", "10 x\n9 y\n-1 z\nnone\n")]).0,
            "none\n-1 z\n9 y\n10 x\n"
        );
        assert_eq!(run("sort", &["-rn"], &[("-", "1\n3\n2\n")]).0, "3\n2\n1\n");
        assert_eq!(
            run("uniq", &["-c"], &[("-", "a\na\nb\na\n")]).0,
            "      2 a\n      1 b\n      1 a\n"
        );
    }
}
