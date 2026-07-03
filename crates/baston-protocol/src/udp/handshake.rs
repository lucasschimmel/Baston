//! UDP connection handshake (`GameServer::ProcessPacket`, msgType == 1).
//!
//! Client → server (reliable, channel 0):
//! `u32 1` + url-encoded `token=<connection token>&guid=<u64>`.
//!
//! Server → client on success:
//! `u32 1` + `" <netId> <hostNetId> <hostNetBase> <slotId> <serverTimeMs>"`
//! (non-OneSync: host fields and slot/time are -1).

use super::MSG_CONNECT;

/// Parsed `msgType 1` handshake payload.
#[derive(Debug, PartialEq, Eq)]
pub struct ConnectPayload {
    pub token: String,
    pub guid: String,
}

pub fn parse_connect(payload: &[u8]) -> Option<ConnectPayload> {
    let text = std::str::from_utf8(payload).ok()?;
    let mut token = None;
    let mut guid = None;
    for (k, v) in form_urlencoded::parse(text.as_bytes()) {
        match k.as_ref() {
            "token" => token = Some(v.into_owned()),
            "guid" => guid = Some(v.into_owned()),
            _ => {}
        }
    }
    Some(ConnectPayload {
        token: token?,
        guid: guid.unwrap_or_default(),
    })
}

/// Build the `connectOK` reply. BASTON Phase B is not OneSync: host net id,
/// host base, slot id and time are all -1.
pub fn build_connect_ok(net_id: u32) -> Vec<u8> {
    let mut out = MSG_CONNECT.to_le_bytes().to_vec();
    out.extend_from_slice(format!(" {net_id} -1 -1 -1 -1").as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect_payload() {
        let parsed = parse_connect(b"token=abc-123&guid=76561198000000000").unwrap();
        assert_eq!(parsed.token, "abc-123");
        assert_eq!(parsed.guid, "76561198000000000");
        assert!(parse_connect(b"guid=1").is_none(), "token required");
    }

    #[test]
    fn connect_ok_format() {
        let ok = build_connect_ok(1);
        assert_eq!(&ok[..4], &1u32.to_le_bytes());
        assert_eq!(&ok[4..], b" 1 -1 -1 -1 -1");
    }
}
