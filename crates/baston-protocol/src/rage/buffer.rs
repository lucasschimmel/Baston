//! RAGE `datBitBuffer`-compatible bit-level buffer.
//!
//! Faithful port of `rl::MessageBuffer` (`RlMessageBuffer.h` in the CitizenFX
//! engine, itself a reimplementation of `rage::datBitBuffer`). The wire format
//! is MSB-first within each byte: the first bit written lands in bit 7 of byte
//! 0. All quirks of the C++ implementation are preserved intentionally:
//! - a failed `read_bits_single` still advances the cursor (the C++ does);
//! - the "length hack" widens every 13-bit field to 16 bits when OneSync
//!   big-id mode is on (`MessageBufferLengthHack`) — here a per-buffer flag
//!   instead of a global;
//! - `require_length` uses a strict `<` comparison;
//! - signed values are encoded as a sign bit followed by `length - 1` bits of
//!   the value XOR'd with the sign extension (not plain two's complement).

/// Bit-level reader/writer over a byte buffer, MSB-first within each byte.
#[derive(Debug, Clone)]
pub struct MessageBuffer {
    data: Vec<u8>,
    cur_bit: usize,
    max_bit: usize,
    /// OneSync big-id mode: widen 13-bit fields to 16 bits on read/write.
    length_hack: bool,
}

impl MessageBuffer {
    /// Writer over a zeroed buffer of `size` bytes.
    pub fn new(size: usize) -> Self {
        Self {
            data: vec![0; size],
            cur_bit: 0,
            max_bit: size * 8,
            length_hack: false,
        }
    }

    /// Reader/writer over existing bytes.
    pub fn from_bytes(data: Vec<u8>) -> Self {
        let max_bit = data.len() * 8;
        Self {
            data,
            cur_bit: 0,
            max_bit,
            length_hack: false,
        }
    }

    /// Enable/disable the 13→16-bit length hack (OneSync big-id mode).
    pub fn with_length_hack(mut self, enabled: bool) -> Self {
        self.length_hack = enabled;
        self
    }

    fn effective_len(&self, length: usize) -> usize {
        if length == 13 && self.length_hack {
            16
        } else {
            length
        }
    }

    /// Read up to 32 bits as an unsigned integer. On overflow the cursor still
    /// advances (matching `ReadBitsSingle`) and `None` is returned.
    pub fn read_bits_single(&mut self, length: usize) -> Option<u32> {
        let length = self.effective_len(length);
        debug_assert!(length <= 32);
        if self.cur_bit + length > self.max_bit {
            self.cur_bit += length;
            return None;
        }
        let mut value: u32 = 0;
        for i in 0..length {
            let bit = self.cur_bit + i;
            let byte = self.data[bit / 8];
            value = (value << 1) | ((byte >> (7 - (bit % 8))) & 1) as u32;
        }
        self.cur_bit += length;
        Some(value)
    }

    /// Write up to 32 bits, MSB-first. Fails (without advancing) on overflow.
    pub fn write_bits_single(&mut self, value: u32, length: usize) -> bool {
        let length = self.effective_len(length);
        debug_assert!(length <= 32);
        if self.cur_bit + length > self.max_bit {
            return false;
        }
        for i in 0..length {
            let bit = self.cur_bit + i;
            let b = ((value >> (length - 1 - i)) & 1) as u8;
            let idx = bit / 8;
            let shift = 7 - (bit % 8);
            self.data[idx] = (self.data[idx] & !(1 << shift)) | (b << shift);
        }
        self.cur_bit += length;
        true
    }

    /// Read a single bit. Out-of-range reads return 0 without failing
    /// (matching `ReadBit`), but still advance the cursor.
    pub fn read_bit(&mut self) -> u8 {
        let idx = self.cur_bit / 8;
        if idx >= self.data.len() {
            self.cur_bit += 1;
            return 0;
        }
        let bit = (self.data[idx] >> (7 - (self.cur_bit % 8))) & 1;
        self.cur_bit += 1;
        bit
    }

    /// Write a single bit; fails if past the end of the buffer.
    pub fn write_bit(&mut self, bit: bool) -> bool {
        let idx = self.cur_bit / 8;
        if idx >= self.data.len() {
            return false;
        }
        let shift = 7 - (self.cur_bit % 8);
        self.data[idx] = (self.data[idx] & !(1 << shift)) | ((bit as u8) << shift);
        self.cur_bit += 1;
        true
    }

    /// Copy `length` bits out of the stream into `dest` (packed MSB-first from
    /// dest bit 0). Port of `ReadBits`/`CopyBits`.
    pub fn read_bits(&mut self, dest: &mut [u8], length: usize) -> bool {
        if length == 0 {
            return true;
        }
        if self.cur_bit + length > self.max_bit {
            return false;
        }
        debug_assert!(dest.len() * 8 >= length);
        copy_bits(dest, &self.data, length, 0, self.cur_bit);
        self.cur_bit += length;
        true
    }

    /// Copy `length` bits from `src` (read MSB-first from src bit 0) into the
    /// stream. Port of `WriteBits`/`CopyBits`.
    pub fn write_bits(&mut self, src: &[u8], length: usize) -> bool {
        if self.cur_bit + length > self.max_bit {
            return false;
        }
        debug_assert!(src.len() * 8 >= length);
        copy_bits(&mut self.data, src, length, self.cur_bit, 0);
        self.cur_bit += length;
        true
    }

    /// Signed read: 1 sign bit + `length - 1` value bits XOR'd with the sign
    /// extension. Port of `ReadSigned`.
    pub fn read_signed(&mut self, length: usize) -> Option<i32> {
        // Guard before `length - 1` underflows (length 0) or overflows a u32
        // read. These bounds hold for every RAGE signed field; a length driven
        // by attacker-supplied bits must never panic here.
        if length == 0 || length > 32 {
            return None;
        }
        let sign = self.read_bits_single(1)? as i32;
        let data = self.read_bits_single(length - 1)? as i32;
        Some(sign + (data ^ -sign))
    }

    /// Signed write, inverse of [`Self::read_signed`]. Port of `WriteSigned`.
    pub fn write_signed(&mut self, value: i32, length: usize) -> bool {
        let sign = (value < 0) as u32;
        let sign_ex: i32 = if value < 0 { -1 } else { 0 };
        let d = (value ^ sign_ex) as u32;
        self.write_bits_single(sign, 1) && self.write_bits_single(d, length - 1)
    }

    /// Quantized unsigned float over `[0, divisor]`. Port of `ReadFloat`.
    pub fn read_float(&mut self, length: usize, divisor: f32) -> Option<f32> {
        // `1u64 << length` is UB for length >= 64 and reads past 32 bits are
        // meaningless here; reject out-of-range lengths from untrusted streams.
        if length == 0 || length > 32 {
            return None;
        }
        let integer = self.read_bits_single(length)? as i32;
        let max = ((1u64 << length) - 1) as f32;
        Some((integer as f32 / max) * divisor)
    }

    /// Port of `WriteFloat`.
    pub fn write_float(&mut self, length: usize, divisor: f32, value: f32) -> bool {
        let max = ((1u64 << length) - 1) as f32;
        let integer = ((value / divisor) * max) as i32;
        self.write_bits_single(integer as u32, length)
    }

    /// Quantized signed float over `[-divisor, divisor]`. Port of
    /// `ReadSignedFloat`.
    pub fn read_signed_float(&mut self, length: usize, divisor: f32) -> Option<f32> {
        let integer = self.read_signed(length)?;
        let max = ((1u64 << (length - 1)) - 1) as f32;
        Some((integer as f32 / max) * divisor)
    }

    /// Port of `WriteSignedFloat`.
    pub fn write_signed_float(&mut self, length: usize, divisor: f32, value: f32) -> bool {
        let max = ((1u64 << (length - 1)) - 1) as f32;
        let integer = ((value / divisor) * max) as i32;
        self.write_signed(integer, length)
    }

    /// Up-to-64-bit read: low 32 bits first, then the high bits (the RAGE
    /// word order). Port of `ReadLong`.
    pub fn read_long(&mut self, length: usize) -> Option<u64> {
        // A u64 read can't exceed 64 bits; anything larger would ask
        // `read_bits_single` for >32 bits and produce garbage.
        if length == 0 || length > 64 {
            return None;
        }
        if length <= 32 {
            self.read_bits_single(length).map(u64::from)
        } else {
            let low = self.read_bits_single(32)?;
            let high = self.read_bits_single(length - 32)?;
            Some(u64::from(low) | (u64::from(high) << 32))
        }
    }

    /// Inverse of [`Self::read_long`].
    pub fn write_long(&mut self, value: u64, length: usize) -> bool {
        if length <= 32 {
            self.write_bits_single(value as u32, length)
        } else {
            self.write_bits_single(value as u32, 32)
                && self.write_bits_single((value >> 32) as u32, length - 32)
        }
    }

    /// Strict `<` like the C++ `RequireLength` (off-by-one preserved).
    pub fn require_length(&self, length: usize) -> bool {
        self.cur_bit + length < self.max_bit
    }

    /// Advance to the next byte boundary.
    pub fn align(&mut self) {
        let r = self.cur_bit % 8;
        if r != 0 {
            self.cur_bit += 8 - r;
        }
    }

    pub fn current_bit(&self) -> usize {
        self.cur_bit
    }

    pub fn set_current_bit(&mut self, bit: usize) {
        self.cur_bit = bit;
    }

    pub fn is_at_end(&self) -> bool {
        self.cur_bit >= self.max_bit
    }

    pub fn buffer(&self) -> &[u8] {
        &self.data
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.data
    }

    /// Bytes touched so far (cursor rounded up to a byte). Port of
    /// `GetDataLength`.
    pub fn data_length(&self) -> usize {
        (self.cur_bit / 8) + usize::from(!self.cur_bit.is_multiple_of(8))
    }
}

/// Copy `length` bits from `src` starting at `src_bit` into `dest` starting at
/// `dest_bit`, MSB-first within each byte. Semantic equivalent of the IDA
/// `CopyBits` in `RlMessageBuffer.h`, written as a clear per-bit loop with a
/// byte-aligned `memcpy` fast path.
fn copy_bits(dest: &mut [u8], src: &[u8], length: usize, dest_bit: usize, src_bit: usize) {
    if dest_bit.is_multiple_of(8) && src_bit.is_multiple_of(8) && length.is_multiple_of(8) {
        let db = dest_bit / 8;
        let sb = src_bit / 8;
        let n = length / 8;
        dest[db..db + n].copy_from_slice(&src[sb..sb + n]);
        return;
    }
    for i in 0..length {
        let s = src_bit + i;
        let bit = (src[s / 8] >> (7 - (s % 8))) & 1;
        let d = dest_bit + i;
        let idx = d / 8;
        let shift = 7 - (d % 8);
        dest[idx] = (dest[idx] & !(1 << shift)) | (bit << shift);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msb_first_layout() {
        let mut buf = MessageBuffer::new(1);
        assert!(buf.write_bits_single(0b1010, 4));
        assert!(buf.write_bits_single(0b0110, 4));
        assert_eq!(buf.buffer(), &[0b1010_0110]);
    }

    #[test]
    fn unsigned_roundtrip_unaligned() {
        let mut buf = MessageBuffer::new(16);
        assert!(buf.write_bits_single(1, 3));
        assert!(buf.write_bits_single(0x1FFF, 13));
        assert!(buf.write_bits_single(0xABCDE, 20));
        buf.set_current_bit(0);
        assert_eq!(buf.read_bits_single(3), Some(1));
        assert_eq!(buf.read_bits_single(13), Some(0x1FFF));
        assert_eq!(buf.read_bits_single(20), Some(0xABCDE));
    }

    #[test]
    fn length_hack_widens_13_bit_fields() {
        let mut buf = MessageBuffer::new(4).with_length_hack(true);
        assert!(buf.write_bits_single(0xFFFF, 13)); // actually 16 bits
        assert_eq!(buf.current_bit(), 16);
        buf.set_current_bit(0);
        assert_eq!(buf.read_bits_single(13), Some(0xFFFF));
    }

    #[test]
    fn failed_read_still_advances_cursor() {
        let mut buf = MessageBuffer::from_bytes(vec![0xFF]);
        assert_eq!(buf.read_bits_single(6), Some(0x3F));
        assert_eq!(buf.read_bits_single(6), None);
        assert_eq!(buf.current_bit(), 12); // advanced past the end, like C++
    }

    #[test]
    fn failed_write_does_not_advance() {
        let mut buf = MessageBuffer::new(1);
        assert!(buf.write_bits_single(0x0F, 4));
        assert!(!buf.write_bits_single(0x1F, 5));
        assert_eq!(buf.current_bit(), 4);
    }

    #[test]
    fn signed_matches_cpp_asymmetry() {
        // The C++ WriteSigned/ReadSigned pair is NOT mutually inverse: each
        // one inverts the *game's* counterpart, and chaining the FiveM pair
        // yields v+1 for negative values. The port preserves this exactly —
        // a "fixed" symmetric codec would desync against real clients.
        for (written, read_back) in [
            (-4096, -4095),
            (-5, -4),
            (-1, 0),
            (0, 0),
            (1, 1),
            (4095, 4095),
        ] {
            let mut buf = MessageBuffer::new(4);
            assert!(buf.write_signed(written, 14));
            buf.set_current_bit(0);
            assert_eq!(buf.read_signed(14), Some(read_back), "value {written}");
        }
    }

    #[test]
    fn float_quantization_roundtrip() {
        // Sector-position style field: ReadFloat(12, 54.0f).
        let mut buf = MessageBuffer::new(4);
        assert!(buf.write_float(12, 54.0, 27.3));
        buf.set_current_bit(0);
        let v = buf.read_float(12, 54.0).unwrap();
        assert!((v - 27.3).abs() < 54.0 / 4095.0, "got {v}");
    }

    #[test]
    fn signed_float_roundtrip() {
        let mut buf = MessageBuffer::new(4);
        assert!(buf.write_signed_float(12, 80.0, -55.5));
        buf.set_current_bit(0);
        let v = buf.read_signed_float(12, 80.0).unwrap();
        // Two quanta of tolerance: one for quantization, one for the signed
        // off-by-one documented in `signed_matches_cpp_asymmetry`.
        assert!((v + 55.5).abs() < 2.0 * 80.0 / 2047.0, "got {v}");
    }

    #[test]
    fn long_uses_low_word_first() {
        let mut buf = MessageBuffer::new(8);
        // Value must fit in 56 bits: high word uses only 24 bits here.
        assert!(buf.write_long(0x0023_4567_89AB_CDEF, 56));
        buf.set_current_bit(0);
        // Low 32 bits come first on the wire.
        assert_eq!(buf.read_bits_single(32), Some(0x89AB_CDEF));
        buf.set_current_bit(0);
        assert_eq!(buf.read_long(56), Some(0x0023_4567_89AB_CDEF));
    }

    #[test]
    fn blob_copy_unaligned_roundtrip() {
        let payload = [0xDE, 0xAD, 0xBE, 0xEF, 0x15];
        let mut buf = MessageBuffer::new(16);
        assert!(buf.write_bits_single(0b101, 3)); // misalign the stream
        assert!(buf.write_bits(&payload, 37)); // non-byte-multiple blob length
        buf.set_current_bit(0);
        assert_eq!(buf.read_bits_single(3), Some(0b101));
        let mut out = [0u8; 5];
        assert!(buf.read_bits(&mut out, 37));
        // Only the first 37 bits are meaningful; mask the tail of byte 4.
        assert_eq!(&out[..4], &payload[..4]);
        assert_eq!(out[4] & 0b1111_1000, payload[4] & 0b1111_1000);
    }

    #[test]
    fn aligned_blob_fast_path() {
        let payload = [1, 2, 3, 4];
        let mut buf = MessageBuffer::new(8);
        assert!(buf.write_bits(&payload, 32));
        assert_eq!(&buf.buffer()[..4], &payload);
        buf.set_current_bit(0);
        let mut out = [0u8; 4];
        assert!(buf.read_bits(&mut out, 32));
        assert_eq!(out, payload);
    }

    #[test]
    fn read_bit_past_end_returns_zero() {
        let mut buf = MessageBuffer::from_bytes(vec![0xFF]);
        buf.set_current_bit(8);
        assert_eq!(buf.read_bit(), 0);
    }

    #[test]
    fn data_length_rounds_up() {
        let mut buf = MessageBuffer::new(4);
        buf.write_bits_single(0, 12);
        assert_eq!(buf.data_length(), 2);
        buf.align();
        assert_eq!(buf.current_bit(), 16);
    }
}
