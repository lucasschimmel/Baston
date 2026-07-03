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
    }

    pub fn remove(&self, source: u32) -> Option<PlayerInfo> {
        self.session_tokens.retain(|_, s| *s != source);
        self.players.remove(&source).map(|(_, p)| p)
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
    }
}
