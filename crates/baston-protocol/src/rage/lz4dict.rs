//! LZ4 block (de)compression matching the OneSync clone stream exactly.
//!
//! The wire is asymmetric, and we reproduce it verbatim (`ServerGameState.cpp`):
//! - **inbound** (`netClones`/`netAcks` uploads) is compressed by the client
//!   *with* the shared dictionary, so the server decompresses with
//!   `LZ4_decompress_safe_usingDict` (`UncompressClonePacket`, SGS.cpp:3815);
//! - **outbound** (`msgPackedClones`/`msgPackedAcks`) is compressed by the
//!   server with plain `LZ4_compress_default` — **no dictionary** — in
//!   `FlushBuffer` (SGS.cpp:748). The client decompresses it dictless to match.

use std::os::raw::{c_char, c_int};

/// The 64 KiB static dictionary shipped with FiveM (`dict_five_20210329.h`),
/// used only for the inbound (client→server) direction.
pub static DICT_FIVE: &[u8] = include_bytes!("../../assets/dict_five_20210329.bin");

/// Decompress an inbound clone/ack payload using the shared dictionary.
///
/// `max_out` bounds the output (the C++ uses a 16 KiB stack buffer). Returns
/// the decompressed bytes, or `None` if LZ4 reports an error (negative length),
/// mirroring the `length <= 0` early-out in `ParseGameStatePacket`.
///
/// liblz4 exposes `LZ4_decompress_safe_usingDict` only as a thin wrapper over
/// the streaming decode API, which is what `lz4-sys` actually binds — so we
/// prime a decode stream with the dictionary and do a single continue call,
/// byte-for-byte equivalent.
pub fn decompress_using_dict(input: &[u8], max_out: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; max_out];
    // SAFETY: the stream handle is created and freed within this call; LZ4 only
    // reads `input`/`DICT_FIVE` and writes at most `max_out` bytes into `out`.
    unsafe {
        let stream = lz4_sys::LZ4_createStreamDecode();
        if stream.is_null() {
            return None;
        }
        let set_ok =
            lz4_sys::LZ4_setStreamDecode(stream, DICT_FIVE.as_ptr(), DICT_FIVE.len() as c_int);
        let len = if set_ok == 0 {
            -1
        } else {
            lz4_sys::LZ4_decompress_safe_continue(
                stream,
                input.as_ptr(),
                out.as_mut_ptr(),
                input.len() as c_int,
                max_out as c_int,
            )
        };
        lz4_sys::LZ4_freeStreamDecode(stream);
        if len <= 0 {
            return None;
        }
        out.truncate(len as usize);
    }
    Some(out)
}

/// Decompress a plain (dictionary-less) LZ4 block — the inverse of
/// [`compress_default`], used to read outbound `msgPackedClones` fragments.
pub fn decompress_plain(input: &[u8], max_out: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; max_out];
    // SAFETY: LZ4 reads `input` and writes at most `max_out` bytes into `out`.
    let len = unsafe {
        lz4_sys::LZ4_decompress_safe(
            input.as_ptr() as *const c_char,
            out.as_mut_ptr() as *mut c_char,
            input.len() as c_int,
            max_out as c_int,
        )
    };
    if len <= 0 {
        return None;
    }
    out.truncate(len as usize);
    Some(out)
}

/// Compress an outbound payload with plain LZ4 (no dictionary), matching
/// `LZ4_compress_default` in `FlushBuffer`.
pub fn compress_default(input: &[u8]) -> Vec<u8> {
    // SAFETY: `bound` is LZ4's own worst-case size, so the write cannot overrun.
    let bound = unsafe { lz4_sys::LZ4_compressBound(input.len() as c_int) };
    let mut out = vec![0u8; bound.max(1) as usize];
    let len = unsafe {
        lz4_sys::LZ4_compress_default(
            input.as_ptr() as *const c_char,
            out.as_mut_ptr() as *mut c_char,
            input.len() as c_int,
            out.len() as c_int,
        )
    };
    out.truncate(len.max(0) as usize);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dict_is_full_64k() {
        assert_eq!(DICT_FIVE.len(), 65536);
    }

    #[test]
    fn plain_roundtrip_via_dict_decompressor() {
        // A payload compressed dictless can still be decompressed with a dict
        // argument as long as no back-reference actually reaches into the
        // dictionary window — true for short standalone buffers. This checks
        // our FFI wiring end to end without needing a matching C++ compressor.
        let original = b"netClones test payload \x01\x02\x03 with some repetition repetition";
        let compressed = compress_default(original);
        let restored = decompress_using_dict(&compressed, 4096).unwrap();
        assert_eq!(&restored, original);
    }

    #[test]
    fn garbage_input_returns_none() {
        // Truncated/garbage LZ4 stream must fail cleanly, not panic.
        assert!(decompress_using_dict(&[0xFF, 0xFF, 0xFF, 0xFF], 4096).is_none());
    }
}
