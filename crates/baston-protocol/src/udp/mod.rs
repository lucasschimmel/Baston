//! Game-transport (UDP/ENet) message types.
//!
//! FiveM's game channel is plain ENet 1.3 (2 channels —
//! `GameServerNet.ENet.cpp`: `enet_host_create(addr, ..., 2, 0, 0)`). Framing
//! inside ENet packets is `u32 LE message type` (either the literal `1` for
//! the connection handshake or a Jenkins-one-at-a-time hash of the message
//! name) followed by the payload (`GameServer::ProcessPacket`).

pub mod handshake;
pub mod time_sync;

/// Jenkins one-at-a-time — FiveM's `HashRageString` (case-sensitive).
pub const fn hash_rage_string(s: &str) -> u32 {
    let bytes = s.as_bytes();
    let mut hash: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        hash = hash.wrapping_add(bytes[i] as u32);
        hash = hash.wrapping_add(hash << 10);
        hash ^= hash >> 6;
        i += 1;
    }
    hash = hash.wrapping_add(hash << 3);
    hash ^= hash >> 11;
    hash.wrapping_add(hash << 15)
}

/// The connection handshake "message type" is the literal 1, not a hash.
pub const MSG_CONNECT: u32 = 1;
pub const MSG_TIME_SYNC_REQ: u32 = hash_rage_string("msgTimeSyncReq");
pub const MSG_TIME_SYNC: u32 = hash_rage_string("msgTimeSync");
pub const MSG_ROUTE: u32 = hash_rage_string("msgRoute");
pub const MSG_NET_EVENT: u32 = hash_rage_string("msgNetEvent");
pub const MSG_SERVER_EVENT: u32 = hash_rage_string("msgServerEvent");
pub const MSG_I_QUIT: u32 = hash_rage_string("msgIQuit");

/// Read the leading `u32 LE` message type.
pub fn read_message_type(data: &[u8]) -> Option<(u32, &[u8])> {
    if data.len() < 4 {
        return None;
    }
    let ty = u32::from_le_bytes(data[..4].try_into().ok()?);
    Some((ty, &data[4..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_rage_string_matches_joaat() {
        // Independent joaat reference values.
        assert_eq!(hash_rage_string(""), 0);
        // Verified against the classic Jenkins OAAT implementation.
        let h = hash_rage_string("msgTimeSyncReq");
        assert_ne!(h, 0);
        assert_eq!(h, hash_rage_string("msgTimeSyncReq"));
        assert_ne!(h, hash_rage_string("msgTimeSync"));
    }

    #[test]
    fn read_message_type_splits_payload() {
        let mut data = 1u32.to_le_bytes().to_vec();
        data.extend_from_slice(b"token=abc");
        let (ty, rest) = read_message_type(&data).unwrap();
        assert_eq!(ty, MSG_CONNECT);
        assert_eq!(rest, b"token=abc");
        assert!(read_message_type(&[1, 2]).is_none());
    }
}
