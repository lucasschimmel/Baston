//! Clock synchronization (`net-packet/include/TimeSync.h`).
//!
//! Client → server (`msgTimeSyncReq`): `u32 hash | u32 requestTime | u32 requestSequence`.
//! Server → client (`msgTimeSync`, reliable, channel 1):
//! `u32 hash | u32 requestTime | u32 requestSequence | u32 serverTimeMillis`
//! (the request is echoed, then the server clock, truncated to 32 bits).

use super::{MSG_TIME_SYNC, MSG_TIME_SYNC_REQ};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSyncRequest {
    pub request_time: u32,
    pub request_sequence: u32,
}

/// Parse the payload following the `msgTimeSyncReq` type field.
pub fn parse_time_sync_request(payload: &[u8]) -> Option<TimeSyncRequest> {
    if payload.len() < 8 {
        return None;
    }
    Some(TimeSyncRequest {
        request_time: u32::from_le_bytes(payload[..4].try_into().ok()?),
        request_sequence: u32::from_le_bytes(payload[4..8].try_into().ok()?),
    })
}

/// Build the full `msgTimeSync` response packet.
pub fn build_time_sync_response(request: TimeSyncRequest, server_time_ms: u64) -> Vec<u8> {
    let mut out = MSG_TIME_SYNC.to_le_bytes().to_vec();
    out.extend_from_slice(&request.request_time.to_le_bytes());
    out.extend_from_slice(&request.request_sequence.to_le_bytes());
    out.extend_from_slice(&((server_time_ms & 0xFFFF_FFFF) as u32).to_le_bytes());
    out
}

/// Build a `msgTimeSyncReq` packet (used by tests / future client tooling).
pub fn build_time_sync_request(request: TimeSyncRequest) -> Vec<u8> {
    let mut out = MSG_TIME_SYNC_REQ.to_le_bytes().to_vec();
    out.extend_from_slice(&request.request_time.to_le_bytes());
    out.extend_from_slice(&request.request_sequence.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp::read_message_type;

    #[test]
    fn request_roundtrip() {
        let request = TimeSyncRequest {
            request_time: 123_456,
            request_sequence: 7,
        };
        let packet = build_time_sync_request(request);
        let (ty, payload) = read_message_type(&packet).unwrap();
        assert_eq!(ty, MSG_TIME_SYNC_REQ);
        assert_eq!(parse_time_sync_request(payload).unwrap(), request);
    }

    #[test]
    fn response_echoes_request_and_truncates_time() {
        let request = TimeSyncRequest {
            request_time: 42,
            request_sequence: 1,
        };
        let packet = build_time_sync_response(request, 0x1_2345_6789);
        let (ty, payload) = read_message_type(&packet).unwrap();
        assert_eq!(ty, MSG_TIME_SYNC);
        assert_eq!(u32::from_le_bytes(payload[..4].try_into().unwrap()), 42);
        assert_eq!(u32::from_le_bytes(payload[4..8].try_into().unwrap()), 1);
        assert_eq!(
            u32::from_le_bytes(payload[8..12].try_into().unwrap()),
            0x2345_6789
        );
    }

    #[test]
    fn short_payload_rejected() {
        assert!(parse_time_sync_request(&[0; 7]).is_none());
    }
}
