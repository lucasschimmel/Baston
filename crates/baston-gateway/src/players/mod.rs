//! Player registry: source-id allocation and connected-player tracking.

use std::sync::atomic::{AtomicU32, Ordering};

use baston_protocol::PlayerInfo;
use dashmap::DashMap;

/// Registry of connected players, keyed by source id.
#[derive(Default)]
pub struct PlayerRegistry {
    players: DashMap<u32, PlayerInfo>,
    next_source: AtomicU32,
}

impl PlayerRegistry {
    pub fn new() -> Self {
        Self {
            players: DashMap::new(),
            // FiveM source ids start at 1; 0 is "no player".
            next_source: AtomicU32::new(1),
        }
    }

    /// Allocate the next source id.
    pub fn allocate_source(&self) -> u32 {
        self.next_source.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a player that completed the connection flow.
    pub fn insert(&self, player: PlayerInfo) {
        self.players.insert(player.source, player);
    }

    /// Remove a player (disconnect). Returns the removed info if present.
    pub fn remove(&self, source: u32) -> Option<PlayerInfo> {
        self.players.remove(&source).map(|(_, p)| p)
    }

    pub fn get(&self, source: u32) -> Option<PlayerInfo> {
        self.players.get(&source).map(|p| p.value().clone())
    }

    pub fn count(&self) -> usize {
        self.players.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_monotonic_sources_from_one() {
        let reg = PlayerRegistry::new();
        assert_eq!(reg.allocate_source(), 1);
        assert_eq!(reg.allocate_source(), 2);
    }

    #[test]
    fn insert_get_remove() {
        let reg = PlayerRegistry::new();
        let source = reg.allocate_source();
        reg.insert(PlayerInfo {
            source,
            name: "Lucas".into(),
            identifiers: vec!["ip:127.0.0.1".into()],
        });
        assert_eq!(reg.count(), 1);
        assert_eq!(reg.get(source).unwrap().name, "Lucas");
        assert_eq!(reg.remove(source).unwrap().name, "Lucas");
        assert_eq!(reg.count(), 0);
    }
}
