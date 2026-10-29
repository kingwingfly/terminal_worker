//! The names of /dev's devices (owner's request, 2026-10-28): presentations others follow,
//! /dev/present/NAME, and live sound others hear, /dev/snd/NAME, which the worker's `devices`
//! keeps.

/// Where presentations are ...
pub const PRESENT: &str = "/dev/present";
/// ... and live sound.
pub const SOUND: &str = "/dev/snd";

/// The longest a device's name may be, in bytes.
const NAME_MAX: usize = 255;

/// The device of `directory` a name given to `present` or `stream` names: NAME, or the whole
/// path, `directory`/NAME.
pub fn device_path(directory: &str, name: &str) -> Result<String, String> {
    let name = name.strip_prefix(&format!("{directory}/")).unwrap_or(name);
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > NAME_MAX
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Err(format!("Invalid device name: {name:?}"));
    }
    Ok(format!("{directory}/{name}"))
}

/// The name a device of the file at `path` takes unless given one: the file's, without its
/// extension (`.slides.md` whole).
pub fn default_name(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = match file.len().checked_sub(".slides.md".len()) {
        Some(at) if file.is_char_boundary(at) && file[at..].eq_ignore_ascii_case(".slides.md") => {
            &file[..at]
        }
        _ => file.rsplit_once('.').map_or(file, |(stem, _)| stem),
    };
    if stem.is_empty() { file } else { stem }.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_files_and_stay_in_their_directory() {
        assert_eq!(default_name("/talks/talk.slides.md"), "talk");
        assert_eq!(default_name("/talks/Talk.Slides.MD"), "Talk");
        assert_eq!(default_name("/talks/spéctrum.pdf"), "spéctrum");
        assert_eq!(default_name("/talks/talk.weba"), "talk");
        assert_eq!(default_name("/talks/notes"), "notes");
        assert_eq!(default_name("/talks/.slides.md"), ".slides.md");
        assert_eq!(device_path(PRESENT, "talk").unwrap(), "/dev/present/talk");
        let whole = device_path(PRESENT, "/dev/present/talk").unwrap();
        assert_eq!(whole, "/dev/present/talk");
        assert_eq!(
            device_path(SOUND, "/dev/snd/talk").unwrap(),
            "/dev/snd/talk"
        );
        for wrong in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a\nb",
            "/dev/present/",
            "/dev/snd/talk",
        ] {
            assert!(device_path(PRESENT, wrong).is_err(), "{wrong:?}");
        }
    }
}
