//! Port of `pi-core/agent/src/harness/tools/image.ts`.

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

fn starts_with(buffer: &[u8], prefix: &[u8]) -> bool {
    buffer.len() >= prefix.len() && buffer[..prefix.len()] == *prefix
}

fn starts_with_ascii(buffer: &[u8], offset: usize, text: &str) -> bool {
    let bytes = text.as_bytes();
    if buffer.len() < offset + bytes.len() {
        return false;
    }
    &buffer[offset..offset + bytes.len()] == bytes
}

fn read_u32_be(buffer: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(
        buffer
            .get(offset..offset + 4)
            .unwrap_or(&[0; 4])
            .try_into()
            .unwrap(),
    )
}

fn read_u32_le(buffer: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        buffer
            .get(offset..offset + 4)
            .unwrap_or(&[0; 4])
            .try_into()
            .unwrap(),
    )
}

fn read_u16_le(buffer: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        buffer
            .get(offset..offset + 2)
            .unwrap_or(&[0; 2])
            .try_into()
            .unwrap(),
    )
}

fn is_png(buffer: &[u8]) -> bool {
    buffer.len() >= 16
        && read_u32_be(buffer, PNG_SIGNATURE.len()) == 13
        && starts_with_ascii(buffer, 12, "IHDR")
}

fn is_animated_png(buffer: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= buffer.len() {
        let chunk_length = read_u32_be(buffer, offset);
        let chunk_type_offset = offset + 4;
        if starts_with_ascii(buffer, chunk_type_offset, "acTL") {
            return true;
        }
        if starts_with_ascii(buffer, chunk_type_offset, "IDAT") {
            return false;
        }
        let next_offset = offset as u64 + 8 + chunk_length as u64 + 4;
        if next_offset <= offset as u64 || next_offset > buffer.len() as u64 {
            return false;
        }
        offset = next_offset as usize;
    }
    false
}

fn is_bmp(buffer: &[u8]) -> bool {
    if buffer.len() < 26 {
        return false;
    }
    let declared_file_size = read_u32_le(buffer, 2);
    let pixel_data_offset = read_u32_le(buffer, 10);
    let dib_header_size = read_u32_le(buffer, 14);
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if (pixel_data_offset as u64) < 14 + dib_header_size as u64 {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }

    let (color_planes, bits_per_pixel) = if dib_header_size == 12 {
        (read_u16_le(buffer, 22), read_u16_le(buffer, 24))
    } else if (40..=124).contains(&dib_header_size) {
        if buffer.len() < 30 {
            return false;
        }
        (read_u16_le(buffer, 26), read_u16_le(buffer, 28))
    } else {
        return false;
    };
    color_planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits_per_pixel)
}

/// Port of `detectSupportedImageMimeType`.
pub fn detect_supported_image_mime_type(buffer: &[u8]) -> Option<&'static str> {
    if starts_with(buffer, &[0xff, 0xd8, 0xff]) {
        return if buffer.get(3) == Some(&0xf7) {
            None
        } else {
            Some("image/jpeg")
        };
    }
    if starts_with(buffer, &PNG_SIGNATURE) && is_png(buffer) && !is_animated_png(buffer) {
        return Some("image/png");
    }
    if starts_with_ascii(buffer, 0, "GIF") {
        return Some("image/gif");
    }
    if starts_with_ascii(buffer, 0, "RIFF") && starts_with_ascii(buffer, 8, "WEBP") {
        return Some("image/webp");
    }
    if starts_with_ascii(buffer, 0, "BM") && is_bmp(buffer) {
        return Some("image/bmp");
    }
    None
}

/// Port of `encodeBase64` (standard alphabet with padding).
pub fn encode_base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
