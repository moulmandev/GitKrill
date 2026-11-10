//! The fonts installed on this computer, for Preferences > Editor.
//!
//! [`scan`] lists every family in the system and user font folders with
//! whether it is monospaced. It reads only the few hundred bytes of each
//! file that say so (the table directory and the `name`, `post` and `OS/2`
//! tables), never whole fonts: a Windows font folder holds hundreds of
//! megabytes. [`load`] then reads the one font picked.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use eframe::egui::FontData;

/// One family as the font list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontEntry {
    pub family: String,
    pub monospace: bool,
    /// The file of its regular face, and the face's index in a collection.
    pub file: PathBuf,
    pub index: u32,
}

/// What the tables of one face say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Face {
    pub index: u32,
    /// The family ("Cascadia Mono"), the typographic one when there is one.
    pub family: String,
    /// The style within the family ("Regular", "Bold Italic").
    pub style: String,
    pub weight: u16,
    pub italic: bool,
    pub monospace: bool,
}

/// Where fonts are installed on this system.
pub fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = dirs::home_dir();
    if cfg!(windows) {
        if let Some(windir) = std::env::var_os("WINDIR") {
            dirs.push(Path::new(&windir).join("Fonts"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(Path::new(&local).join("Microsoft").join("Windows").join("Fonts"));
        }
    } else if cfg!(target_os = "macos") {
        dirs.push("/System/Library/Fonts".into());
        dirs.push("/Library/Fonts".into());
        if let Some(home) = &home {
            dirs.push(home.join("Library/Fonts"));
        }
    } else {
        dirs.push("/usr/share/fonts".into());
        dirs.push("/usr/local/share/fonts".into());
        if let Some(home) = &home {
            dirs.push(home.join(".local/share/fonts"));
            dirs.push(home.join(".fonts"));
        }
    }
    dirs
}

fn is_font_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ["ttf", "otf", "ttc", "otc"].iter().any(|known| ext.eq_ignore_ascii_case(known)))
}

fn font_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() && depth > 0 {
            font_files(&path, depth - 1, out);
        } else if is_font_file(&path) {
            out.push(path);
        }
    }
}

/// Every font family installed, once each (its most regular face), sorted
/// by name. Takes a moment: call it off the UI thread.
pub fn scan() -> Vec<FontEntry> {
    let mut files = Vec::new();
    for dir in font_dirs() {
        font_files(&dir, 4, &mut files);
    }
    let mut faces: Vec<(Face, PathBuf)> = Vec::new();
    for path in files {
        let Ok(mut file) = File::open(&path) else { continue };
        let mut read = |offset: u64, len: usize| read_at(&mut file, offset, len);
        for face in faces_with(&mut read) {
            faces.push((face, path.clone()));
        }
    }
    families(faces)
}

/// One entry per family, from faces found in files.
fn families(mut faces: Vec<(Face, PathBuf)>) -> Vec<FontEntry> {
    // Families macOS hides (".SF NS") and vertical CJK variants ("@MS Gothic").
    faces.retain(|(face, _)| !face.family.is_empty() && !face.family.starts_with(['.', '@']));
    faces.sort_by(|(a, path_a), (b, path_b)| {
        a.family
            .to_lowercase()
            .cmp(&b.family.to_lowercase())
            .then(regularity(a).cmp(&regularity(b)))
            .then(path_a.cmp(path_b))
            .then(a.index.cmp(&b.index))
    });
    let mut out: Vec<FontEntry> = Vec::new();
    for (face, path) in faces {
        match out.last_mut() {
            Some(last) if last.family.eq_ignore_ascii_case(&face.family) => last.monospace |= face.monospace,
            _ => out.push(FontEntry { family: face.family, monospace: face.monospace, file: path, index: face.index }),
        }
    }
    out
}

/// How far a face is from the family's regular one; lower is closer.
fn regularity(face: &Face) -> (bool, u16, bool) {
    (face.italic, face.weight.abs_diff(400), !face.style.eq_ignore_ascii_case("regular"))
}

/// The font `family`, from `file` (face `index`) when that file still holds
/// it, or else wherever it is installed. Checked to be a font egui can use.
pub fn load(family: &str, file: &Path, index: u32) -> Result<FontData, String> {
    let from = |path: &Path, index: u32| -> Option<FontData> {
        let bytes = std::fs::read(path).ok()?;
        let face = faces_in(&bytes).into_iter().find(|face| face.index == index)?;
        if !face.family.eq_ignore_ascii_case(family) {
            return None;
        }
        // egui gives up on (panics over) a font it cannot parse.
        skrifa::FontRef::from_index(&bytes, index).ok()?;
        let mut data = FontData::from_owned(bytes);
        data.index = index;
        Some(data)
    };
    if !file.as_os_str().is_empty()
        && let Some(data) = from(file, index)
    {
        return Ok(data);
    }
    let found = scan().into_iter().find(|entry| entry.family.eq_ignore_ascii_case(family));
    match found {
        Some(entry) => from(&entry.file, entry.index).ok_or_else(|| format!("{family} could not be read.")),
        None => Err(format!("{family} is not installed.")),
    }
}

// ---- reading font tables ------------------------------------------------------

/// No table this reads is anywhere near this big; a bigger one is junk.
const MAX_TABLE: usize = 1 << 20;
/// Faces in one collection file, at most.
const MAX_FACES: u32 = 64;

fn read_at(file: &mut File, offset: u64, len: usize) -> Option<Vec<u8>> {
    if len > MAX_TABLE {
        return None;
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = vec![0; len];
    file.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// The faces of a font file held in memory.
pub fn faces_in(bytes: &[u8]) -> Vec<Face> {
    let mut read = |offset: u64, len: usize| {
        let start = usize::try_from(offset).ok()?;
        bytes.get(start..start.checked_add(len)?).map(<[u8]>::to_vec)
    };
    faces_with(&mut read)
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// The faces of a font file, through `read(offset, len)`.
fn faces_with(read: &mut dyn FnMut(u64, usize) -> Option<Vec<u8>>) -> Vec<Face> {
    let Some(head) = read(0, 12) else { return Vec::new() };
    if &head[..4] == b"ttcf" {
        let count = u32_at(&head, 8).unwrap_or(0).min(MAX_FACES);
        let Some(offsets) = read(12, count as usize * 4) else { return Vec::new() };
        (0..count)
            .filter_map(|index| {
                let offset = u32_at(&offsets, index as usize * 4)?;
                face_at(read, u64::from(offset), index)
            })
            .collect()
    } else {
        face_at(read, 0, 0).into_iter().collect()
    }
}

/// The face whose table directory starts at `offset`.
fn face_at(read: &mut dyn FnMut(u64, usize) -> Option<Vec<u8>>, offset: u64, index: u32) -> Option<Face> {
    let header = read(offset, 12)?;
    let version = u32_at(&header, 0)?;
    // TrueType outlines, CFF outlines ("OTTO"), Apple's "true".
    if ![0x0001_0000, u32::from_be_bytes(*b"OTTO"), u32::from_be_bytes(*b"true")].contains(&version) {
        return None;
    }
    let tables = u16_at(&header, 4)? as usize;
    let records = read(offset + 12, tables * 16)?;
    let mut find = |tag: &[u8; 4], len: Option<usize>| -> Option<Vec<u8>> {
        let record = records.as_chunks::<16>().0.iter().find(|record| &record[..4] == tag)?;
        let (start, size) = (u32_at(record, 8)?, u32_at(record, 12)? as usize);
        read(u64::from(start), len.map_or(size, |len| len.min(size)))
    };
    let name = find(b"name", None)?;
    let post = find(b"post", Some(16));
    let os2 = find(b"OS/2", Some(64));

    let family = name_string(&name, 16).or_else(|| name_string(&name, 1))?;
    let style = name_string(&name, 17).or_else(|| name_string(&name, 2)).unwrap_or_default();
    let fixed_pitch = post.as_deref().and_then(|post| u32_at(post, 12)).is_some_and(|fixed| fixed != 0);
    // PANOSE: family type 2 is Latin text, proportion 9 is monospaced.
    let panose_mono = os2.as_deref().is_some_and(|os2| os2.get(32) == Some(&2) && os2.get(35) == Some(&9));
    let weight = os2.as_deref().and_then(|os2| u16_at(os2, 4)).unwrap_or(400);
    let selection = os2.as_deref().and_then(|os2| u16_at(os2, 62)).unwrap_or(0);
    let lower = style.to_lowercase();
    let italic = selection & 1 != 0 || lower.contains("italic") || lower.contains("oblique");
    Some(Face { index, family, style, weight, italic, monospace: fixed_pitch || panose_mono })
}

/// Name `id` from a `name` table, in English when there is a choice.
fn name_string(table: &[u8], id: u16) -> Option<String> {
    let count = u16_at(table, 2)? as usize;
    let strings = u16_at(table, 4)? as usize;
    let mut best: Option<(u8, String)> = None;
    for record in table.get(6..6 + count * 12)?.as_chunks::<12>().0 {
        let field = |at: usize| u16_at(record, at).unwrap_or(0);
        let (platform, encoding, language, name) = (field(0), field(2), field(4), field(6));
        if name != id {
            continue;
        }
        let (len, at) = (field(8) as usize, field(10) as usize);
        let Some(raw) = table.get(strings + at..strings + at + len) else { continue };
        let (score, text) = match (platform, encoding) {
            // Windows, Unicode: UTF-16BE. US English first.
            (3, 1 | 10) => (if language == 0x409 { 4 } else { 2 }, utf16_be(raw)),
            // Unicode platform: UTF-16BE.
            (0, _) => (3, utf16_be(raw)),
            // Macintosh, Roman: ASCII for the names that matter here.
            (1, 0) if language == 0 => (1, raw.iter().map(|&b| char::from(b)).collect()),
            _ => continue,
        };
        let text = text.trim().to_owned();
        if !text.is_empty() && best.as_ref().is_none_or(|(known, _)| score > *known) {
            best = Some((score, text));
        }
    }
    best.map(|(_, text)| text)
}

fn utf16_be(raw: &[u8]) -> String {
    let units = raw.as_chunks::<2>().0.iter().map(|pair| u16::from_be_bytes(*pair));
    char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes of a font egui itself ships with.
    fn bundled(name: &str) -> Vec<u8> {
        let fonts = eframe::egui::FontDefinitions::default();
        fonts.font_data[name].font.to_vec()
    }

    #[test]
    fn names_and_monospace_come_from_the_font_tables() {
        let hack = faces_in(&bundled("Hack"));
        assert_eq!(hack.len(), 1);
        assert_eq!((hack[0].family.as_str(), hack[0].style.as_str()), ("Hack", "Regular"));
        assert!(hack[0].monospace && !hack[0].italic);
        assert_eq!(hack[0].weight, 400);

        let ubuntu = faces_in(&bundled("Ubuntu-Light"));
        assert_eq!(ubuntu.len(), 1);
        assert!(ubuntu[0].family.starts_with("Ubuntu"), "{:?}", ubuntu[0]);
        assert!(!ubuntu[0].monospace);

        // Not a font at all.
        assert!(faces_in(b"definitely not a font").is_empty());
        assert!(faces_in(&[]).is_empty());
    }

    /// A collection of two fonts, written the way `.ttc` files are: the
    /// header, then each face's tables at offsets from the start of the file.
    #[test]
    fn every_face_of_a_collection_is_read() {
        let hack = bundled("Hack");
        let ubuntu = bundled("Ubuntu-Light");
        let mut ttc = Vec::new();
        ttc.extend_from_slice(b"ttcf");
        ttc.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        ttc.extend_from_slice(&2u32.to_be_bytes());
        let header = 12 + 8;
        let second = header + hack.len();
        ttc.extend_from_slice(&(header as u32).to_be_bytes());
        ttc.extend_from_slice(&(second as u32).to_be_bytes());
        // Table offsets inside each face are relative to its own start in
        // a standalone file; move them to where the face now sits.
        for (font, at) in [(&hack, header), (&ubuntu, second)] {
            let mut moved = font.clone();
            let tables = u16_at(font, 4).unwrap() as usize;
            for record in 0..tables {
                let field = 12 + record * 16 + 8;
                let offset = u32_at(font, field).unwrap() + at as u32;
                moved[field..field + 4].copy_from_slice(&offset.to_be_bytes());
            }
            ttc.extend_from_slice(&moved);
        }
        let faces = faces_in(&ttc);
        assert_eq!(faces.len(), 2);
        assert_eq!((faces[0].index, faces[0].family.as_str(), faces[0].monospace), (0, "Hack", true));
        assert_eq!((faces[1].index, faces[1].monospace), (1, false));
        // egui (skrifa) agrees on the collection's second face.
        assert!(skrifa::FontRef::from_index(&ttc, 1).is_ok());
    }

    #[test]
    fn the_regular_face_stands_for_its_family() {
        let face = |family: &str, style: &str, weight: u16, italic: bool, monospace: bool| Face {
            index: 0,
            family: family.to_owned(),
            style: style.to_owned(),
            weight,
            italic,
            monospace,
        };
        let entries = families(vec![
            (face("Mono Sans", "Bold", 700, false, true), "b.ttf".into()),
            (face("Mono Sans", "Regular", 400, false, true), "r.ttf".into()),
            (face("Mono Sans", "Italic", 400, true, true), "i.ttf".into()),
            (face("alpha", "Regular", 400, false, false), "a.ttf".into()),
            (face(".Hidden", "Regular", 400, false, false), "h.ttf".into()),
            (face("@Vertical", "Regular", 400, false, false), "v.ttf".into()),
        ]);
        let names: Vec<_> = entries.iter().map(|e| (e.family.as_str(), e.file.to_str().unwrap())).collect();
        assert_eq!(names, [("alpha", "a.ttf"), ("Mono Sans", "r.ttf")]);
        assert!(entries[1].monospace);
    }

    #[test]
    fn loading_checks_the_file_still_holds_the_family() {
        let dir = std::env::temp_dir().join(format!("gitkrill-font-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Hack-Regular.ttf");
        std::fs::write(&file, bundled("Hack")).unwrap();
        let data = load("Hack", &file, 0).unwrap();
        assert_eq!((data.font.len(), data.index), (bundled("Hack").len(), 0));
        // The file holds another family now, and none is installed by that name.
        let error = load("No Such Font Family 12345", &file, 0).unwrap_err();
        assert!(error.contains("not installed"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
