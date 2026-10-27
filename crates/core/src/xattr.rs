//! Extended attributes, as setfattr and getfattr show and change them: `user.*` for anyone who
//! may read (to get) or write (to set) a file or directory, `trusted.*` for root only. Values are
//! bytes, written as text, `0x` and hexadecimal digits, or `0s` and base64.

/// The longest name, ...
pub const NAME_MAX: usize = 255;
/// ... the longest value, and the most one node holds, names and values together, as an ext4
/// block does.
pub const VALUE_MAX: usize = 4096;
pub const TOTAL_MAX: usize = 4096;

/// The namespace of an attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Namespace {
    User,
    Trusted,
}

/// The namespace of the attribute `name`, which must have one this filesystem keeps.
pub fn namespace(name: &str) -> Result<Namespace, String> {
    if name.len() > NAME_MAX {
        return Err(format!("{name}: Numerical result out of range"));
    }
    let (space, rest) = name.split_once('.').unwrap_or((name, ""));
    if rest.is_empty() {
        return Err(format!("{name}: Operation not supported"));
    }
    match space {
        "user" => Ok(Namespace::User),
        "trusted" => Ok(Namespace::Trusted),
        _ => Err(format!("{name}: Operation not supported")),
    }
}

/// The bytes `text` stands for, as setfattr -v reads it: `0x...` hexadecimal, `0s...` base64,
/// or text, quoted or not, with `\` escapes in quotes.
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    let invalid = || format!("Invalid value: {text}");
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        // Byte slicing below needs ASCII: `0xaé` would split a character and panic.
        if !hex.is_ascii() || hex.len() % 2 != 0 {
            return Err(invalid());
        }
        return (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| invalid()))
            .collect();
    }
    if let Some(encoded) = text.strip_prefix("0s").or_else(|| text.strip_prefix("0S")) {
        return base64(encoded).ok_or_else(invalid);
    }
    let Some(quoted) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) else {
        return Ok(text.as_bytes().to_vec());
    };
    let mut out = Vec::new();
    let mut bytes = quoted.bytes().peekable();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        // \\, \", \n and octal \ooo.
        match bytes.next().ok_or_else(invalid)? {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            d @ b'0'..=b'7' => {
                let mut value = u32::from(d - b'0');
                for _ in 0..2 {
                    match bytes.peek() {
                        Some(d @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(d - b'0');
                            bytes.next();
                        }
                        _ => break,
                    }
                }
                out.push(u8::try_from(value).map_err(|_| invalid())?);
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// How getfattr writes `value`: `"text"`, with octal escapes for what is not printable,
/// `0x...` or `0s...`, as `encoding` says (`text`, `hex` or `base64`).
pub fn encode(value: &[u8], encoding: &str) -> String {
    match encoding {
        "hex" => format!(
            "0x{}",
            value.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ),
        "base64" => format!("0s{}", to_base64(value)),
        _ => {
            let mut out = String::from("\"");
            for &b in value {
                match b {
                    b'"' | b'\\' => {
                        out.push('\\');
                        out.push(b as char);
                    }
                    0x20..=0x7e => out.push(b as char),
                    _ => out.push_str(&format!("\\{b:03o}")),
                }
            }
            out.push('"');
            out
        }
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn to_base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::new();
    let mut n = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        n = n << 6 | ALPHABET.iter().position(|a| *a == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((n >> bits & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_read_and_write_as_the_attr_tools_do() {
        assert_eq!(decode("hello").unwrap(), b"hello");
        assert_eq!(decode("\"a\\\"b\\012\"").unwrap(), b"a\"b\n");
        assert_eq!(decode("0x00ff").unwrap(), [0, 255]);
        assert_eq!(decode("0saGk=").unwrap(), b"hi");
        assert!(decode("0x0").is_err());
        assert!(decode("0xaé").is_err());
        assert_eq!(encode(b"say \"hi\"\n", "text"), "\"say \\\"hi\\\"\\012\"");
        assert_eq!(encode(&[0, 255], "hex"), "0x00ff");
        assert_eq!(encode(b"hi", "base64"), "0saGk=");
        assert_eq!(
            decode(&encode(b"any\x01bytes", "base64")).unwrap(),
            b"any\x01bytes"
        );
    }

    #[test]
    fn only_user_and_trusted_attributes_are_kept() {
        assert_eq!(namespace("user.comment"), Ok(Namespace::User));
        assert_eq!(namespace("trusted.tag"), Ok(Namespace::Trusted));
        for name in [
            "security.selinux",
            "system.posix_acl_access",
            "user.",
            "comment",
        ] {
            assert!(namespace(name).is_err(), "{name}");
        }
    }
}
