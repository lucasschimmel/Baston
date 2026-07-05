//! Object-ID leasing wire format (`msgRequestObjectIds` → `msgObjectIds`).
//!
//! The response is a byte-serialized run-length list: a `u16` entry count
//! followed by `(gap: u16, size: u16)` pairs, decoded as `last += gap + 1; emit
//! last..=last+size` (`net-packet/include/ObjectIds.h`, `SetIds`/`GetIds`). The
//! server hands out 6 IDs in big mode, 32 otherwise
//! (`RequestObjectIdsPacketHandler.cpp:40`).

use crate::udp::hash_rage_string;

/// `HashRageString("msgObjectIds")` — server→client id grant.
pub const MSG_OBJECT_IDS: u32 = hash_rage_string("msgObjectIds");
/// `HashRageString("msgRequestObjectIds")` — client→server id request.
pub const MSG_REQUEST_OBJECT_IDS: u32 = hash_rage_string("msgRequestObjectIds");

/// IDs handed out per request (`fx::IsBigMode() ? 6 : 32`).
pub fn ids_per_request(big_mode: bool) -> usize {
    if big_mode {
        6
    } else {
        32
    }
}

/// Run-length compress a sorted id list into `(gap, size)` entries, matching
/// `ServerObjectIds::SetIds`. `gap = id - 2 - last`; `size` counts the
/// consecutive run after the first.
fn compress(ids: &[u16]) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    let mut last: i32 = -1;
    let mut i = 0usize;
    while i < ids.len() {
        let gap = (ids[i] as i32 - 2 - last) as u16;
        let mut size: u16 = 0;
        i += 1;
        while i < ids.len() {
            if ids[i] != ids[i - 1] + 1 {
                break;
            }
            size += 1;
            i += 1;
        }
        last = ids[i - 1] as i32;
        out.push((gap, size));
    }
    out
}

/// Decompress `(gap, size)` entries back into ids, matching
/// `ServerObjectIds::GetIds`: `last += gap + 1`, then emit `size + 1`
/// consecutive ids starting at `last`. Used by tests and simulated clients.
fn decompress(entries: &[(u16, u16)]) -> Vec<u16> {
    let mut ids = Vec::new();
    let mut last: u16 = 0;
    for &(gap, size) in entries {
        last = last.wrapping_add(gap.wrapping_add(1));
        // `last` post-increments through the run and is left one past the last
        // emitted id, exactly as the C++ `objectId = last++` loop leaves it.
        for _ in 0..=size {
            ids.push(last);
            last = last.wrapping_add(1);
        }
    }
    ids
}

/// Build a `msgObjectIds` response packet: `[u32 type][u16 count][(u16 gap, u16
/// size)...]`, all little-endian bytes.
pub fn build_object_ids(ids: &[u16]) -> Vec<u8> {
    let entries = compress(ids);
    let mut out = MSG_OBJECT_IDS.to_le_bytes().to_vec();
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (gap, size) in entries {
        out.extend_from_slice(&gap.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
    }
    out
}

/// Parse a `msgObjectIds` payload (message type already stripped) back into
/// ids. Mirror of [`build_object_ids`] for tests / headless clients.
pub fn parse_object_ids(payload: &[u8]) -> Option<Vec<u16>> {
    if payload.len() < 2 {
        return None;
    }
    let count = u16::from_le_bytes(payload[..2].try_into().ok()?) as usize;
    let mut entries = Vec::with_capacity(count);
    let mut off = 2;
    for _ in 0..count {
        let gap = u16::from_le_bytes(payload.get(off..off + 2)?.try_into().ok()?);
        let size = u16::from_le_bytes(payload.get(off + 2..off + 4)?.try_into().ok()?);
        entries.push((gap, size));
        off += 4;
    }
    Some(decompress(&entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_request_counts() {
        assert_eq!(ids_per_request(true), 6);
        assert_eq!(ids_per_request(false), 32);
    }

    #[test]
    fn roundtrip_contiguous_block() {
        let ids: Vec<u16> = (1..=32).collect();
        let packet = build_object_ids(&ids);
        assert_eq!(
            u32::from_le_bytes(packet[..4].try_into().unwrap()),
            MSG_OBJECT_IDS
        );
        let decoded = parse_object_ids(&packet[4..]).unwrap();
        assert_eq!(decoded, ids);
    }

    #[test]
    fn roundtrip_sparse_ids() {
        let ids = vec![5u16, 6, 7, 20, 21, 100];
        let packet = build_object_ids(&ids);
        let decoded = parse_object_ids(&packet[4..]).unwrap();
        assert_eq!(decoded, ids);
    }

    #[test]
    fn roundtrip_single_id() {
        let ids = vec![8191u16];
        let packet = build_object_ids(&ids);
        assert_eq!(parse_object_ids(&packet[4..]).unwrap(), ids);
    }
}
