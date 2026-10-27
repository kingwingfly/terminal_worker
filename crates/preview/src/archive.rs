//! The members of ZIP and tar archives, gzipped too, as the viewer lists them. Listing a gzipped
//! tar decompresses all of it, as its headers lie between its members' contents, so it counts
//! what it decompresses: an archive that expands to more than [`EXPANDED_MAX`] is not listed,
//! however small it is.

use std::io::{self, Cursor, Read};

use flate2::read::MultiGzDecoder;

/// The most members listed.
pub const ENTRIES_MAX: usize = 10_000;
/// The most bytes a listing reads, decompressed: as much as the terminal extracts.
pub const EXPANDED_MAX: u64 = 1_000_000_000;
/// What a listing that would read more says.
const EXPANDS: &str = "the archive expands to more than 1 GB, more than the terminal lists";

/// A file or directory in an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub path: String,
    pub size: u64,
    pub directory: bool,
}

/// The first [`ENTRIES_MAX`] members of the archive `data`, and whether it has more.
pub fn list(data: &[u8]) -> Result<(Vec<Member>, bool), String> {
    match data {
        [b'P', b'K', 3, 4, ..] | [b'P', b'K', 5, 6, ..] => zip(data),
        [0x1F, 0x8B, ..] => tar(Budget {
            reader: MultiGzDecoder::new(data),
            left: EXPANDED_MAX,
        }),
        _ => tar(data),
    }
}

/// Whether `data` starts as an archive [`list`] reads.
pub fn is_archive(data: &[u8]) -> bool {
    matches!(
        data,
        [b'P', b'K', 3, 4, ..] | [b'P', b'K', 5, 6, ..] | [0x1F, 0x8B, ..]
    ) || data.get(257..262) == Some(b"ustar")
}

fn zip(data: &[u8]) -> Result<(Vec<Member>, bool), String> {
    let mut archive = ::zip::ZipArchive::new(Cursor::new(data))
        .map_err(|e| format!("not a readable ZIP archive: {e}"))?;
    let members = (0..archive.len().min(ENTRIES_MAX))
        .map(|index| {
            let file = archive.by_index_raw(index).map_err(|e| e.to_string())?;
            Ok(Member {
                path: file.name().map_err(|e| e.to_string())?.into_owned(),
                size: file.size(),
                directory: file.is_dir(),
            })
        })
        .collect::<Result<_, String>>()?;
    Ok((members, archive.len() > ENTRIES_MAX))
}

fn tar(reader: impl Read) -> Result<(Vec<Member>, bool), String> {
    let unreadable = |e: io::Error| match e.to_string() {
        expanded if expanded.contains(EXPANDS) => EXPANDS.to_string(),
        e => format!("not a readable tar archive: {e}"),
    };
    let mut archive = ::tar::Archive::new(reader);
    let mut members = Vec::new();
    for entry in archive.entries().map_err(unreadable)? {
        if members.len() == ENTRIES_MAX {
            return Ok((members, true));
        }
        let entry = entry.map_err(unreadable)?;
        members.push(Member {
            path: entry
                .path()
                .map_err(unreadable)?
                .to_string_lossy()
                .into_owned(),
            size: entry.header().size().map_err(unreadable)?,
            directory: entry.header().entry_type().is_dir(),
        });
    }
    Ok((members, false))
}

/// A reader of at most `left` more bytes, which fails beyond them.
struct Budget<R> {
    reader: R,
    left: u64,
}

impl<R: Read> Read for Budget<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            // The end, if it is there.
            return match self.reader.read(&mut [0])? {
                0 => Ok(0),
                _ => Err(io::Error::other(EXPANDS)),
            };
        }
        let most = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let read = self.reader.read(&mut buf[..most])?;
        self.left -= read as u64;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn tarball(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = ::tar::Builder::new(vec![]);
        for (name, content) in members {
            let mut header = ::tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_cksum();
            tar.append_data(&mut header, name, *content).unwrap();
        }
        tar.into_inner().unwrap()
    }
    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(vec![], flate2::Compression::default());
        gz.write_all(data).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn archives_list_their_members() {
        let mut zip = ::zip::ZipWriter::new(Cursor::new(vec![]));
        zip.add_directory("docs/", ::zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.start_file("docs/a.txt", ::zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"hello").unwrap();
        let data = zip.finish().unwrap().into_inner();
        assert!(is_archive(&data));
        assert_eq!(
            list(&data).unwrap(),
            (
                vec![
                    Member {
                        path: "docs/".into(),
                        size: 0,
                        directory: true
                    },
                    Member {
                        path: "docs/a.txt".into(),
                        size: 5,
                        directory: false
                    },
                ],
                false
            )
        );

        let tar = tarball(&[("a.txt", b"hello")]);
        let expected = (
            vec![Member {
                path: "a.txt".into(),
                size: 5,
                directory: false,
            }],
            false,
        );
        assert_eq!(list(&gzip(&tar)).unwrap(), expected);
        assert_eq!(list(&tar).unwrap(), expected);
        assert!(is_archive(&tar) && is_archive(&gzip(&tar)));
        assert!(!is_archive(b"plain text"));
    }

    #[test]
    fn a_gzipped_tar_that_expands_too_far_is_not_listed() {
        let mut budget = Budget {
            reader: &[7u8; 100][..],
            left: 100,
        };
        assert_eq!(io::copy(&mut budget, &mut io::sink()).unwrap(), 100);
        let mut budget = Budget {
            reader: &[7u8; 101][..],
            left: 100,
        };
        assert!(io::copy(&mut budget, &mut io::sink()).is_err());
        // The bytes between headers count too: a member is skipped by reading it.
        let tar = tarball(&[("big.bin", &[0u8; 4096]), ("after.txt", b"x")]);
        let gz = gzip(&tar);
        let small = Budget {
            reader: MultiGzDecoder::new(&gz[..]),
            left: 2048,
        };
        assert_eq!(super::tar(small).unwrap_err(), EXPANDS);
        let enough = Budget {
            reader: MultiGzDecoder::new(&gz[..]),
            left: tar.len() as u64,
        };
        assert_eq!(super::tar(enough).unwrap().0.len(), 2);
    }
}
