//! Moving an MP4's or QuickTime file's index (its `moov` box) before its media (`mdat`), as
//! qt-faststart and ffmpeg's `-movflags +faststart` do, so that a player can start from the first
//! bytes. Nothing is re-encoded: the boxes are reordered, and the chunk offsets in the index
//! (`stco`, `co64`) that point into the media moved behind it grow by the index's size. If one
//! would no longer fit in 32 bits, every `stco` becomes a `co64`.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// The largest index read into memory.
const MOOV_MAX: u64 = 256 * 1024 * 1024;

/// What making a file faststart did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Its index already came first: nothing was written.
    Already,
    /// The index, of this many bytes, now comes before the media.
    Moved(u64),
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// A top-level box: its type, where it starts, and its size, header included.
struct TopBox {
    kind: [u8; 4],
    start: u64,
    size: u64,
}

fn top_boxes(file: &mut File, length: u64) -> Result<Vec<TopBox>, String> {
    let mut boxes = Vec::new();
    let mut offset = 0;
    while offset < length {
        if boxes.len() > 100_000 {
            return Err("too many boxes".into());
        }
        let mut header = [0u8; 16];
        file.seek(SeekFrom::Start(offset)).map_err(err)?;
        let read = Read::by_ref(file).take(16).read(&mut header).map_err(err)?;
        if read < 8 {
            return Err("a box header is cut short".into());
        }
        let kind = [header[4], header[5], header[6], header[7]];
        let size = match u32::from_be_bytes([header[0], header[1], header[2], header[3]]) {
            0 => length - offset,
            1 if read == 16 => u64::from_be_bytes(header[8..16].try_into().unwrap()),
            1 => return Err("a large box header is cut short".into()),
            size => u64::from(size),
        };
        if size < 8 || offset + size > length {
            return Err("a box's size runs past the end of the file".into());
        }
        boxes.push(TopBox {
            kind,
            start: offset,
            size,
        });
        offset += size;
    }
    Ok(boxes)
}

/// Make the file at `input` faststart, writing the result to `output`, unless it already is.
pub fn faststart(input: &Path, output: &Path) -> Result<Outcome, String> {
    let mut file = File::open(input).map_err(err)?;
    let length = file.metadata().map_err(err)?.len();
    let boxes = top_boxes(&mut file, length)
        .map_err(|e| format!("not a readable MP4 or QuickTime file: {e}"))?;
    let first = |kind: &[u8; 4]| boxes.iter().position(|b| &b.kind == kind);
    let (Some(moov), Some(mdat)) = (first(b"moov"), first(b"mdat")) else {
        return Err("not an MP4 or QuickTime file: it has no moov and mdat boxes".into());
    };
    if moov < mdat {
        return Ok(Outcome::Already);
    }
    if boxes.iter().filter(|b| &b.kind == b"moov").count() > 1 {
        return Err("it has more than one moov box".into());
    }
    if first(b"moof").is_some() {
        return Err("a fragmented MP4 cannot be reordered so; it streams as it is".into());
    }
    let (index, media) = (&boxes[moov], &boxes[mdat]);
    if index.size > MOOV_MAX {
        return Err("its index is larger than 256 MB".into());
    }
    let mut moov_bytes = vec![0u8; index.size as usize];
    file.seek(SeekFrom::Start(index.start)).map_err(err)?;
    file.read_exact(&mut moov_bytes).map_err(err)?;
    let moved = relocated(&moov_bytes, media.start, index.start)?;
    let mut out = BufWriter::new(File::create(output).map_err(err)?);
    // What comes before the media, the index, the media up to where the index was, and what
    // followed the index.
    copy(&mut file, &mut out, 0, media.start)?;
    out.write_all(&moved).map_err(err)?;
    copy(&mut file, &mut out, media.start, index.start)?;
    copy(&mut file, &mut out, index.start + index.size, length)?;
    out.flush().map_err(err)?;
    Ok(Outcome::Moved(moved.len() as u64))
}

/// Bytes `from..to` of `file`, written to `out`.
fn copy(file: &mut File, out: &mut impl Write, from: u64, to: u64) -> Result<(), String> {
    file.seek(SeekFrom::Start(from)).map_err(err)?;
    io::copy(&mut Read::by_ref(file).take(to - from), out).map_err(err)?;
    Ok(())
}

/// The index `moov`, a whole moov box, as it is once moved from `moov_at` to `insert_at`:
/// offsets into what lies between grow by its new size.
fn relocated(moov: &[u8], insert_at: u64, moov_at: u64) -> Result<Vec<u8>, String> {
    // Its size does not depend on the offsets, only on whether stco boxes become co64.
    let fits = |upgrade| -> Result<Option<Vec<u8>>, String> {
        let size = rebuild(moov, 0, insert_at, moov_at, upgrade)?
            .ok_or_else(|| "an offset does not fit".to_string())?
            .len() as u64;
        rebuild(moov, size, insert_at, moov_at, upgrade)
    };
    match fits(false) {
        Ok(Some(bytes)) => Ok(bytes),
        _ => fits(true)?.ok_or_else(|| "an offset does not fit even in 64 bits".into()),
    }
}

/// The boxes in `bytes`, rebuilt with their chunk offsets moved by `shift` where they point
/// into `insert_at..moov_at`; with `upgrade`, stco boxes as co64. None if an offset in an stco
/// box would no longer fit in 32 bits.
fn rebuild(
    bytes: &[u8],
    shift: u64,
    insert_at: u64,
    moov_at: u64,
    upgrade: bool,
) -> Result<Option<Vec<u8>>, String> {
    let moved = |offset: u64| {
        if (insert_at..moov_at).contains(&offset) {
            offset + shift
        } else {
            offset
        }
    };
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if rest.len() < 8 {
            return Err("a box in the index is cut short".into());
        }
        let mut kind: [u8; 4] = rest[4..8].try_into().unwrap();
        let (size, header) = match u32::from_be_bytes(rest[..4].try_into().unwrap()) {
            0 => (rest.len(), 8),
            1 if rest.len() >= 16 => (
                usize::try_from(u64::from_be_bytes(rest[8..16].try_into().unwrap()))
                    .map_err(err)?,
                16,
            ),
            1 => return Err("a large box in the index is cut short".into()),
            size => (size as usize, 8),
        };
        if size < header || size > rest.len() {
            return Err("a box in the index runs past its end".into());
        }
        let body = &rest[header..size];
        let body = match &kind {
            // The containers that lead to the chunk offsets.
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => {
                match rebuild(body, shift, insert_at, moov_at, upgrade)? {
                    Some(body) => body,
                    None => return Ok(None),
                }
            }
            b"stco" | b"co64" => {
                let wide = &kind == b"co64";
                if body.len() < 8 {
                    return Err("a chunk offset box is cut short".into());
                }
                let count = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
                let width = if wide { 8 } else { 4 };
                if body.len() < 8 + count * width {
                    return Err("a chunk offset box is cut short".into());
                }
                let into = wide || upgrade;
                let mut patched = body[..8].to_vec();
                for entry in body[8..8 + count * width].chunks(width) {
                    let offset = if wide {
                        u64::from_be_bytes(entry.try_into().unwrap())
                    } else {
                        u64::from(u32::from_be_bytes(entry.try_into().unwrap()))
                    };
                    let offset = moved(offset);
                    if into {
                        patched.extend_from_slice(&offset.to_be_bytes());
                    } else {
                        let Ok(offset) = u32::try_from(offset) else {
                            return Ok(None);
                        };
                        patched.extend_from_slice(&offset.to_be_bytes());
                    }
                }
                if into {
                    kind = *b"co64";
                }
                patched
            }
            _ => body.to_vec(),
        };
        let total = body.len() as u64 + 8;
        match u32::try_from(total) {
            Ok(total) => out.extend_from_slice(&total.to_be_bytes()),
            Err(_) => {
                out.extend_from_slice(&1u32.to_be_bytes());
                out.extend_from_slice(&kind);
                out.extend_from_slice(&(total + 8).to_be_bytes());
                out.extend_from_slice(&body);
                i += size;
                continue;
            }
        }
        out.extend_from_slice(&kind);
        out.extend_from_slice(&body);
        i += size;
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }
    /// An index whose one track's chunks start at `offsets`, in an stco, or a co64 if `wide`.
    fn index(offsets: &[u64], wide: bool) -> Vec<u8> {
        let mut table = vec![0, 0, 0, 0];
        table.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for offset in offsets {
            if wide {
                table.extend_from_slice(&offset.to_be_bytes());
            } else {
                table.extend_from_slice(&(*offset as u32).to_be_bytes());
            }
        }
        let table = boxed(if wide { b"co64" } else { b"stco" }, &table);
        let stbl = boxed(b"stbl", &[boxed(b"stsd", &[0; 8]), table].concat());
        let minf = boxed(b"minf", &stbl);
        let mdia = boxed(b"mdia", &[boxed(b"mdhd", &[0; 4]), minf].concat());
        let trak = boxed(b"trak", &[boxed(b"tkhd", &[0; 4]), mdia].concat());
        boxed(b"moov", &[boxed(b"mvhd", &[0; 4]), trak].concat())
    }
    /// The chunk offsets in the index `moov`.
    fn offsets(moov: &[u8]) -> (bool, Vec<u64>) {
        let at = |kind: &[u8]| moov.windows(4).position(|w| w == kind);
        let (wide, start) = match (at(b"stco"), at(b"co64")) {
            (Some(i), _) => (false, i + 4),
            (_, Some(i)) => (true, i + 4),
            _ => panic!("no chunk offsets"),
        };
        let count = u32::from_be_bytes(moov[start + 4..start + 8].try_into().unwrap()) as usize;
        let width = if wide { 8 } else { 4 };
        let entries = &moov[start + 8..start + 8 + count * width];
        let values = entries
            .chunks(width)
            .map(|e| {
                if wide {
                    u64::from_be_bytes(e.try_into().unwrap())
                } else {
                    u64::from(u32::from_be_bytes(e.try_into().unwrap()))
                }
            })
            .collect();
        (wide, values)
    }

    #[test]
    fn the_index_moves_before_the_media_and_its_offsets_follow() {
        for wide in [false, true] {
            let dir = std::env::temp_dir().join(format!("faststart-{wide}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let ftyp = boxed(b"ftyp", b"isom\0\0\0\0isom");
            let media: Vec<u8> = (0..200u8).collect();
            let mdat = boxed(b"mdat", &media);
            let first = (ftyp.len() + 8) as u64;
            let moov = index(&[first, first + 100], wide);
            let input = [ftyp.clone(), mdat, moov.clone(), boxed(b"free", &[0; 4])].concat();
            std::fs::write(dir.join("in"), &input).unwrap();
            assert_eq!(
                faststart(&dir.join("in"), &dir.join("out")).unwrap(),
                Outcome::Moved(moov.len() as u64)
            );
            let out = std::fs::read(dir.join("out")).unwrap();
            assert_eq!(out.len(), input.len());
            assert_eq!(&out[ftyp.len() + 4..ftyp.len() + 8], b"moov");
            let (_, moved) = offsets(&out[ftyp.len()..ftyp.len() + moov.len()]);
            // Each offset still points at the same media.
            for (old, new) in [first, first + 100].iter().zip(&moved) {
                assert_eq!(out[*new as usize], input[*old as usize]);
            }
            // Done once, it is done.
            std::fs::write(dir.join("in"), &out).unwrap();
            assert_eq!(
                faststart(&dir.join("in"), &dir.join("again")).unwrap(),
                Outcome::Already
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_real_video_with_its_index_last_comes_out_faststart() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
        let out = std::env::var("FASTSTART_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::temp_dir().join(format!("faststart-real-{}", std::process::id()))
            });
        let moved = faststart(&fixtures.join("tail-moov.mp4"), &out).unwrap();
        assert!(matches!(moved, Outcome::Moved(_)));
        let mut file = File::open(&out).unwrap();
        let length = file.metadata().unwrap().len();
        let kinds: Vec<[u8; 4]> = top_boxes(&mut file, length)
            .unwrap()
            .iter()
            .map(|b| b.kind)
            .collect();
        let at = |kind: &[u8; 4]| kinds.iter().position(|k| k == kind).unwrap();
        assert!(at(b"moov") < at(b"mdat"));
        assert_eq!(
            faststart(
                &fixtures.join("faststart.mp4"),
                &out.with_extension("again")
            )
            .unwrap(),
            Outcome::Already
        );
        if std::env::var("FASTSTART_OUT").is_err() {
            std::fs::remove_file(&out).unwrap();
        }
    }

    #[test]
    fn offsets_that_outgrow_32_bits_become_64_bit_ones() {
        let near = u64::from(u32::MAX) - 10;
        let moov = index(&[100, near], false);
        // As if the index were moved from beyond 4 GB to byte 50.
        let moved = relocated(&moov, 50, near + 1000).unwrap();
        let (wide, values) = offsets(&moved);
        assert!(wide);
        let size = moved.len() as u64;
        assert_eq!(values, [100 + size, near + size]);
        assert_eq!(size, moov.len() as u64 + 8);
    }

    #[test]
    fn what_is_not_an_mp4_is_refused() {
        let dir = std::env::temp_dir().join(format!("faststart-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("text"), b"just some words, not boxes at all").unwrap();
        assert!(faststart(&dir.join("text"), &dir.join("out")).is_err());
        std::fs::write(dir.join("nomoov"), boxed(b"ftyp", b"isom")).unwrap();
        assert!(
            faststart(&dir.join("nomoov"), &dir.join("out"))
                .unwrap_err()
                .contains("no moov")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
