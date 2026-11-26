// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The owner's images of a turn as the model sees them: each local file read and attached inline
//! as an image block after the turn's text, as the reference attaches user images natively for a
//! model that takes them (ADR-0084). The journal keeps the text alone; only the copy of the input
//! each request of the turn sends carries the blocks.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::agent::block;
use crate::wire::c4g::{Block, BlockType, MediaType};

/// Largest image file attached: its base64 stays within the 5 MiB a vendor takes per image.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024 / 4 * 3;

/// Rough tokens of one attached image in a pre-flight count, the reference's flat cost per
/// image instead of the length of its base64.
pub const IMAGE_TOKENS: usize = 1500;

/// The blocks of `paths` in order: an image block per file that can be attached, else a text
/// block that says the image could not be attached and why.
#[must_use]
pub fn blocks(paths: &[String]) -> Vec<Block> {
    paths
        .iter()
        .map(|path| match image(Path::new(path)) {
            Ok(block) => block,
            Err(reason) => text(&format!(
                "[The user sent an image saved at {path}, but it could not be attached ({reason}), \
                 so you cannot view it; do not describe its content.]"
            )),
        })
        .collect()
}

/// The image block of the file at `path`.
fn image(path: &Path) -> Result<Block, String> {
    if !path.is_absolute() {
        return Err("not an absolute path".to_owned());
    }
    let file = File::open(path).map_err(|e| format!("cannot open it: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot read it: {e}"))?;
    if !meta.is_file() {
        return Err("not a regular file".to_owned());
    }
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(format!("{} bytes, more than {MAX_IMAGE_BYTES}", meta.len()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read it: {e}"))?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(format!("more than {MAX_IMAGE_BYTES} bytes"));
    }
    let media_type = media_type(&bytes).ok_or("not a PNG, JPEG, GIF or WebP image")?;
    Ok(Block {
        media_type: Some(media_type),
        base64: Some(base64(&bytes)),
        ..block(BlockType::Image)
    })
}

/// The media type of `bytes` by their signature: what the image is, whatever its name says.
#[must_use]
pub fn media_type(bytes: &[u8]) -> Option<MediaType> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(MediaType::ImagePng)
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some(MediaType::ImageJpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(MediaType::ImageGif)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(MediaType::ImageWebp)
    } else {
        None
    }
}

/// Standard base64 with padding.
#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn text(text: &str) -> Block {
    Block {
        text: Some(text.to_owned()),
        ..block(BlockType::Text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn the_signature_names_the_media_type() {
        assert_eq!(
            media_type(b"\x89PNG\r\n\x1a\nrest"),
            Some(MediaType::ImagePng)
        );
        assert_eq!(media_type(b"\xff\xd8\xff\xe0"), Some(MediaType::ImageJpeg));
        assert_eq!(media_type(b"GIF89a.."), Some(MediaType::ImageGif));
        assert_eq!(
            media_type(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(MediaType::ImageWebp)
        );
        assert_eq!(media_type(b"BM...."), None);
        assert_eq!(media_type(b"RIFF\0\0\0\0WAVE"), None);
    }

    #[test]
    fn a_rough_count_takes_an_image_at_a_flat_cost() {
        use crate::agent::text_message;
        use crate::compress::messages_tokens_rough;
        use crate::wire::c4g::Role;
        let text = text_message(Role::User, "Look.");
        let mut with_image = text.clone();
        with_image.content.push(Block {
            media_type: Some(MediaType::ImagePng),
            base64: Some("A".repeat(400_000)),
            ..block(BlockType::Image)
        });
        assert_eq!(
            messages_tokens_rough(&[with_image]),
            messages_tokens_rough(&[text]) + IMAGE_TOKENS
        );
    }
}
