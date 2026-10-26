//! Wildcards, as a shell expands them in the words of a command line: `*` matches any run of
//! characters in a name, `?` one character, and `[...]` one of a set (`[a-z]`, `[!.]`). A name
//! that starts with a dot matches only a pattern that starts with one. Wildcards that were
//! quoted or escaped are written `\*` in a pattern, and match only themselves.

/// One part of a pattern, between its slashes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    /// A name, as it is: quoted, escaped, or without wildcards.
    Name(String),
    /// A pattern, which matches names in a directory.
    Pattern(String),
}

/// The parts of `pattern` between its slashes; whether it is absolute is whether it starts with
/// one. A slash is never a wildcard, so no part spans two names.
pub fn parts(pattern: &str) -> Vec<Part> {
    pattern
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| {
            if has_wildcards(part) {
                Part::Pattern(part.to_string())
            } else {
                Part::Name(unescape(part))
            }
        })
        .collect()
}

/// Whether `pattern` has a wildcard that is not escaped.
pub fn has_wildcards(pattern: &str) -> bool {
    let mut escaped = false;
    for c in pattern.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '*' | '?' | '[' => return true,
            _ => {}
        }
    }
    false
}

/// `pattern` with its escapes removed: the text a word without wildcards stands for.
pub fn unescape(pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            c => out.push(c),
        }
    }
    out
}

#[derive(Debug)]
enum Token {
    Char(char),
    Any,
    Star,
    /// One character in, or with `negated` not in, these ranges.
    Set {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

fn tokens(pattern: &str) -> Vec<Token> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                tokens.push(Token::Char(chars[i + 1]));
                i += 2;
            }
            '*' => {
                // Stars in a row are one.
                if !matches!(tokens.last(), Some(Token::Star)) {
                    tokens.push(Token::Star);
                }
                i += 1;
            }
            '?' => {
                tokens.push(Token::Any);
                i += 1;
            }
            '[' => match set(&chars[i + 1..]) {
                Some((token, used)) => {
                    tokens.push(token);
                    i += 1 + used;
                }
                // A [ that closes no set is itself.
                None => {
                    tokens.push(Token::Char('['));
                    i += 1;
                }
            },
            c => {
                tokens.push(Token::Char(c));
                i += 1;
            }
        }
    }
    tokens
}

/// The set that `chars`, which follow a `[`, start, and how many of them it takes, up to and
/// including its `]`: a `]` first in the set is one of its characters.
fn set(chars: &[char]) -> Option<(Token, usize)> {
    let mut i = 0;
    let negated = matches!(chars.first(), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut ranges = Vec::new();
    let start = i;
    while i < chars.len() {
        let mut c = chars[i];
        if c == ']' && i > start {
            return Some((Token::Set { negated, ranges }, i + 1));
        }
        if c == '\\' && i + 1 < chars.len() {
            i += 1;
            c = chars[i];
        }
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|&end| end != ']') {
            let mut end = i + 2;
            if chars[end] == '\\' && end + 1 < chars.len() {
                end += 1;
            }
            ranges.push((c, chars[end]));
            i = end + 1;
        } else {
            ranges.push((c, c));
            i += 1;
        }
    }
    None
}

/// Whether `name`, a name in a directory, matches `pattern`, one part of a pattern.
pub fn matches(pattern: &str, name: &str) -> bool {
    if name.starts_with('.') && !pattern.starts_with('.') && !pattern.starts_with("\\.") {
        return false;
    }
    let tokens = tokens(pattern);
    let name: Vec<char> = name.chars().collect();
    let one = |token: &Token, c: char| match token {
        Token::Char(t) => *t == c,
        Token::Any => true,
        Token::Set { negated, ranges } => {
            ranges.iter().any(|&(low, high)| low <= c && c <= high) != *negated
        }
        Token::Star => false,
    };
    // Greedy, going back to the last star when what follows it does not match.
    let (mut t, mut n) = (0, 0);
    let mut star = None;
    while n < name.len() {
        if t < tokens.len() && matches!(tokens[t], Token::Star) {
            star = Some((t, n));
            t += 1;
        } else if t < tokens.len() && one(&tokens[t], name[n]) {
            t += 1;
            n += 1;
        } else if let Some((at, from)) = star {
            t = at + 1;
            n = from + 1;
            star = Some((at, from + 1));
        } else {
            return false;
        }
    }
    tokens[t..].iter().all(|token| matches!(token, Token::Star))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_match_names_as_a_shell_does() {
        for (pattern, name) in [
            ("*", "notes.md"),
            ("*.md", "a.b.md"),
            ("n?tes.*", "notes.md"),
            ("[ab]*", "beta"),
            ("[a-c]x", "bx"),
            ("[!a]x", "bx"),
            ("[^a]x", "bx"),
            ("[]]x", "]x"),
            ("*é*", "spéctrum"),
            (".*", ".hidden"),
            ("\\.*", ".hidden"),
            ("a\\*", "a*"),
            ("[a", "[a"),
            ("**", "x"),
            ("*a*b*", "xaybz"),
        ] {
            assert!(matches(pattern, name), "{pattern} {name}");
        }
        for (pattern, name) in [
            ("*", ".hidden"),
            ("?hidden", ".hidden"),
            ("[.]hidden", ".hidden"),
            ("*.md", "notes.txt"),
            ("[!a]x", "ax"),
            ("a\\*", "ab"),
            ("?", ""),
            ("a?", "a"),
            ("*a*b", "xaybz"),
        ] {
            assert!(!matches(pattern, name), "{pattern} {name}");
        }
    }

    #[test]
    fn patterns_split_into_names_and_patterns() {
        assert_eq!(
            parts("/docs/*.md"),
            [Part::Name("docs".into()), Part::Pattern("*.md".into())]
        );
        assert_eq!(
            parts("a\\*b/c?"),
            [Part::Name("a*b".into()), Part::Pattern("c?".into())]
        );
        assert!(has_wildcards("x[1]"));
        assert!(!has_wildcards("x\\[1\\]"));
        assert_eq!(unescape("x\\[1\\]"), "x[1]");
    }
}
