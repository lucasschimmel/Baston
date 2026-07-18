//! `baston-voice` — a Mumble-compatible voice server embedded in
//! baston-gateway.
//!
//! # Design: "Mumble at the wire, custom brain"
//!
//! The stock FiveM client embeds an unmodified Mumble client (Opus audio,
//! OCB2-AES128 UDP crypto, TLS control channel). We do not — and cannot —
//! change it. So the server *speaks* the Mumble protocol at the wire, but
//! everything behind that facade (routing, positional audio, area-of-interest
//! culling, observability) is our own engine, wired into BASTON's
//! server-authoritative game state.
//!
//! This is the same philosophy as the rest of BASTON: talk the FiveM protocol
//! on the outside, do better on the inside.
//!
//! # Module map
//!
//! - [`framing`] — TCP control framing (6-byte preamble + protobuf).
//! - [`pds`] — Mumble's PDS varint codec (UDP voice payload fields).
//! - [`proto`] — generated `MumbleProto` message types.
//!
//! Later stories add: `crypto` (OCB2), `voice` (UDP packet parse/route),
//! `session`/`channel`/`target` (control state machine), `tls` (first-byte
//! mux), and the gateway integration surface.

pub mod channel;
pub mod crypto;
pub mod framing;
pub mod pds;
pub mod proto;
pub mod router;
pub mod server;
pub mod session;
pub mod target;
pub mod voice;

/// Mumble wire protocol version advertised by the server (1.4.0), matching the
/// FXServer umurmur build. Encoded `(major << 16) | (minor << 8) | patch`.
pub const PROTOCOL_VERSION: u32 = (1 << 16) | (4 << 8);

/// UDP voice payload codec/type tag, carried in the top 3 bits of byte 0 of a
/// decrypted voice packet (`UDPMessageType_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VoicePacketType {
    CeltAlpha = 0,
    Ping = 1,
    Speex = 2,
    CeltBeta = 3,
    Opus = 4,
}

impl VoicePacketType {
    /// Decode the 3-bit type field. Unknown values return `None`.
    pub fn from_bits(bits: u8) -> Option<Self> {
        Some(match bits & 0x07 {
            0 => Self::CeltAlpha,
            1 => Self::Ping,
            2 => Self::Speex,
            3 => Self::CeltBeta,
            4 => Self::Opus,
            _ => return None,
        })
    }
}

/// The 5-bit target field of a voice packet header: normal channel talk,
/// a whisper VoiceTarget id (1..=30), or the loopback echo test (31).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceTargetKind {
    /// Speak to the sender's current channel (proximity).
    Normal,
    /// Whisper via a registered VoiceTarget id (1..=30).
    Whisper(u8),
    /// Loopback: echo back to the sender only (mic test).
    Loopback,
}

impl VoiceTargetKind {
    /// Decode the 5-bit target field (`byte0 & 0x1F`).
    pub fn from_bits(bits: u8) -> Self {
        match bits & 0x1F {
            0 => Self::Normal,
            31 => Self::Loopback,
            n => Self::Whisper(n),
        }
    }

    /// The 5-bit on-wire value.
    pub fn to_bits(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Loopback => 31,
            Self::Whisper(n) => n & 0x1F,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_1_4_0() {
        assert_eq!(PROTOCOL_VERSION, 0x0001_0400);
    }

    #[test]
    fn voice_type_decodes_known_codecs() {
        assert_eq!(
            VoicePacketType::from_bits(0),
            Some(VoicePacketType::CeltAlpha)
        );
        assert_eq!(VoicePacketType::from_bits(4), Some(VoicePacketType::Opus));
        assert_eq!(VoicePacketType::from_bits(1), Some(VoicePacketType::Ping));
        assert_eq!(VoicePacketType::from_bits(5), None);
    }

    #[test]
    fn target_kind_roundtrips() {
        assert_eq!(VoiceTargetKind::from_bits(0), VoiceTargetKind::Normal);
        assert_eq!(VoiceTargetKind::from_bits(31), VoiceTargetKind::Loopback);
        assert_eq!(VoiceTargetKind::from_bits(7), VoiceTargetKind::Whisper(7));
        for n in 1u8..=30 {
            let k = VoiceTargetKind::Whisper(n);
            assert_eq!(VoiceTargetKind::from_bits(k.to_bits()), k);
        }
    }
}
