//! `msgRoute` — non-OneSync P2P game-sync relay (jalon C4).
//!
//! In non-OneSync mode the GTA netcode itself synchronizes entities between
//! clients; the server only relays the opaque sync payloads
//! (`RoutingPacketHandler.h`):
//! - client → server: `u32 msgRoute | u16 targetNetId | u16 len | data`
//! - server → target: `u32 msgRoute | u16 senderNetId | u16 len | data`
//!   (channel 1, unreliable)

use super::MSG_ROUTE;

/// A parsed client `msgRoute` payload (message type already stripped).
#[derive(Debug, PartialEq, Eq)]
pub struct ClientRoute<'a> {
    pub target_net_id: u16,
    pub data: &'a [u8],
}

/// Parse `u16 targetNetId | u16 len | data`. The length field is unused by
/// the C++ handler ("unusedPacketLength") — the payload is the stream tail.
pub fn parse_client_route(payload: &[u8]) -> Option<ClientRoute<'_>> {
    if payload.len() < 4 {
        return None;
    }
    let target_net_id = u16::from_le_bytes(payload[..2].try_into().ok()?);
    Some(ClientRoute {
        target_net_id,
        data: &payload[4..],
    })
}

/// Build the relayed packet toward the target client.
pub fn build_server_route(sender_net_id: u16, data: &[u8]) -> Vec<u8> {
    let mut out = MSG_ROUTE.to_le_bytes().to_vec();
    out.extend_from_slice(&sender_net_id.to_le_bytes());
    out.extend_from_slice(&(data.len() as u16).to_le_bytes());
    out.extend_from_slice(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp::read_message_type;

    #[test]
    fn route_roundtrip() {
        let packet = build_server_route(7, b"syncdata");
        let (ty, payload) = read_message_type(&packet).unwrap();
        assert_eq!(ty, MSG_ROUTE);
        let route = parse_client_route(payload).unwrap();
        assert_eq!(route.target_net_id, 7);
        assert_eq!(route.data, b"syncdata");
    }

    #[test]
    fn short_payload_rejected() {
        assert!(parse_client_route(&[1, 0, 0]).is_none());
    }
}
