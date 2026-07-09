//! Mumble PDS (Packet Data Stream) varint codec.
//!
//! The UDP voice payload encodes session id, sequence and frame lengths with
//! Mumble's own variable-length integer scheme (NOT protobuf varint). Ported
//! from Mumble's `PacketDataStream`. Encoding by leading-byte prefix:
//!
//! ```text
//! 0xxxxxxx                          7-bit  value  (1 byte)
//! 10xxxxxx +1                       14-bit value  (2 bytes)
//! 110xxxxx +2                       21-bit value  (3 bytes)
//! 1110xxxx +3                       28-bit value  (4 bytes)
//! 111100__ +4 (full u32)            32-bit value  (5 bytes)
//! 111101__ +8 (full u64)            64-bit value  (9 bytes)
//! 111110__ recursive negative       -(varint)
//! 111111xx                          small negative (-1..-4)
//! ```
//!
//! We implement the unsigned path (session/sequence/lengths are unsigned) plus
//! decode tolerance for the negative forms so a hostile client can't desync us.

/// Reader over a PDS-encoded byte slice.
pub struct PdsReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> PdsReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes consumed so far.
    pub fn offset(&self) -> usize {
        self.pos
    }

    /// Bytes still unread (e.g. the trailing positional block).
    pub fn remaining(&self) -> &'a [u8] {
        &self.buf[self.pos.min(self.buf.len())..]
    }

    fn next_byte(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    /// Consume one raw byte (opaque codec-frame data), failing on truncation.
    /// Distinct from [`read_u64`](Self::read_u64), which decodes a varint.
    pub fn read_raw_byte(&mut self) -> Option<u8> {
        self.next_byte()
    }

    /// Decode one unsigned varint. Returns `None` on truncation.
    pub fn read_u64(&mut self) -> Option<u64> {
        let v0 = self.next_byte()? as u64;
        // 0xxxxxxx
        if v0 & 0x80 == 0 {
            return Some(v0);
        }
        // 10xxxxxx + 1
        if v0 & 0xC0 == 0x80 {
            let b1 = self.next_byte()? as u64;
            return Some(((v0 & 0x3F) << 8) | b1);
        }
        // 110xxxxx + 2
        if v0 & 0xE0 == 0xC0 {
            let b1 = self.next_byte()? as u64;
            let b2 = self.next_byte()? as u64;
            return Some(((v0 & 0x1F) << 16) | (b1 << 8) | b2);
        }
        // 1110xxxx + 3
        if v0 & 0xF0 == 0xE0 {
            let b1 = self.next_byte()? as u64;
            let b2 = self.next_byte()? as u64;
            let b3 = self.next_byte()? as u64;
            return Some(((v0 & 0x0F) << 24) | (b1 << 16) | (b2 << 8) | b3);
        }
        // 111100__ full 32-bit
        if v0 & 0xFC == 0xF0 {
            let mut v = 0u64;
            for _ in 0..4 {
                v = (v << 8) | self.next_byte()? as u64;
            }
            return Some(v);
        }
        // 111101__ full 64-bit
        if v0 & 0xFC == 0xF4 {
            let mut v = 0u64;
            for _ in 0..8 {
                v = (v << 8) | self.next_byte()? as u64;
            }
            return Some(v);
        }
        // 111110__ recursive negative — read and discard the magnitude so the
        // stream stays aligned; we surface it as 0 (callers treat unsigned).
        if v0 & 0xFC == 0xF8 {
            let _ = self.read_u64()?;
            return Some(0);
        }
        // 111111xx small negative (-1..-4): 2 low bits, no extra bytes.
        Some(0u64.wrapping_sub((v0 & 0x03) + 1))
    }

    /// Convenience for the common u32-range fields (session, sequence).
    pub fn read_u32(&mut self) -> Option<u32> {
        self.read_u64().map(|v| v as u32)
    }
}

/// Append an unsigned varint to `out` using the minimal PDS encoding.
pub fn write_u64(out: &mut Vec<u8>, value: u64) {
    if value < 0x80 {
        out.push(value as u8);
    } else if value < 0x4000 {
        out.push(0x80 | (value >> 8) as u8);
        out.push((value & 0xFF) as u8);
    } else if value < 0x20_0000 {
        out.push(0xC0 | (value >> 16) as u8);
        out.push((value >> 8) as u8);
        out.push((value & 0xFF) as u8);
    } else if value < 0x1000_0000 {
        out.push(0xE0 | (value >> 24) as u8);
        out.push((value >> 16) as u8);
        out.push((value >> 8) as u8);
        out.push((value & 0xFF) as u8);
    } else if value < 0x1_0000_0000 {
        out.push(0xF0);
        out.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        out.push(0xF4);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: u64) {
        let mut buf = Vec::new();
        write_u64(&mut buf, v);
        let mut r = PdsReader::new(&buf);
        assert_eq!(r.read_u64(), Some(v), "value {v} did not roundtrip");
        assert_eq!(
            r.offset(),
            buf.len(),
            "reader consumed wrong length for {v}"
        );
    }

    #[test]
    fn roundtrips_all_size_classes() {
        for v in [
            0u64,
            1,
            0x7F,
            0x80,
            0x3FFF,
            0x4000,
            0x1F_FFFF,
            0x20_0000,
            0x0FFF_FFFF,
            0x1000_0000,
            0xFFFF_FFFF,
            0x1_0000_0000,
            u64::MAX,
        ] {
            roundtrip(v);
        }
    }

    #[test]
    fn minimal_encoding_lengths() {
        let lens = |v: u64| {
            let mut b = Vec::new();
            write_u64(&mut b, v);
            b.len()
        };
        assert_eq!(lens(0x7F), 1);
        assert_eq!(lens(0x80), 2);
        assert_eq!(lens(0x3FFF), 2);
        assert_eq!(lens(0x4000), 3);
        assert_eq!(lens(0x1F_FFFF), 3);
        assert_eq!(lens(0x20_0000), 4);
        assert_eq!(lens(0x0FFF_FFFF), 4);
        assert_eq!(lens(0x1000_0000), 5);
        assert_eq!(lens(0x1_0000_0000), 9);
    }

    #[test]
    fn truncated_input_returns_none() {
        // Leading byte claims a 3-byte form but only 1 byte present.
        let mut r = PdsReader::new(&[0xC0]);
        assert_eq!(r.read_u64(), None);
    }

    #[test]
    fn remaining_exposes_trailing_positional_block() {
        // session varint (0x2A) then 12 trailing bytes (fake position floats).
        let mut buf = Vec::new();
        write_u64(&mut buf, 42);
        buf.extend_from_slice(&[9u8; 12]);
        let mut r = PdsReader::new(&buf);
        assert_eq!(r.read_u32(), Some(42));
        assert_eq!(r.remaining(), &[9u8; 12]);
    }
}
