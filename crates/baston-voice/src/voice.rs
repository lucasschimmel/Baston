//! Decrypted UDP voice packet parsing.
//!
//! After OCB2 decryption a voice datagram is:
//!
//! ```text
//! byte 0        (type << 5) | target
//! bytes 1..     PDS varint: session id            (skipped on ingest — the
//!                                                   server trusts the socket,
//!                                                   not the self-declared id)
//!               PDS varint(s): codec frame headers + Opus/CELT payload
//!               optional 12-byte positional block (3 × big-endian f32)
//! ```
//!
//! The server never re-encodes audio: it parses just enough to (a) know the
//! codec/target, (b) find where the optional positional block starts so it can
//! strip or (V3) rewrite it, and (c) forward the opaque payload. Ported from
//! umurmur's `Client_voiceMsg` walk (`client.cpp`).

use crate::pds::PdsReader;
use crate::{VoicePacketType, VoiceTargetKind};

/// A parsed-but-opaque inbound voice packet. `audio_end` marks the byte offset
/// where the optional positional block begins (== `payload.len()` when absent).
#[derive(Debug, Clone)]
pub struct VoicePacket {
    pub codec: VoicePacketType,
    pub target: VoiceTargetKind,
    /// The full decrypted payload (header byte included at index 0).
    pub payload: Vec<u8>,
    /// Offset in `payload` where the positional block starts.
    pub audio_end: usize,
    /// Decoded positional coordinates, if a 12-byte block was present.
    pub position: Option<[f32; 3]>,
}

impl VoicePacket {
    /// Whether a positional block is present.
    pub fn has_position(&self) -> bool {
        self.position.is_some()
    }

    /// The bytes to forward when the receiver must NOT get positional data
    /// (context mismatch, or proximity disabled): audio only.
    pub fn without_position(&self) -> &[u8] {
        &self.payload[..self.audio_end]
    }

    /// Re-emit the packet with a rewritten positional block (V3: server-
    /// authoritative position). Produces a fresh payload = header+audio + pos.
    pub fn with_rewritten_position(&self, pos: [f32; 3]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.audio_end + 12);
        out.extend_from_slice(&self.payload[..self.audio_end]);
        for c in pos {
            out.extend_from_slice(&c.to_be_bytes());
        }
        out
    }
}

/// Encode a server→client voice packet: the speaker's session id is inserted
/// after the header byte (the client→server form carries the sequence varint
/// first; the server prepends who is speaking, as umurmur does). The remaining
/// payload — sequence varint + audio frames (+ position when `with_position`)
/// — is forwarded verbatim.
pub fn encode_outbound(session: u64, packet: &VoicePacket, with_position: bool) -> Vec<u8> {
    let body = if with_position {
        &packet.payload[1..]
    } else {
        &packet.payload[1..packet.audio_end.min(packet.payload.len())]
    };
    let mut out = Vec::with_capacity(1 + 9 + body.len());
    out.push(packet.payload[0]);
    crate::pds::write_u64(&mut out, session);
    out.extend_from_slice(body);
    out
}

/// Parse a decrypted voice payload. Returns `None` on a malformed/truncated
/// packet (the caller drops it — never panics on hostile input).
pub fn parse(payload: &[u8]) -> Option<VoicePacket> {
    let header = *payload.first()?;
    let codec = VoicePacketType::from_bits(header >> 5)?;
    let target = VoiceTargetKind::from_bits(header & 0x1F);

    // Ping packets are handled elsewhere; a normal voice packet must carry a
    // session varint and (usually) codec frames.
    let mut r = PdsReader::new(&payload[1..]);
    // Session id (self-declared) — read and discard; the server keys routing on
    // the authenticated socket, not this field.
    r.read_u64()?;

    match codec {
        VoicePacketType::Opus => {
            // Opus: one varint header, low 13 bits = frame length.
            let hdr = r.read_u64()?;
            let frame_len = (hdr & 0x1FFF) as usize;
            skip(&mut r, frame_len)?;
        }
        VoicePacketType::Ping => {
            // UDP ping is not a voice packet; positional logic doesn't apply.
            let audio_end = 1 + r.offset();
            return Some(VoicePacket {
                codec,
                target,
                payload: payload.to_vec(),
                audio_end: audio_end.min(payload.len()),
                position: None,
            });
        }
        _ => {
            // CELT/Speex: sequence of frame headers; continuation bit 0x80.
            loop {
                let h = r.read_u64()? as u8;
                skip(&mut r, (h & 0x7F) as usize)?;
                if h & 0x80 == 0 {
                    break;
                }
            }
        }
    }

    // Everything after the audio frames is the optional positional block.
    let audio_end = 1 + r.offset();
    let tail = &payload[audio_end.min(payload.len())..];
    let position = decode_position(tail);

    Some(VoicePacket {
        codec,
        target,
        payload: payload.to_vec(),
        audio_end,
        position,
    })
}

/// Advance the reader by `n` bytes of opaque frame data, failing on truncation.
fn skip(r: &mut PdsReader, n: usize) -> Option<()> {
    for _ in 0..n {
        r.read_raw_byte()?;
    }
    Some(())
}

/// Decode a 12-byte positional block (3 × big-endian f32). Anything else → none.
fn decode_position(tail: &[u8]) -> Option<[f32; 3]> {
    if tail.len() != 12 {
        return None;
    }
    let f = |i: usize| f32::from_be_bytes([tail[i], tail[i + 1], tail[i + 2], tail[i + 3]]);
    Some([f(0), f(4), f(8)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pds::write_u64;

    /// Build a minimal Opus voice packet: header, session, one opus frame of
    /// `audio` bytes, optional trailing position.
    fn opus_packet(
        target: VoiceTargetKind,
        session: u64,
        audio: &[u8],
        pos: Option<[f32; 3]>,
    ) -> Vec<u8> {
        let mut p = Vec::new();
        p.push((VoicePacketType::Opus as u8) << 5 | target.to_bits());
        write_u64(&mut p, session);
        write_u64(&mut p, audio.len() as u64); // opus frame header (len in low 13 bits)
        p.extend_from_slice(audio);
        if let Some(c) = pos {
            for v in c {
                p.extend_from_slice(&v.to_be_bytes());
            }
        }
        p
    }

    #[test]
    fn parses_opus_without_position() {
        let pkt = opus_packet(VoiceTargetKind::Normal, 7, &[1, 2, 3, 4], None);
        let v = parse(&pkt).unwrap();
        assert_eq!(v.codec, VoicePacketType::Opus);
        assert_eq!(v.target, VoiceTargetKind::Normal);
        assert!(!v.has_position());
        assert_eq!(v.audio_end, pkt.len());
        assert_eq!(v.without_position(), &pkt[..]);
    }

    #[test]
    fn parses_opus_with_position() {
        let pos = [12.5f32, -80.0, 30.25];
        let pkt = opus_packet(VoiceTargetKind::Whisper(3), 42, &[9; 10], Some(pos));
        let v = parse(&pkt).unwrap();
        assert_eq!(v.target, VoiceTargetKind::Whisper(3));
        assert_eq!(v.position, Some(pos));
        // without_position strips exactly the 12-byte tail.
        assert_eq!(v.without_position().len(), pkt.len() - 12);
    }

    #[test]
    fn rewrite_position_replaces_tail() {
        let pkt = opus_packet(VoiceTargetKind::Normal, 1, &[5; 8], Some([0.0, 0.0, 0.0]));
        let v = parse(&pkt).unwrap();
        let rewritten = v.with_rewritten_position([1.0, 2.0, 3.0]);
        let v2 = parse(&rewritten).unwrap();
        assert_eq!(v2.position, Some([1.0, 2.0, 3.0]));
        assert_eq!(v2.without_position(), v.without_position());
    }

    #[test]
    fn encode_outbound_inserts_session_and_strips_position() {
        let pos = [4.0f32, 5.0, 6.0];
        let pkt = opus_packet(VoiceTargetKind::Normal, 9, &[7; 6], Some(pos));
        let v = parse(&pkt).unwrap();

        // With position: header + session varint + original body (seq..pos).
        let out = encode_outbound(42, &v, true);
        let mut r = crate::pds::PdsReader::new(&out[1..]);
        assert_eq!(r.read_u64(), Some(42), "speaker session prepended");
        assert_eq!(out[0], pkt[0]);
        assert_eq!(&out[1 + r.offset()..], &pkt[1..]);

        // Without position: the 12-byte tail is gone.
        let stripped = encode_outbound(42, &v, false);
        assert_eq!(stripped.len(), out.len() - 12);
    }

    #[test]
    fn truncated_packet_is_rejected() {
        // Claims an 8-byte opus frame but supplies only 2 bytes.
        let mut p = Vec::new();
        p.push((VoicePacketType::Opus as u8) << 5);
        write_u64(&mut p, 1);
        write_u64(&mut p, 8);
        p.extend_from_slice(&[0, 0]);
        assert!(parse(&p).is_none());
    }

    #[test]
    fn empty_payload_is_rejected() {
        assert!(parse(&[]).is_none());
    }
}
