//! Canonical content fingerprints.
//!
//! The fingerprint must be identical on both machines for "the same" content,
//! even though each OS re-encodes it (macOS may hand back TIFF for an image we
//! wrote as PNG, received files land in a different directory, …). So we hash
//! a canonical form rather than raw bytes.

use crate::ClipboardContent;
use linker_proto::ContentHash;
use sha2::{Digest, Sha256};

pub fn content_hash(c: &ClipboardContent) -> ContentHash {
    let mut h = Sha256::new();
    match c {
        ClipboardContent::Text { text, .. } => {
            h.update(b"text\0");
            // Normalise line endings so CRLF/LF differences don't defeat the guard.
            h.update(text.replace("\r\n", "\n").as_bytes());
        }
        ClipboardContent::Image { png } => {
            h.update(b"image\0");
            h.update(image_pixels_hash(png));
        }
        ClipboardContent::Files { paths } => {
            h.update(b"files\0");
            for p in paths {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                h.update(name.as_bytes());
                h.update([0]);
                h.update(size.to_be_bytes());
            }
        }
    }
    h.finalize().into()
}

/// Hash of decoded RGBA pixels + dimensions (falls back to raw bytes if the
/// image can't be decoded).
pub fn image_pixels_hash(encoded: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    match image::load_from_memory(encoded) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            h.update(rgba.width().to_be_bytes());
            h.update(rgba.height().to_be_bytes());
            h.update(rgba.as_raw());
        }
        Err(_) => h.update(encoded),
    }
    h.finalize().into()
}

/// Encode RGBA pixels as PNG.
pub fn rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<u8>> {
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| anyhow::anyhow!("bad RGBA buffer size"))?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

/// Re-encode any supported image (e.g. TIFF) as PNG.
pub fn to_png(encoded: &[u8]) -> anyhow::Result<Vec<u8>> {
    let img = image::load_from_memory(encoded)?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_hash_ignores_line_endings_and_html() {
        let a = ClipboardContent::Text {
            text: "a\r\nb".into(),
            html: None,
        };
        let b = ClipboardContent::Text {
            text: "a\nb".into(),
            html: Some("<p>a</p>".into()),
        };
        assert_eq!(content_hash(&a), content_hash(&b));
    }

    #[test]
    fn image_hash_survives_reencoding() {
        let rgba: Vec<u8> = (0..4 * 4 * 4).map(|i| i as u8).collect();
        let png = rgba_to_png(4, 4, &rgba).unwrap();
        // Re-encode via TIFF and back, as macOS might.
        let img = image::load_from_memory(&png).unwrap();
        let mut tiff = std::io::Cursor::new(Vec::new());
        img.write_to(&mut tiff, image::ImageFormat::Tiff).unwrap();
        let tiff = tiff.into_inner();
        assert_ne!(png, tiff);
        assert_eq!(
            content_hash(&ClipboardContent::Image { png: png.clone() }),
            content_hash(&ClipboardContent::Image { png: tiff })
        );
    }

    #[test]
    fn different_kinds_differ() {
        let t = ClipboardContent::Text {
            text: String::new(),
            html: None,
        };
        let f = ClipboardContent::Files { paths: vec![] };
        assert_ne!(content_hash(&t), content_hash(&f));
    }

    #[test]
    fn files_hash_uses_name_and_size_not_directory() {
        let d1 = std::env::temp_dir().join(format!("linker-h1-{}", std::process::id()));
        let d2 = std::env::temp_dir().join(format!("linker-h2-{}", std::process::id()));
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();
        std::fs::write(d1.join("x.txt"), b"hello").unwrap();
        std::fs::write(d2.join("x.txt"), b"hello").unwrap();
        let a = ClipboardContent::Files {
            paths: vec![d1.join("x.txt")],
        };
        let b = ClipboardContent::Files {
            paths: vec![d2.join("x.txt")],
        };
        assert_eq!(content_hash(&a), content_hash(&b));
        let _ = std::fs::remove_dir_all(&d1);
        let _ = std::fs::remove_dir_all(&d2);
    }
}
