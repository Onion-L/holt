//! Bounded first-frame decoding shared by preview validation and the read hook.
use std::io::{Cursor, Read};
use std::path::Path;

use image::{DynamicImage, ImageDecoder, ImageFormat};

pub const MAX_PIXELS: u64 = 32 * 1024 * 1024;
pub const MODEL_MAX_EDGE: u32 = 2048;
pub const MODEL_MAX_BYTES: usize = 5 * 1024 * 1024;

pub fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    read_file(path, true)
}

pub fn read_for_tool(path: &Path) -> Result<Vec<u8>, String> {
    read_file(path, false)
}

fn read_file(path: &Path, image_only: bool) -> Result<Vec<u8>, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("File could not be read: {e}"))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("Image target must be a regular file.".into());
    }
    let mut prefix = [0u8; 32];
    let prefix_len = file.read(&mut prefix).map_err(|e| e.to_string())?;
    let image_path = path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
        )
    });
    let bounded = image_only || image_path || image::guess_format(&prefix[..prefix_len]).is_ok();
    if bounded && metadata.len() > super::MAX_IMAGE_BYTES {
        return Err("File exceeds the 25 MiB read limit.".into());
    }
    let mut bytes = prefix[..prefix_len].to_vec();
    file.take(if bounded {
        super::MAX_IMAGE_BYTES + 1 - prefix_len as u64
    } else {
        u64::MAX
    })
    .read_to_end(&mut bytes)
    .map_err(|e| e.to_string())?;
    if bounded && bytes.len() as u64 > super::MAX_IMAGE_BYTES {
        return Err("File exceeds the 25 MiB read limit.".into());
    }
    Ok(bytes)
}

pub fn decode(bytes: &[u8]) -> Result<(DynamicImage, bool), String> {
    if bytes.len() as u64 > super::MAX_IMAGE_BYTES {
        return Err("Image exceeds the 25 MiB limit.".into());
    }
    let format = image::guess_format(bytes).map_err(|e| format!("Unsupported image: {e}"))?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
    ) {
        return Err("Unsupported image format. Use PNG, JPEG, GIF, or WebP.".into());
    }
    if format == ImageFormat::Png {
        let decoder =
            image::codecs::png::PngDecoder::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
        if decoder.is_apng().map_err(|e| e.to_string())? {
            return Err("APNG preview and visual reading are unsupported.".into());
        }
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| format!("Image could not be opened: {e}"))?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err("Image exceeds the 32 megapixel decode limit.".into());
    }
    let orientation = decoder
        .orientation()
        .map_err(|e| format!("Image orientation: {e}"))?;
    // DynamicImage decodes a single frame, including for animated WebP/GIF.
    let mut image =
        DynamicImage::from_decoder(decoder).map_err(|e| format!("Corrupt image: {e}"))?;
    image.apply_orientation(orientation);
    Ok((
        image,
        matches!(format, ImageFormat::Gif | ImageFormat::WebP),
    ))
}

pub fn model_image(bytes: &[u8]) -> Result<(Vec<u8>, Vec<String>), String> {
    let (mut image, first_frame) = decode(bytes)?;
    let original = (image.width(), image.height());
    if original.0.max(original.1) > MODEL_MAX_EDGE {
        image = image.resize(
            MODEL_MAX_EDGE,
            MODEL_MAX_EDGE,
            image::imageops::FilterType::Triangle,
        );
    }
    // PNG preserves screenshot text and alpha. Bound its transport size too;
    // unusually noisy inputs may need an additional proportional reduction.
    let bytes = loop {
        let mut output = Cursor::new(Vec::new());
        image
            .write_to(&mut output, ImageFormat::Png)
            .map_err(|e| format!("Image encoding failed: {e}"))?;
        if output.get_ref().len() <= MODEL_MAX_BYTES {
            break output.into_inner();
        }
        let edge = image.width().max(image.height()) * 3 / 4;
        if edge == 0 {
            return Err("Image cannot fit the 5 MiB model input limit.".into());
        }
        image = image.resize(edge, edge, image::imageops::FilterType::Triangle);
    };
    let mut hints = vec![format!(
        "Original: {}x{}; model input: {}x{} pixels (orientation applied).",
        original.0,
        original.1,
        image.width(),
        image.height()
    )];
    if first_frame {
        hints.push("Only the first frame is supplied; animation is not played.".into());
    }
    if original != (image.width(), image.height()) {
        hints.push("Proportionally resized; small text and fine detail may be lost. Source file unchanged.".into());
    }
    Ok((bytes, hints))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, ImageEncoder, Rgba, RgbaImage};

    #[test]
    fn gif_and_animated_webp_supply_only_the_first_frame() {
        let mut gif = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            for color in [[255, 0, 0, 255], [0, 255, 0, 255]] {
                encoder
                    .encode_frame(image::Frame::new(RgbaImage::from_pixel(2, 2, Rgba(color))))
                    .unwrap();
            }
        }
        let webp = include_bytes!("../../tests/fixtures/first-frame.webp");
        for bytes in [gif.as_slice(), webp.as_slice()] {
            let (preview, first_frame) = decode(bytes).unwrap();
            assert!(first_frame);
            let color = preview.get_pixel(0, 0);
            assert!(color[0] > 240 && color[1] < 10);
            let (model, hints) = model_image(bytes).unwrap();
            assert!(hints.iter().any(|hint| hint.contains("first frame")));
            let model = image::load_from_memory(&model).unwrap();
            assert_eq!(model.dimensions(), preview.dimensions());
            assert_eq!(model.get_pixel(0, 0), color);
        }
    }

    #[test]
    fn jpeg_orientation_is_applied_before_preview_and_model_dimensions() {
        let source = RgbaImage::from_fn(40, 20, |x, _| {
            if x < 20 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 255, 0, 255])
            }
        });
        let rgb = DynamicImage::ImageRgba8(source).to_rgb8();
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 95);
        // Little-endian TIFF IFD with EXIF orientation 6 (90 degrees clockwise).
        encoder
            .set_exif_metadata(vec![
                b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 1, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0,
                0, 0,
            ])
            .unwrap();
        encoder
            .write_image(rgb.as_raw(), 40, 20, image::ExtendedColorType::Rgb8)
            .unwrap();
        let (preview, _) = decode(&bytes).unwrap();
        assert_eq!(preview.dimensions(), (20, 40));
        assert!(preview.get_pixel(10, 5)[0] > 240);
        assert!(preview.get_pixel(10, 35)[1] > 240);
        let (model, hints) = model_image(&bytes).unwrap();
        assert_eq!(
            image::load_from_memory(&model).unwrap().dimensions(),
            (20, 40)
        );
        assert!(hints[0].contains("Original: 20x40; model input: 20x40"));
    }

    #[test]
    fn apng_is_rejected_and_pixel_limit_precedes_pixel_decode() {
        let mut apng = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut apng, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_animated(2, 0).unwrap();
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 0, 255]).unwrap();
            writer.write_image_data(&[0, 255, 0, 255]).unwrap();
            writer.finish().unwrap();
        }
        assert!(decode(&apng).unwrap_err().contains("APNG"));

        let mut oversized = Vec::new();
        {
            let encoder = png::Encoder::new(&mut oversized, 8192, 4097);
            let mut writer = encoder.write_header().unwrap();
            writer.write_chunk(png::chunk::IDAT, &[]).unwrap();
        }
        assert!(decode(&oversized).unwrap_err().contains("32 megapixel"));
    }
}
