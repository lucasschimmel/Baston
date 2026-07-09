//! Shared, thread-safe directory of connected players.
//!
//! Owned by the gateway (which allocates sources and inserts/removes players)
//! and shared read-only with the script runtimes so player natives
//! (`GetPlayerName`, `GetPlayerIdentifierByType`, `GetPlayers`) return real
//! data.

use std::sync::atomic::{AtomicU32, Ordering};

use dashmap::DashMap;

use crate::PlayerInfo;

/// Process-wide player table, keyed by FiveM source id.
#[derive(Default)]
pub struct PlayerDirectory {
    players: DashMap<u32, PlayerInfo>,
    /// Connection token (from `initConnect`) → source. Authenticates
    /// follow-up HTTP calls (`X-CitizenFX-Token`) and the UDP handshake.
    session_tokens: DashMap<String, u32>,
    next_source: AtomicU32,
}

impl PlayerDirectory {
    pub fn new() -> Self {
        // Register the gauge at 0 up front so Grafana shows an empty server as
        // "0 players" instead of "No data" until the first connection.
        metrics::gauge!("baston_players_online").set(0.0);
        Self {
            players: DashMap::new(),
            session_tokens: DashMap::new(),
            // FiveM source ids start at 1; 0 is "no player".
            next_source: AtomicU32::new(1),
        }
    }

    /// Bind a connection token to a source (issued at `initConnect`).
    pub fn bind_token(&self, token: String, source: u32) {
        self.session_tokens.insert(token, source);
    }

    /// Resolve a connection token back to its source.
    pub fn source_for_token(&self, token: &str) -> Option<u32> {
        self.session_tokens.get(token).map(|s| *s)
    }

    /// Allocate the next source id.
    pub fn allocate_source(&self) -> u32 {
        self.next_source.fetch_add(1, Ordering::Relaxed)
    }

    pub fn insert(&self, player: PlayerInfo) {
        self.players.insert(player.source, player);
        self.publish_online_gauge();
    }

    pub fn remove(&self, source: u32) -> Option<PlayerInfo> {
        self.session_tokens.retain(|_, s| *s != source);
        let removed = self.players.remove(&source).map(|(_, p)| p);
        self.publish_online_gauge();
        removed
    }

    /// Mirror the live player count into the `baston_players_online` gauge.
    /// Called after every insert/remove so the value tracks `count()` exactly
    /// without a separate counter that could drift.
    fn publish_online_gauge(&self) {
        metrics::gauge!("baston_players_online").set(self.players.len() as f64);
    }

    pub fn get(&self, source: u32) -> Option<PlayerInfo> {
        self.players.get(&source).map(|p| p.value().clone())
    }

    /// The player's identifier of the given type (`license`, `steam`, `ip`, ...),
    /// FXServer `GetPlayerIdentifierByType` semantics: full `type:value` string.
    pub fn identifier_by_type(&self, source: u32, id_type: &str) -> Option<String> {
        let prefix = format!("{id_type}:");
        self.players.get(&source).and_then(|p| {
            p.identifiers
                .iter()
                .find(|id| id.starts_with(&prefix))
                .cloned()
        })
    }

    pub fn exists(&self, source: u32) -> bool {
        self.players.contains_key(&source)
    }

    pub fn identifier_count(&self, source: u32) -> usize {
        self.players
            .get(&source)
            .map(|p| p.identifiers.len())
            .unwrap_or_default()
    }

    pub fn identifier_at(&self, source: u32, index: usize) -> Option<String> {
        self.players
            .get(&source)
            .and_then(|p| p.identifiers.get(index).cloned())
    }

    pub fn endpoint(&self, source: u32) -> Option<String> {
        self.identifier_by_type(source, "ip")
            .map(|id| id.trim_start_matches("ip:").to_owned())
    }

    pub fn guid(&self, source: u32) -> Option<String> {
        self.identifier_by_type(source, "license")
            .or_else(|| self.identifier_at(source, 0))
    }

    pub fn sources(&self) -> Vec<u32> {
        self.players.iter().map(|p| *p.key()).collect()
    }

    pub fn count(&self) -> usize {
        self.players.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_lookup_by_type() {
        let dir = PlayerDirectory::new();
        let source = dir.allocate_source();
        dir.insert(PlayerInfo {
            source,
            name: "Lucas".into(),
            identifiers: vec![
                "license:abc123".into(),
                "steam:76561198000000000".into(),
                "ip:127.0.0.1".into(),
            ],
        });
        assert_eq!(
            dir.identifier_by_type(source, "license").as_deref(),
            Some("license:abc123")
        );
        assert_eq!(
            dir.identifier_by_type(source, "steam").as_deref(),
            Some("steam:76561198000000000")
        );
        assert_eq!(dir.identifier_by_type(source, "discord"), None);
        assert_eq!(dir.identifier_by_type(999, "license"), None);
        assert!(dir.exists(source));
        assert_eq!(dir.identifier_count(source), 3);
        assert_eq!(
            dir.identifier_at(source, 1).as_deref(),
            Some("steam:76561198000000000")
        );
        assert_eq!(dir.endpoint(source).as_deref(), Some("127.0.0.1"));
        assert_eq!(dir.guid(source).as_deref(), Some("license:abc123"));
    }

    #[test]
    fn players_online_gauge_tracks_insert_and_remove() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshot};

        fn online(snapshot: Snapshot) -> Option<f64> {
            snapshot.into_vec().into_iter().find_map(|(ck, _, _, v)| {
                if ck.key().name() == "baston_players_online" {
                    if let DebugValue::Gauge(g) = v {
                        return Some(g.0);
                    }
                }
                None
            })
        }

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        let dir = PlayerDirectory::new();
        metrics::with_local_recorder(&recorder, || {
            let mk = |source| PlayerInfo {
                source,
                name: format!("p{source}"),
                identifiers: vec![],
            };
            let a = dir.allocate_source();
            dir.insert(mk(a));
            let b = dir.allocate_source();
            dir.insert(mk(b));
            // Two connected → gauge is 2.
            assert_eq!(online(snapshotter.snapshot()), Some(2.0));

            dir.remove(a);
            // One left → gauge drops to 1.
            assert_eq!(online(snapshotter.snapshot()), Some(1.0));

            // Removing an unknown source must not move the gauge.
            dir.remove(9999);
            assert_eq!(online(snapshotter.snapshot()), Some(1.0));
        });
    }
}
