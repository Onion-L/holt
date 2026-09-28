//! Custom provider logos: one normalized file per user-defined provider under
//! `<data_dir>/provider-logos/`. Raster uploads are decoded and re-encoded as
//! a PNG no larger than [`MAX_EDGE`]; SVG uploads are kept as text (the UI
//! renders them as a tinted mask, like the built-in brand marks).

use std::io::Cursor;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use holt_proto::{ProviderLogo, ProviderLogoFormat};

use crate::EngineError;

const DIR_NAME: &str = "provider-logos";
/// Longest edge of a stored raster logo — marks render at ≤ 26 logical px,
/// so this leaves headroom for 2x/3x displays without keeping the upload.
const MAX_EDGE: u32 = 128;
/// Upload ceiling for the raw bytes, before normalization.
pub(crate) const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;
/// SVGs are stored verbatim, so they get a tighter cap of their own.
const MAX_SVG_BYTES: usize = 256 * 1024;
/// Decode allocation ceiling: a hostile header cannot claim gigabytes.
const DECODE_MAX_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct ProviderLogoStore {
    dir: PathBuf,
}

impl ProviderLogoStore {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join(DIR_NAME),
        }
    }

    pub(crate) fn load(&self, provider_id: &str) -> Option<ProviderLogo> {
        let format = [ProviderLogoFormat::Svg, ProviderLogoFormat::Png]
            .into_iter()
            .find(|format| self.file(provider_id, *format).is_some_and(|p| p.is_file()))?;
        let bytes = std::fs::read(self.file(provider_id, format)?).ok()?;
        Some(ProviderLogo {
            format,
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    /// Normalize and store `bytes` as the provider's logo, replacing any
    /// previous one (of either format).
    pub(crate) fn save(&self, provider_id: &str, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > MAX_UPLOAD_BYTES {
            return Err("logo exceeds the 8 MiB upload limit".into());
        }
        let (format, stored) = normalize(bytes)?;
        let path = self
            .file(provider_id, format)
            .ok_or_else(|| "provider id cannot name a logo file".to_string())?;
        std::fs::create_dir_all(&self.dir).map_err(|error| error.to_string())?;
        let temp = self.dir.join(format!(".logo.{}.tmp", uuid::Uuid::new_v4()));
        let written = std::fs::write(&temp, &stored).and_then(|()| std::fs::rename(&temp, &path));
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temp);
            return Err(error.to_string());
        }
        for other in [ProviderLogoFormat::Svg, ProviderLogoFormat::Png] {
            if other != format
                && let Some(stale) = self.file(provider_id, other)
            {
                let _ = std::fs::remove_file(stale);
            }
        }
        Ok(())
    }

    /// Drop the provider's logo. Missing files are not an error.
    pub(crate) fn remove(&self, provider_id: &str) -> Result<(), EngineError> {
        for format in [ProviderLogoFormat::Svg, ProviderLogoFormat::Png] {
            let Some(path) = self.file(provider_id, format) else {
                continue;
            };
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(crate) fn remove_all(&self) -> Result<(), EngineError> {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// The logo path for one provider, or `None` when the id could escape
    /// the directory (custom ids already exclude `/`; this guards the rest).
    fn file(&self, provider_id: &str, format: ProviderLogoFormat) -> Option<PathBuf> {
        let safe = !provider_id.is_empty()
            && provider_id != "."
            && provider_id != ".."
            && !provider_id
                .chars()
                .any(|character| matches!(character, '/' | '\\' | ':') || character.is_control());
        let extension = match format {
            ProviderLogoFormat::Png => "png",
            ProviderLogoFormat::Svg => "svg",
        };
        safe.then(|| self.dir.join(format!("{provider_id}.{extension}")))
    }
}

/// Sniff the upload: SVG text stays as-is (size-capped); anything else must
/// decode as a raster image and is re-encoded as a bounded PNG.
fn normalize(bytes: &[u8]) -> Result<(ProviderLogoFormat, Vec<u8>), String> {
    if looks_like_svg(bytes) {
        if bytes.len() > MAX_SVG_BYTES {
            return Err("SVG logo exceeds the 256 KiB limit".into());
        }
        return Ok((ProviderLogoFormat::Svg, bytes.to_vec()));
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("logo format could not be determined: {error}"))?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(DECODE_MAX_ALLOC_BYTES);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| format!("logo is not a supported image: {error}"))?;
    let image = if image.width() > MAX_EDGE || image.height() > MAX_EDGE {
        image.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Lanczos3)
    } else {
        image
    };
    let mut encoded = Vec::new();
    image
        .to_rgba8()
        .write_to(&mut Cursor::new(&mut encoded), image::ImageFormat::Png)
        .map_err(|error| format!("logo could not be encoded: {error}"))?;
    Ok((ProviderLogoFormat::Png, encoded))
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(&bytes[..bytes.len().min(4096)]) else {
        // A multi-byte char split at the window edge is still text.
        return std::str::from_utf8(bytes).is_ok_and(|text| text.contains("<svg"));
    };
    text.trim_start_matches('\u{feff}')
        .trim_start()
        .starts_with('<')
        && text.contains("<svg")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::from_pixel(width, height, image::Rgba([200, 30, 30, 255]))
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn decoded_size(logo: &ProviderLogo) -> (u32, u32) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&logo.data)
            .unwrap();
        let image = image::load_from_memory(&bytes).unwrap();
        (image.width(), image.height())
    }

    #[test]
    fn raster_uploads_are_downscaled_to_a_png() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProviderLogoStore::new(dir.path());
        store.save("acme", &png(512, 256)).unwrap();
        let logo = store.load("acme").unwrap();
        assert_eq!(logo.format, ProviderLogoFormat::Png);
        assert_eq!(decoded_size(&logo), (128, 64));

        // Small images keep their size.
        store.save("acme", &png(32, 32)).unwrap();
        assert_eq!(decoded_size(&store.load("acme").unwrap()), (32, 32));
    }

    #[test]
    fn an_svg_replaces_a_png_and_removal_clears_both() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProviderLogoStore::new(dir.path());
        store.save("acme", &png(16, 16)).unwrap();
        let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"/>"#;
        store.save("acme", svg).unwrap();
        let logo = store.load("acme").unwrap();
        assert_eq!(logo.format, ProviderLogoFormat::Svg);
        assert!(!dir.path().join(DIR_NAME).join("acme.png").exists());

        store.remove("acme").unwrap();
        assert!(store.load("acme").is_none());
        // Removing again is not an error.
        store.remove("acme").unwrap();
    }

    #[test]
    fn junk_and_unsafe_ids_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProviderLogoStore::new(dir.path());
        assert!(store.save("acme", b"not an image").is_err());
        assert!(store.save("..", &png(8, 8)).is_err());
        assert!(store.save("a\\b", &png(8, 8)).is_err());
        assert!(store.load("acme").is_none());
    }
}
