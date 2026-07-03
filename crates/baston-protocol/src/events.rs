//! Net events over the game transport (`net-packet/include/NetEvent.h`).
//!
//! - Server → client `msgNetEvent` (`ServerClientEventPacket`):
//!   `u32 hash | u16 sourceNetId (0xFFFF from server) | u16 nameLen | name+NUL | msgpack args`
//! - Client → server `msgServerEvent` (`ClientServerEventPacket`):
//!   `u32 hash | u16 nameLen | name+NUL | msgpack args`
//!
//! `nameLen` counts the trailing NUL (ConstrainedBytesArray<2, u16::MAX>).
//! Event arguments are a msgpack array.

use crate::udp::MSG_NET_EVENT;

/// Server → client event (TriggerClientEvent).
pub fn build_net_event(event_name: &str, msgpack_args: &[u8]) -> Vec<u8> {
    let mut out = MSG_NET_EVENT.to_le_bytes().to_vec();
    // sourceNetId is always -1 when sent by the server.
    out.extend_from_slice(&u16::MAX.to_le_bytes());
    let name_len = (event_name.len() + 1) as u16; // includes NUL
    out.extend_from_slice(&name_len.to_le_bytes());
    out.extend_from_slice(event_name.as_bytes());
    out.push(0);
    out.extend_from_slice(msgpack_args);
    out
}

/// Parsed client → server event (TriggerServerEvent).
#[derive(Debug, PartialEq, Eq)]
pub struct ServerEvent<'a> {
    pub name: String,
    pub msgpack_args: &'a [u8],
}

/// Parse the payload following the `msgServerEvent` type field.
pub fn parse_server_event(payload: &[u8]) -> Option<ServerEvent<'_>> {
    if payload.len() < 2 {
        return None;
    }
    let name_len = u16::from_le_bytes(payload[..2].try_into().ok()?) as usize;
    if name_len < 1 || payload.len() < 2 + name_len {
        return None;
    }
    let raw_name = &payload[2..2 + name_len];
    // Trailing NUL is included in the length.
    let name = raw_name.strip_suffix(&[0]).unwrap_or(raw_name);
    Some(ServerEvent {
        name: String::from_utf8_lossy(name).into_owned(),
        msgpack_args: &payload[2 + name_len..],
    })
}

/// Encode script-side JSON args into the msgpack array the client expects.
pub fn json_args_to_msgpack(args_json: &str) -> Result<Vec<u8>, String> {
    let args: serde_json::Value =
        serde_json::from_str(args_json).map_err(|e| format!("invalid args JSON: {e}"))?;
    rmp_serde::to_vec(&args).map_err(|e| format!("msgpack encode failed: {e}"))
}

/// Decode msgpack event args into JSON for the script runtimes.
pub fn msgpack_to_json_args(data: &[u8]) -> Result<serde_json::Value, String> {
    if data.is_empty() {
        return Ok(serde_json::Value::Array(Vec::new()));
    }
    rmp_serde::from_slice(data).map_err(|e| format!("msgpack decode failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp::read_message_type;

    #[test]
    fn net_event_layout() {
        let args = json_args_to_msgpack(r#"[{"model":"mp_m_freemode_01"},1.5]"#).unwrap();
        let packet = build_net_event("axiom:core:spawnCharacter", &args);
        let (ty, payload) = read_message_type(&packet).unwrap();
        assert_eq!(ty, MSG_NET_EVENT);
        assert_eq!(u16::from_le_bytes(payload[..2].try_into().unwrap()), 0xFFFF);
        let name_len = u16::from_le_bytes(payload[2..4].try_into().unwrap()) as usize;
        assert_eq!(name_len, "axiom:core:spawnCharacter".len() + 1);
        assert_eq!(payload[4 + name_len - 1], 0, "NUL terminated");
    }

    #[test]
    fn server_event_roundtrip() {
        let args = json_args_to_msgpack(r#"["hello",42]"#).unwrap();
        // Build a client→server packet by hand.
        let mut payload = Vec::new();
        payload.extend_from_slice(&(("myEvent".len() + 1) as u16).to_le_bytes());
        payload.extend_from_slice(b"myEvent\0");
        payload.extend_from_slice(&args);

        let event = parse_server_event(&payload).unwrap();
        assert_eq!(event.name, "myEvent");
        let json = msgpack_to_json_args(event.msgpack_args).unwrap();
        assert_eq!(json, serde_json::json!(["hello", 42]));
    }

    #[test]
    fn malformed_server_event_rejected() {
        assert!(parse_server_event(&[]).is_none());
        assert!(
            parse_server_event(&[10, 0, b'x']).is_none(),
            "truncated name"
        );
    }
}
