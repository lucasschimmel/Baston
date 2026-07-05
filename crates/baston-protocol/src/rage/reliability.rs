//! Reliability packets: `gameStateNAck` (NAK mode) and `gameStateAck` (ARQ).
//!
//! Both are byte-serialized little-endian (`GameStateNAck.h` / `GameStateAck.h`).
//! Arrays use the `SmallBytesArray` convention: a `u8` element count followed by
//! packed elements. `IgnoreListEntry` is `#pragma pack(1)`: `u16 entry` +
//! `u64 lastFrame` = 10 bytes.
//!
//! BASTON's NG reliability applies these against the per-client sparse view: a
//! missing-frame range rolls the client's base frame back so the next tick
//! re-sends everything since; ignore-list entries roll back a single entity;
//! the recreate list forces a re-create. No frame-snapshot backlog is kept, so
//! a client can never be dropped for "NACK outside the backlog".

use crate::udp::hash_rage_string;

/// `HashRageString("gameStateNAck")`.
pub const GAME_STATE_NACK: u32 = hash_rage_string("gameStateNAck");
/// `HashRageString("gameStateAck")`.
pub const GAME_STATE_ACK: u32 = hash_rage_string("gameStateAck");

pub mod flags {
    pub const IS_MISSING_FRAMES: u8 = 1 << 0;
    pub const HAS_IGNORE_LIST: u8 = 1 << 1;
    pub const HAS_RECREATE_LIST: u8 = 1 << 2;
    pub const HAS_IGNORE_LIST_ENTRY_RESEND_FRAME: u8 = 1 << 3;
}

/// One `IgnoreListEntry`: an entity the client couldn't apply, with the frame
/// it wants resent from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IgnoreEntry {
    pub object_id: u16,
    pub last_frame: u64,
}

/// A parsed `gameStateNAck` (NAK mode).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GameStateNAck {
    pub flags: u8,
    pub frame_index: u64,
    /// `Some((first, last))` when the missing-frames flag is set.
    pub missing_frames: Option<(u64, u64)>,
    pub ignore_list: Vec<IgnoreEntry>,
    pub recreate_list: Vec<u16>,
}

/// A parsed `gameStateAck` (ARQ mode): positive ack for a frame, with optional
/// still-broken entities.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GameStateAck {
    pub frame_index: u64,
    pub ignore_list: Vec<IgnoreEntry>,
    pub recreate_list: Vec<u16>,
}

/// Little-endian byte cursor over a payload.
struct Reader<'a> {
    buf: &'a [u8],
    off: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, off: 0 }
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.buf.get(self.off)?;
        self.off += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let v = u16::from_le_bytes(self.buf.get(self.off..self.off + 2)?.try_into().ok()?);
        self.off += 2;
        Some(v)
    }
    fn u64(&mut self) -> Option<u64> {
        let v = u64::from_le_bytes(self.buf.get(self.off..self.off + 8)?.try_into().ok()?);
        self.off += 8;
        Some(v)
    }
    fn ignore_list(&mut self) -> Option<Vec<IgnoreEntry>> {
        let count = self.u8()? as usize;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let object_id = self.u16()?;
            let last_frame = self.u64()?;
            out.push(IgnoreEntry {
                object_id,
                last_frame,
            });
        }
        Some(out)
    }
    fn recreate_list(&mut self) -> Option<Vec<u16>> {
        let count = self.u8()? as usize;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(self.u16()?);
        }
        Some(out)
    }
}

/// Parse a `gameStateNAck` payload (message type already stripped).
pub fn parse_nack(payload: &[u8]) -> Option<GameStateNAck> {
    let mut r = Reader::new(payload);
    let flags = r.u8()?;
    let frame_index = r.u64()?;
    let missing_frames = if flags & flags::IS_MISSING_FRAMES != 0 {
        Some((r.u64()?, r.u64()?))
    } else {
        None
    };
    let ignore_list = if flags & flags::HAS_IGNORE_LIST != 0 {
        r.ignore_list()?
    } else {
        Vec::new()
    };
    let recreate_list = if flags & flags::HAS_RECREATE_LIST != 0 {
        r.recreate_list()?
    } else {
        Vec::new()
    };
    Some(GameStateNAck {
        flags,
        frame_index,
        missing_frames,
        ignore_list,
        recreate_list,
    })
}

/// Parse a `gameStateAck` payload (message type already stripped).
pub fn parse_ack(payload: &[u8]) -> Option<GameStateAck> {
    let mut r = Reader::new(payload);
    let frame_index = r.u64()?;
    let ignore_list = r.ignore_list()?;
    let recreate_list = r.recreate_list()?;
    Some(GameStateAck {
        frame_index,
        ignore_list,
        recreate_list,
    })
}

/// Build a `gameStateNAck` payload (without the leading message type) — used by
/// simulated clients and round-trip tests.
pub fn build_nack(nack: &GameStateNAck) -> Vec<u8> {
    let mut out = vec![nack.flags];
    out.extend_from_slice(&nack.frame_index.to_le_bytes());
    if let Some((first, last)) = nack.missing_frames {
        out.extend_from_slice(&first.to_le_bytes());
        out.extend_from_slice(&last.to_le_bytes());
    }
    if nack.flags & flags::HAS_IGNORE_LIST != 0 {
        out.push(nack.ignore_list.len() as u8);
        for e in &nack.ignore_list {
            out.extend_from_slice(&e.object_id.to_le_bytes());
            out.extend_from_slice(&e.last_frame.to_le_bytes());
        }
    }
    if nack.flags & flags::HAS_RECREATE_LIST != 0 {
        out.push(nack.recreate_list.len() as u8);
        for &id in &nack.recreate_list {
            out.extend_from_slice(&id.to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_frames_roundtrip() {
        let nack = GameStateNAck {
            flags: flags::IS_MISSING_FRAMES,
            frame_index: 0x1122_3344_5566_7788,
            missing_frames: Some((100, 105)),
            ignore_list: Vec::new(),
            recreate_list: Vec::new(),
        };
        assert_eq!(parse_nack(&build_nack(&nack)).unwrap(), nack);
    }

    #[test]
    fn ignore_and_recreate_lists_roundtrip() {
        let nack = GameStateNAck {
            flags: flags::HAS_IGNORE_LIST | flags::HAS_RECREATE_LIST,
            frame_index: 7,
            missing_frames: None,
            ignore_list: vec![
                IgnoreEntry {
                    object_id: 42,
                    last_frame: 900,
                },
                IgnoreEntry {
                    object_id: 43,
                    last_frame: 901,
                },
            ],
            recreate_list: vec![10, 20, 30],
        };
        let bytes = build_nack(&nack);
        assert_eq!(parse_nack(&bytes).unwrap(), nack);
    }

    #[test]
    fn empty_nack_has_no_optional_sections() {
        let nack = GameStateNAck {
            flags: 0,
            frame_index: 5,
            ..Default::default()
        };
        let bytes = build_nack(&nack);
        // flags(1) + frameIndex(8) only.
        assert_eq!(bytes.len(), 9);
        assert_eq!(parse_nack(&bytes).unwrap(), nack);
    }

    #[test]
    fn ignore_entry_is_ten_bytes_packed() {
        let nack = GameStateNAck {
            flags: flags::HAS_IGNORE_LIST,
            frame_index: 0,
            missing_frames: None,
            ignore_list: vec![IgnoreEntry {
                object_id: 1,
                last_frame: 2,
            }],
            recreate_list: Vec::new(),
        };
        let bytes = build_nack(&nack);
        // flags(1) + frame(8) + count(1) + entry(2 + 8) = 20
        assert_eq!(bytes.len(), 20);
    }

    #[test]
    fn truncated_payload_returns_none() {
        assert!(parse_nack(&[flags::IS_MISSING_FRAMES, 0, 0]).is_none());
    }

    #[test]
    fn ack_roundtrip() {
        // Build an ack payload by hand: frame(8) + ignore(count=1 + entry) + recreate(count=0)
        let mut payload = 55u64.to_le_bytes().to_vec();
        payload.push(1);
        payload.extend_from_slice(&9u16.to_le_bytes());
        payload.extend_from_slice(&800u64.to_le_bytes());
        payload.push(0);
        let ack = parse_ack(&payload).unwrap();
        assert_eq!(ack.frame_index, 55);
        assert_eq!(
            ack.ignore_list,
            vec![IgnoreEntry {
                object_id: 9,
                last_frame: 800
            }]
        );
        assert!(ack.recreate_list.is_empty());
    }
}
