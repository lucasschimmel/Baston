//! The server icon published in `/info.json` as `icon`.
//!
//! FXServer's `load_server_icon` reads a PNG, refuses it unless it is 96×96,
//! and base64-encodes it into the info document (`InfoHttpHandler.cpp:228`).
//! The constraint belongs to the FiveM server browser, not to BASTON, so it is
//! reproduced rather than relaxed: an icon of the wrong size is not shown
//! smaller, it is not shown at all.
//!
//! Refusing at load, with the dimensions in the message, is the whole point.
//! An operator whose logo silently never appears has no way to find out why.

use std::path::Path;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

/// The size the FiveM server browser requires.
const REQUIRED: (u32, u32) = (96, 96);

/// A PNG file begins with this, always.
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// `IHDR` is required by the PNG spec to be the first chunk, so width and
/// height sit at fixed offsets: 8 magic + 4 length + 4 type = 16.
const IHDR_WIDTH_OFFSET: usize = 16;

/// Enough to be certain the file is not a plausible 96×96 PNG. Generous —
/// the point is to refuse a video someone renamed, not to police compression.
const MAX_BYTES: u64 = 512 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum IconError {
    #[error(
        "could not read the server icon at {path}: {reason}\n  \
         → check [server] icon points at a file this process can read"
    )]
    Unreadable { path: String, reason: String },

    #[error(
        "the server icon at {path} is {size} bytes, over the {max} byte limit\n  \
         → a 96x96 PNG is a few kilobytes; this is probably the wrong file"
    )]
    TooLarge { path: String, size: u64, max: u64 },

    #[error(
        "the server icon at {0} is not a PNG\n  \
         → the FiveM server browser only accepts PNG; convert it"
    )]
    NotPng(String),

    #[error(
        "the server icon at {path} is {width}x{height}, and must be 96x96\n  \
         → the FiveM server browser will not display any other size; resize it"
    )]
    WrongSize {
        path: String,
        width: u32,
        height: u32,
    },
}

/// Read an icon and return it base64-encoded, ready for `info.json`.
///
/// # Errors
///
/// Every failure names the file and what to do about it.
pub fn load(path: &Path) -> Result<String, IconError> {
    let display = path.display().to_string();

    let size = std::fs::metadata(path)
        .map_err(|e| IconError::Unreadable {
            path: display.clone(),
            reason: e.to_string(),
        })?
        .len();
    if size > MAX_BYTES {
        return Err(IconError::TooLarge {
            path: display,
            size,
            max: MAX_BYTES,
        });
    }

    let bytes = std::fs::read(path).map_err(|e| IconError::Unreadable {
        path: display.clone(),
        reason: e.to_string(),
    })?;

    let (width, height) =
        png_dimensions(&bytes).ok_or_else(|| IconError::NotPng(display.clone()))?;
    if (width, height) != REQUIRED {
        return Err(IconError::WrongSize {
            path: display,
            width,
            height,
        });
    }

    Ok(B64.encode(&bytes))
}

/// Width and height from a PNG's `IHDR`, or `None` if this is not a PNG.
///
/// Deliberately not a decode: the file is republished byte-for-byte, so the
/// only questions are "is it a PNG" and "what size does it claim to be".
/// Pulling in an image decoder to answer them would add an attack surface that
/// parses operator-supplied bytes for no gain.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(PNG_MAGIC) {
        return None;
    }
    // The IHDR chunk type must be present where the spec puts it; a file with
    // the magic but another first chunk is malformed, not a smaller icon.
    if bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(
        bytes
            .get(IHDR_WIDTH_OFFSET..IHDR_WIDTH_OFFSET + 4)?
            .try_into()
            .ok()?,
    );
    let height = u32::from_be_bytes(
        bytes
            .get(IHDR_WIDTH_OFFSET + 4..IHDR_WIDTH_OFFSET + 8)?
            .try_into()
            .ok()?,
    );
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG header claiming `width`×`height`. Enough for the checks above,
    /// which never decode pixels.
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = PNG_MAGIC.to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]); // depth, colour, etc.
        bytes
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_ninety_six_square_png_is_accepted_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = png_header(96, 96);
        let path = write(dir.path(), "logo.png", &bytes);

        let encoded = load(&path).expect("96x96 is the accepted size");
        assert_eq!(B64.decode(encoded).unwrap(), bytes, "republished verbatim");
    }

    #[test]
    fn another_size_is_refused_with_its_dimensions_in_the_message() {
        // The failure an operator actually hits. The message has to say what
        // the file is, or they have no way to find out why nothing appears.
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "logo.png", &png_header(512, 512));

        let err = load(&path).expect_err("512x512 must be refused");
        assert!(matches!(
            err,
            IconError::WrongSize {
                width: 512,
                height: 512,
                ..
            }
        ));
        assert!(err.to_string().contains("512x512"));
    }

    #[test]
    fn a_file_that_is_not_a_png_is_refused_whatever_it_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "logo.png", b"GIF89a and then some bytes");
        assert!(matches!(load(&path), Err(IconError::NotPng(_))));
    }

    #[test]
    fn a_truncated_png_header_is_refused_rather_than_panicking() {
        // Every offset read is bounds-checked; this is the test that says so.
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "logo.png", &png_header(96, 96)[..20]);
        assert!(matches!(load(&path), Err(IconError::NotPng(_))));
    }

    #[test]
    fn the_magic_alone_is_not_a_png() {
        assert_eq!(png_dimensions(PNG_MAGIC), None);
    }

    #[test]
    fn a_missing_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(&dir.path().join("nope.png")).expect_err("missing must fail");
        assert!(matches!(err, IconError::Unreadable { .. }));
        assert!(err.to_string().contains("nope.png"));
    }
}
