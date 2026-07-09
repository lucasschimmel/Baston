//! Mumble TCP control-channel framing.
//!
//! Every control message on the wire is a 6-byte preamble followed by a
//! protobuf payload:
//!
//! ```text
//! bytes 0..2  message type   (u16, big-endian)
//! bytes 2..6  payload length (u32, big-endian)
//! bytes 6..   protobuf payload
//! ```
//!
//! Ported from the Mumble/umurmur framing (`messagehandler.cpp`,
//! `messages.h`). The type table is a hard contract with the stock client.

/// Preamble size in bytes (type u16 + length u32).
pub const PREAMBLE_SIZE: usize = 6;

/// Maximum payload we accept, mirroring umurmur's `MAX_MSGSIZE`
/// (`BUFSIZE 8192` − preamble). Anything larger is a protocol error and we
/// refuse it rather than allocate unbounded.
pub const MAX_MSGSIZE: usize = 8192 - PREAMBLE_SIZE;

/// Mumble control message type tags (`messageType_t`). The numeric values are
/// the on-wire type field and must not be reordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum MessageType {
    Version = 0,
    UdpTunnel = 1,
    Authenticate = 2,
    Ping = 3,
    Reject = 4,
    ServerSync = 5,
    ChannelRemove = 6,
    ChannelState = 7,
    UserRemove = 8,
    UserState = 9,
    BanList = 10,
    TextMessage = 11,
    PermissionDenied = 12,
    Acl = 13,
    QueryUsers = 14,
    CryptSetup = 15,
    ContextActionModify = 16,
    ContextAction = 17,
    UserList = 18,
    VoiceTarget = 19,
    PermissionQuery = 20,
    CodecVersion = 21,
    UserStats = 22,
    RequestBlob = 23,
    ServerConfig = 24,
    SuggestConfig = 25,
}

impl MessageType {
    /// Map an on-wire type tag to its enum. Unknown tags return `None` — the
    /// caller drops the frame rather than guessing.
    pub fn from_u16(v: u16) -> Option<Self> {
        use MessageType::*;
        Some(match v {
            0 => Version,
            1 => UdpTunnel,
            2 => Authenticate,
            3 => Ping,
            4 => Reject,
            5 => ServerSync,
            6 => ChannelRemove,
            7 => ChannelState,
            8 => UserRemove,
            9 => UserState,
            10 => BanList,
            11 => TextMessage,
            12 => PermissionDenied,
            13 => Acl,
            14 => QueryUsers,
            15 => CryptSetup,
            16 => ContextActionModify,
            17 => ContextAction,
            18 => UserList,
            19 => VoiceTarget,
            20 => PermissionQuery,
            21 => CodecVersion,
            22 => UserStats,
            23 => RequestBlob,
            24 => ServerConfig,
            25 => SuggestConfig,
            _ => return None,
        })
    }

    /// The on-wire type tag.
    pub fn tag(self) -> u16 {
        self as u16
    }
}

/// Framing / decode errors on the control channel.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame payload length {0} exceeds MAX_MSGSIZE ({MAX_MSGSIZE})")]
    TooLarge(usize),
    #[error("unknown message type tag {0}")]
    UnknownType(u16),
    #[error("protobuf decode failed for {ty:?}: {source}")]
    Decode {
        ty: MessageType,
        #[source]
        source: prost::DecodeError,
    },
}

/// A parsed preamble: message type plus the declared payload length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preamble {
    pub ty: u16,
    pub len: u32,
}

impl Preamble {
    /// Parse the 6-byte preamble. Returns `None` if fewer than 6 bytes are
    /// available yet (the caller keeps buffering).
    pub fn parse(buf: &[u8]) -> Option<Preamble> {
        if buf.len() < PREAMBLE_SIZE {
            return None;
        }
        let ty = u16::from_be_bytes([buf[0], buf[1]]);
        let len = u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]);
        Some(Preamble { ty, len })
    }

    /// Total frame size (preamble + payload).
    pub fn frame_len(&self) -> usize {
        PREAMBLE_SIZE + self.len as usize
    }
}

/// Encode a protobuf message into a full Mumble frame (preamble + payload).
///
/// Returns `TooLarge` rather than emitting a frame the peer would reject.
pub fn encode<M: prost::Message>(ty: MessageType, msg: &M) -> Result<Vec<u8>, FrameError> {
    let len = msg.encoded_len();
    if len > MAX_MSGSIZE {
        return Err(FrameError::TooLarge(len));
    }
    let mut out = Vec::with_capacity(PREAMBLE_SIZE + len);
    out.extend_from_slice(&ty.tag().to_be_bytes());
    out.extend_from_slice(&(len as u32).to_be_bytes());
    msg.encode(&mut out).expect("Vec grows to encoded_len");
    Ok(out)
}

/// Decode a payload slice into a typed protobuf message.
pub fn decode<M: prost::Message + Default>(
    ty: MessageType,
    payload: &[u8],
) -> Result<M, FrameError> {
    M::decode(payload).map_err(|source| FrameError::Decode { ty, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto;

    #[test]
    fn type_table_roundtrips_every_tag() {
        for tag in 0u16..=25 {
            let ty = MessageType::from_u16(tag).expect("known tag");
            assert_eq!(ty.tag(), tag);
        }
        assert!(MessageType::from_u16(26).is_none());
        assert!(MessageType::from_u16(9999).is_none());
    }

    #[test]
    fn preamble_needs_six_bytes() {
        assert!(Preamble::parse(&[0, 0, 0, 0, 0]).is_none());
        let p = Preamble::parse(&[0, 5, 0, 0, 0, 42, 0xAA]).unwrap();
        assert_eq!(p.ty, 5); // ServerSync
        assert_eq!(p.len, 42);
        assert_eq!(p.frame_len(), PREAMBLE_SIZE + 42);
    }

    #[test]
    fn encode_then_decode_a_real_message() {
        let sync = proto::ServerSync {
            session: Some(7),
            max_bandwidth: Some(128_000),
            welcome_text: Some("hi".into()),
            permissions: Some(0xf0e),
        };
        let frame = encode(MessageType::ServerSync, &sync).unwrap();

        let pre = Preamble::parse(&frame).unwrap();
        assert_eq!(pre.ty, MessageType::ServerSync.tag());
        assert_eq!(pre.frame_len(), frame.len());

        let payload = &frame[PREAMBLE_SIZE..];
        let back: proto::ServerSync = decode(MessageType::ServerSync, payload).unwrap();
        assert_eq!(back.session, Some(7));
        assert_eq!(back.max_bandwidth, Some(128_000));
        assert_eq!(back.permissions, Some(0xf0e));
    }

    #[test]
    fn oversized_payload_is_refused() {
        // A CryptSetup with a huge bogus key would blow past MAX_MSGSIZE.
        let big = proto::TextMessage {
            actor: None,
            session: vec![],
            channel_id: vec![],
            tree_id: vec![],
            message: "x".repeat(MAX_MSGSIZE + 10),
        };
        assert!(matches!(
            encode(MessageType::TextMessage, &big),
            Err(FrameError::TooLarge(_))
        ));
    }
}
