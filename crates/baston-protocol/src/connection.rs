//! HTTP connection-phase messages (jalon B2).
//!
//! Field names and values mirror FXServer:
//! - `POST /client method=initConnect` request/response —
//!   `components/citizen-server-impl/src/InitConnectMethod.cpp` +
//!   `components/net/src/NetLibrary.cpp` (client side).
//! - `POST /client method=getConfiguration` response —
//!   `components/citizen-server-impl/src/GetConfigurationMethod.cpp` +
//!   `ResourceConfigurationCacheComponent.cpp`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `data["protocol"] = 5` (InitConnectMethod.cpp).
pub const PROTOCOL_VERSION: u32 = 5;
/// Minimum client protocol accepted (`if (protocol < 12)` — client sends its
/// own NETWORK_PROTOCOL which must be >= 12).
pub const MIN_CLIENT_PROTOCOL: u32 = 12;
/// `net::NetBitVersion::netVersion5` = BuildNetVersion(2025, 01, 01, 00, 00):
/// decimal date digits packed as hex nibbles (NetBitVersion.h).
pub const NET_BIT_VERSION: u64 = 0x2025_0101_0000;
/// `GameServerNet.ENet.cpp GetClientVersion()` — the game transport is ENet v2.
pub const NETLIB_VERSION: u32 = 2;
/// Default packfile name served per resource (`ResourceFilesComponent`).
pub const DEFAULT_RESOURCE_SET: &str = "resource.rpf";

/// Parsed fields of `POST /client method=initConnect` (NetLibrary.cpp postMap).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InitConnectRequest {
    pub name: String,
    /// Decimal u64 string (ROS GUID).
    pub guid: String,
    /// Client NETWORK_PROTOCOL number.
    pub protocol: u32,
    /// `"<build>"` or `"<build>_<revision>"`.
    #[serde(rename = "gameBuild")]
    pub game_build: Option<String>,
    #[serde(rename = "gameName")]
    pub game_name: Option<String>,
    #[serde(rename = "cfxTicket2")]
    pub cfx_ticket2: Option<String>,
    /// Steam auth ticket (optional).
    #[serde(rename = "authTicket")]
    pub auth_ticket: Option<String>,
}

/// Successful `initConnect` response body (InitConnectMethod.cpp `data`).
/// The client hard-fails when `sH` is missing (`node["sH"].is_null()`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitConnectResponse {
    pub protocol: u32,
    #[serde(rename = "bitVersion")]
    pub bit_version: u64,
    /// `sv_pureLevel`.
    pub pure: u32,
    /// `sv_scriptHookAllowed`.
    #[serde(rename = "sH")]
    pub script_hook_allowed: bool,
    #[serde(rename = "enhancedHostSupport")]
    pub enhanced_host_support: bool,
    pub onesync: bool,
    pub onesync_big: bool,
    pub onesync_lh: bool,
    pub onesync_population: bool,
    /// Connection token — identifies the client in follow-up requests
    /// (`X-CitizenFX-Token` header) and the UDP handshake.
    pub token: String,
    pub gamename: String,
    #[serde(rename = "netlibVersion")]
    pub netlib_version: u32,
    #[serde(rename = "maxClients")]
    pub max_clients: u32,
    /// Deferral handover data (empty object by default).
    pub handover: serde_json::Value,
}

impl InitConnectResponse {
    /// Response with BASTON's fixed protocol constants filled in.
    pub fn new(token: String, game_name: &str, max_clients: u32) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            bit_version: NET_BIT_VERSION,
            pure: 0,
            script_hook_allowed: false,
            // FXServer sends `sv_enhancedHostSupport && !IsOneSync()`; BASTON
            // is always non-OneSync and the gateway implements the
            // sessionmanager arbitration (hostingSession/sessionHostResult),
            // so the client can fall back to hosting when a P2P join fails
            // instead of failing fast (NetHook.cpp HS_JOIN_FAILURE).
            enhanced_host_support: true,
            onesync: false,
            onesync_big: false,
            onesync_lh: false,
            onesync_population: false,
            token,
            gamename: game_name.to_owned(),
            netlib_version: NETLIB_VERSION,
            max_clients,
            handover: serde_json::Value::Object(Default::default()),
        }
    }
}

/// One resource entry in the `getConfiguration` response
/// (ResourceConfigurationCacheComponent::GetConfiguration).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceConfiguration {
    pub name: String,
    /// Downloadable sets: `{ "resource.rpf": "<sha1 hex of the packfile>" }`.
    pub files: BTreeMap<String, String>,
    /// Streaming assets (empty for BASTON Phase B — no `stream/` support).
    #[serde(rename = "streamFiles")]
    pub stream_files: BTreeMap<String, serde_json::Value>,
}

/// `getConfiguration` response (GetConfigurationMethod.cpp).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetConfigurationResponse {
    /// URL template — the client substitutes the server host for `%s`.
    #[serde(rename = "fileServer")]
    pub file_server: String,
    pub resources: Vec<ResourceConfiguration>,
}

/// Internal handoff between the HTTP phase and the UDP phase (B3): the
/// session token issued by `initConnect` authenticates the first UDP packet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionTicket {
    pub session_token: String,
    pub source: u32,
    pub udp_port: u16,
    pub resources: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_connect_response_uses_fxserver_field_names() {
        let response = InitConnectResponse::new("tok-123".into(), "gta5", 32);
        let json = serde_json::to_value(&response).unwrap();
        // Field names the client actually checks (NetLibrary.cpp).
        assert_eq!(json["protocol"], 5);
        assert_eq!(json["bitVersion"], 0x2025_0101_0000u64);
        assert!(json["sH"].is_boolean(), "client errors when sH is null");
        assert_eq!(json["netlibVersion"], 2);
        assert_eq!(json["maxClients"], 32);
        assert_eq!(json["token"], "tok-123");
        assert!(json["handover"].is_object());
    }

    #[test]
    fn configuration_roundtrip() {
        let config = GetConfigurationResponse {
            file_server: "https://%s/files".into(),
            resources: vec![ResourceConfiguration {
                name: "axiom-core".into(),
                files: BTreeMap::from([("resource.rpf".to_owned(), "ab12".to_owned())]),
                stream_files: BTreeMap::new(),
            }],
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: GetConfigurationResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.resources[0].name, "axiom-core");
        assert_eq!(back.resources[0].files["resource.rpf"], "ab12");
        assert!(json.contains("\"fileServer\""));
        assert!(json.contains("\"streamFiles\""));
    }

    #[test]
    fn connection_ticket_roundtrip() {
        let ticket = ConnectionTicket {
            session_token: "uuid".into(),
            source: 1,
            udp_port: 30120,
            resources: vec!["axiom-core".into()],
        };
        let bytes = serde_json::to_vec(&ticket).unwrap();
        let back: ConnectionTicket = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.udp_port, 30120);
        assert_eq!(back.resources, vec!["axiom-core".to_owned()]);
    }
}
