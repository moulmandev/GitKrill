//! Images for the diff view's preview of image files: both versions read
//! from the object database (or the working tree) and decoded on the reader
//! thread, so the UI only has to upload pixels. SVG files are drawn with
//! resvg the same way, beside their text diff.

use std::fmt;
use std::io::Cursor;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use resvg::usvg::fontdb;
use resvg::{tiny_skia, usvg};

use git2::{Oid, Repository};

/// Files larger than this are not decoded.
const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Images wider or taller than this are not decoded (a 1 KB file can claim
/// to be 100 000 pixels wide).
const MAX_DIMENSION: u32 = 16_384;
/// Longest side of the copy kept for display: enough for a full-screen view
/// on a large monitor, at most 16 MB of pixels.
const MAX_SIDE: u32 = 2048;

/// The extensions worth trying to show as a picture.
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "ico"];

/// Whether `path` names a format the preview can decode.
pub fn is_image_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| EXTENSIONS.iter().any(|known| known.eq_ignore_ascii_case(ext)))
}

/// Whether `path` names an SVG file: text that the preview can also draw.
pub fn is_svg_path(path: &str) -> bool {
    Path::new(path).extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
}

/// Both versions of an image file. A side is `None` when the file did not
/// exist there (added or deleted).
#[derive(Default)]
pub struct ImageDiff {
    pub old: Option<Side>,
    pub new: Option<Side>,
}

impl fmt::Debug for ImageDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageDiff").field("old", &self.old).field("new", &self.new).finish()
    }
}

/// One version of the file.
pub struct Side {
    /// Size of the file.
    pub bytes: u64,
    /// The decoded picture, or why there is none.
    pub picture: Result<Picture, String>,
}

impl fmt::Debug for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let picture = self.picture.as_ref().map(|p| (p.format, p.width, p.height));
        f.debug_struct("Side").field("bytes", &self.bytes).field("picture", &picture).finish()
    }
}

pub struct Picture {
    /// Different for every decode, so the UI can tell when to upload anew.
    pub id: u64,
    /// "PNG", "JPEG", ...
    pub format: &'static str,
    /// The image's own size in pixels.
    pub width: u32,
    pub height: u32,
    /// RGBA pixels, `rgba_size[0]` by `rgba_size[1]`: the image itself, or
    /// a copy scaled down to [`MAX_SIDE`].
    pub rgba: Vec<u8>,
    pub rgba_size: [usize; 2],
}

/// Reads both versions of an image file. `old` and `new` are blob ids from
/// the diff (zero when the side is missing or not hashed); `workdir_file`
/// is read for the new side when it has no blob, as for unstaged changes.
pub fn load(repo: &Repository, old: Option<Oid>, new: Option<Oid>, workdir_file: Option<&Path>) -> ImageDiff {
    load_with(repo, old, new, workdir_file, decode)
}

/// Reads and draws both versions of an SVG file, as [`load`] does images.
pub fn load_svg(repo: &Repository, old: Option<Oid>, new: Option<Oid>, workdir_file: Option<&Path>) -> ImageDiff {
    load_with(repo, old, new, workdir_file, decode_svg)
}

fn load_with(
    repo: &Repository,
    old: Option<Oid>,
    new: Option<Oid>,
    workdir_file: Option<&Path>,
    decode: fn(&[u8]) -> Side,
) -> ImageDiff {
    let blob = |id: Option<Oid>| -> Option<Side> {
        let id = id.filter(|id| !id.is_zero())?;
        let blob = repo.find_blob(id).ok()?;
        Some(decode(blob.content()))
    };
    let new_side = blob(new).or_else(|| {
        let path = workdir_file?;
        let size = std::fs::metadata(path).ok()?.len();
        if size > MAX_BYTES {
            return Some(Side { bytes: size, picture: Err("Too large to preview".to_owned()) });
        }
        Some(decode(&std::fs::read(path).ok()?))
    });
    ImageDiff { old: blob(old), new: new_side }
}

impl ImageDiff {
    /// Every version there is was drawn: an SVG that does not parse is
    /// shown as text alone.
    pub fn all_drawn(&self) -> bool {
        let sides = [&self.old, &self.new];
        sides.iter().any(|side| side.is_some()) && sides.iter().flat_map(|side| side.as_ref()).all(|side| side.picture.is_ok())
    }
}

/// Different for every decode, so the UI can tell when to upload anew.
fn next_id() -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// SVG files larger than this are not drawn.
const MAX_SVG_BYTES: u64 = 8 * 1024 * 1024;
/// A drawn SVG has at least this many pixels on its longest side, so that
/// it stays sharp when the preview enlarges a small icon, and at most
/// [`MAX_SIDE`].
const SVG_SIDE: f32 = 1024.0;

/// The fonts text in an SVG is drawn with: the app's own (a sans-serif and
/// a monospace face, which it carries anyway), standing in for every family
/// an SVG may ask for. No system fonts: looking through them takes a while
/// and would make the preview depend on the machine.
static SVG_FONTS: LazyLock<Arc<fontdb::Database>> = LazyLock::new(|| {
    /// One of egui's fonts as a font source, without copying it.
    struct Font(Arc<eframe::egui::FontData>);
    impl AsRef<[u8]> for Font {
        fn as_ref(&self) -> &[u8] {
            &self.0.font
        }
    }
    let mut db = fontdb::Database::new();
    let fonts = eframe::egui::FontDefinitions::default().font_data;
    let mut family = |name: &str| -> Option<String> {
        let id = *db.load_font_source(fontdb::Source::Binary(Arc::new(Font(fonts.get(name)?.clone())))).first()?;
        Some(db.face(id)?.families.first()?.0.clone())
    };
    let (sans, mono) = (family("Ubuntu-Light"), family("Hack"));
    if let Some(sans) = sans {
        db.set_sans_serif_family(sans.clone());
        // Also what a family nobody has falls back to.
        db.set_serif_family(sans.clone());
        db.set_cursive_family(sans.clone());
        db.set_fantasy_family(sans);
    }
    if let Some(mono) = mono {
        db.set_monospace_family(mono);
    }
    Arc::new(db)
});

/// Draws one SVG file's bytes.
pub fn decode_svg(bytes: &[u8]) -> Side {
    let size = bytes.len() as u64;
    let picture = (|| {
        if size > MAX_SVG_BYTES {
            return Err("Too large to preview".to_owned());
        }
        let options = usvg::Options { fontdb: SVG_FONTS.clone(), ..usvg::Options::default() };
        let tree = usvg::Tree::from_data(bytes, &options).map_err(|_| "Not a valid SVG".to_owned())?;
        let own = tree.size();
        let (width, height) = (own.width().ceil().max(1.0), own.height().ceil().max(1.0));
        if width > MAX_DIMENSION as f32 || height > MAX_DIMENSION as f32 {
            return Err("Too large to preview".to_owned());
        }
        let longest = width.max(height);
        let scale = if longest < SVG_SIDE { SVG_SIDE / longest } else { (MAX_SIDE as f32 / longest).min(1.0) };
        let pixels = |side: f32| ((side * scale).round() as u32).clamp(1, MAX_SIDE);
        let (w, h) = (pixels(width), pixels(height));
        let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or_else(|| "Too large to preview".to_owned())?;
        let transform = tiny_skia::Transform::from_scale(w as f32 / own.width(), h as f32 / own.height());
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        let rgba = pixmap
            .pixels()
            .iter()
            .flat_map(|pixel| {
                let color = pixel.demultiply();
                [color.red(), color.green(), color.blue(), color.alpha()]
            })
            .collect();
        Ok(Picture {
            id: next_id(),
            format: "SVG",
            width: width as u32,
            height: height as u32,
            rgba,
            rgba_size: [w as usize, h as usize],
        })
    })();
    Side { bytes: size, picture }
}

/// Decodes one file's bytes, scaled down for display when it is large.
pub fn decode(bytes: &[u8]) -> Side {
    let size = bytes.len() as u64;
    let picture = (|| {
        if size > MAX_BYTES {
            return Err("Too large to preview".to_owned());
        }
        let mut reader = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|_| "Unreadable image".to_owned())?;
        let format = match reader.format() {
            Some(image::ImageFormat::Png) => "PNG",
            Some(image::ImageFormat::Jpeg) => "JPEG",
            Some(image::ImageFormat::Gif) => "GIF",
            Some(image::ImageFormat::Bmp) => "BMP",
            Some(image::ImageFormat::WebP) => "WebP",
            Some(image::ImageFormat::Ico) => "ICO",
            _ => return Err("Not an image format the preview knows".to_owned()),
        };
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_DIMENSION);
        limits.max_image_height = Some(MAX_DIMENSION);
        reader.limits(limits);
        let image = reader.decode().map_err(|error| match error {
            image::ImageError::Limits(_) => "Too large to preview".to_owned(),
            _ => format!("This {format} file could not be read"),
        })?;
        let (width, height) = (image.width(), image.height());
        let shown = if width > MAX_SIDE || height > MAX_SIDE { image.thumbnail(MAX_SIDE, MAX_SIDE) } else { image };
        let rgba = shown.to_rgba8();
        let rgba_size = [rgba.width() as usize, rgba.height() as usize];
        Ok(Picture {
            id: next_id(),
            format,
            width,
            height,
            rgba: rgba.into_raw(),
            rgba_size,
        })
    })();
    Side { bytes: size, picture }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A `w` by `h` PNG in memory, every pixel `rgba`.
    pub fn png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(w, h, image::Rgba(rgba));
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn image_paths_are_recognised_by_extension() {
        assert!(is_image_path("art/logo.PNG"));
        assert!(is_image_path("a/b/photo.jpeg"));
        assert!(is_image_path("icon.ico"));
        assert!(!is_image_path("logo.svg"));
        assert!(!is_image_path("png"));
        assert!(!is_image_path("archive.png.zip"));
    }

    #[test]
    fn a_small_png_is_decoded_as_it_is() {
        let side = decode(&png(3, 2, [255, 0, 0, 128]));
        let picture = side.picture.unwrap();
        assert_eq!((picture.format, picture.width, picture.height), ("PNG", 3, 2));
        assert_eq!(picture.rgba_size, [3, 2]);
        assert_eq!(&picture.rgba[..4], &[255, 0, 0, 128]);
        assert_eq!(picture.rgba.len(), 3 * 2 * 4);
    }

    #[test]
    fn a_large_png_is_scaled_down_but_keeps_its_size() {
        let picture = decode(&png(4000, 1000, [0, 0, 0, 255])).picture.unwrap();
        assert_eq!((picture.width, picture.height), (4000, 1000));
        assert_eq!(picture.rgba_size, [2048, 512]);
    }

    #[test]
    fn garbage_and_unknown_formats_say_why() {
        let side = decode(b"definitely not an image");
        assert_eq!(side.bytes, 23);
        assert!(side.picture.is_err());

        // A PNG header claiming to be enormous is refused before decoding.
        let mut huge = png(1, 1, [0; 4]);
        huge[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        let crc = crc32(&huge[12..29]);
        huge[29..33].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(decode(&huge).picture.err().as_deref(), Some("Too large to preview"));
    }

    /// The CRC PNG chunks carry, so an edited header still reads as valid.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    const SQUARE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="8" viewBox="0 0 16 8">
<rect width="8" height="8" fill="#ff0000"/><text x="9" y="7" font-family="Arial" font-size="6">A</text></svg>"##;

    #[test]
    fn an_svg_is_drawn_sharp_enough_to_enlarge() {
        assert!(is_svg_path("art/Logo.SVG") && !is_svg_path("logo.svgz") && !is_image_path("logo.svg"));
        let picture = decode_svg(SQUARE.as_bytes()).picture.unwrap();
        assert_eq!((picture.format, picture.width, picture.height), ("SVG", 16, 8));
        // Drawn at 1024 pixels on its longest side.
        assert_eq!(picture.rgba_size, [1024, 512]);
        let pixel = |x: usize, y: usize| &picture.rgba[(y * 1024 + x) * 4..][..4];
        assert_eq!(pixel(10, 10), [255, 0, 0, 255]);
        // Right of the square: transparent but for the letter.
        assert_eq!(pixel(1020, 10)[3], 0);
        // The text is drawn, in the app's own font for want of Arial.
        let inked = (576..1024).flat_map(|x| (0..512).map(move |y| (x, y))).filter(|&(x, y)| pixel(x, y)[3] > 0).count();
        assert!(inked > 1000, "{inked} pixels of text");
    }

    #[test]
    fn broken_or_huge_svgs_are_not_drawn() {
        assert_eq!(decode_svg(b"<svg").picture.err().as_deref(), Some("Not a valid SVG"));
        assert!(decode_svg(b"not xml at all").picture.is_err());
        let huge = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="10"/>"#;
        assert_eq!(decode_svg(huge.as_bytes()).picture.err().as_deref(), Some("Too large to preview"));
        // A big one is drawn at most 2048 pixels wide.
        let big = r#"<svg xmlns="http://www.w3.org/2000/svg" width="4000" height="1000"><rect width="10" height="10"/></svg>"#;
        assert_eq!(decode_svg(big.as_bytes()).picture.unwrap().rgba_size, [2048, 512]);
        let drawn = ImageDiff { old: Some(decode_svg(SQUARE.as_bytes())), new: None };
        assert!(drawn.all_drawn());
        let half = ImageDiff { old: Some(decode_svg(SQUARE.as_bytes())), new: Some(decode_svg(b"<svg")) };
        assert!(!half.all_drawn() && !ImageDiff::default().all_drawn());
    }

    #[test]
    fn every_decode_gets_its_own_id() {
        let bytes = png(1, 1, [0; 4]);
        let (a, b) = (decode(&bytes).picture.unwrap(), decode(&bytes).picture.unwrap());
        assert_ne!(a.id, b.id);
    }
}
