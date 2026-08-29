//! Thread-safe routing-bucket assignment and policy registry.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

pub type RoutingBucketId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LockdownMode {
    #[default]
    Inactive,
    Relaxed,
    Strict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketPolicy {
    pub lockdown: LockdownMode,
    pub population_enabled: bool,
}

impl Default for BucketPolicy {
    fn default() -> Self {
        Self {
            lockdown: LockdownMode::Inactive,
            population_enabled: true,
        }
    }
}

#[derive(Debug, Default)]
struct RegistryState {
    players: HashMap<u32, RoutingBucketId>,
    entities: HashMap<u16, RoutingBucketId>,
    policies: HashMap<RoutingBucketId, BucketPolicy>,
}

#[derive(Debug, Clone, Default)]
pub struct RoutingBucketRegistry {
    inner: Arc<RwLock<RegistryState>>,
}

impl RoutingBucketRegistry {
    pub fn player_bucket(&self, source: u32) -> RoutingBucketId {
        self.inner
            .read()
            .expect("routing bucket registry poisoned")
            .players
            .get(&source)
            .copied()
            .unwrap_or(0)
    }

    pub fn entity_bucket(&self, object_id: u16) -> RoutingBucketId {
        self.inner
            .read()
            .expect("routing bucket registry poisoned")
            .entities
            .get(&object_id)
            .copied()
            .unwrap_or(0)
    }

    pub fn set_player_bucket(&self, source: u32, bucket: RoutingBucketId) {
        let mut state = self
            .inner
            .write()
            .expect("routing bucket registry poisoned");
        if bucket == 0 {
            state.players.remove(&source);
        } else {
            state.players.insert(source, bucket);
        }
    }

    pub fn set_entity_bucket(&self, object_id: u16, bucket: RoutingBucketId) {
        let mut state = self
            .inner
            .write()
            .expect("routing bucket registry poisoned");
        if bucket == 0 {
            state.entities.remove(&object_id);
        } else {
            state.entities.insert(object_id, bucket);
        }
    }

    pub fn remove_player(&self, source: u32) {
        self.inner
            .write()
            .expect("routing bucket registry poisoned")
            .players
            .remove(&source);
    }

    pub fn remove_entity(&self, object_id: u16) {
        self.inner
            .write()
            .expect("routing bucket registry poisoned")
            .entities
            .remove(&object_id);
    }

    pub fn policy(&self, bucket: RoutingBucketId) -> BucketPolicy {
        self.inner
            .read()
            .expect("routing bucket registry poisoned")
            .policies
            .get(&bucket)
            .copied()
            .unwrap_or_default()
    }

    pub fn set_lockdown(&self, bucket: RoutingBucketId, lockdown: LockdownMode) {
        self.inner
            .write()
            .expect("routing bucket registry poisoned")
            .policies
            .entry(bucket)
            .or_default()
            .lockdown = lockdown;
    }

    pub fn set_population_enabled(&self, bucket: RoutingBucketId, enabled: bool) {
        self.inner
            .write()
            .expect("routing bucket registry poisoned")
            .policies
            .entry(bucket)
            .or_default()
            .population_enabled = enabled;
    }

    pub fn visible(&self, source: u32, object_id: u16) -> bool {
        self.player_bucket(source) == self.entity_bucket(object_id)
    }

    /// Strict lockdown denies client-originated creates. Relaxed and inactive
    /// accept them; server-side provenance can be added without changing the
    /// visibility model.
    pub fn allows_client_create(&self, source: u32) -> bool {
        self.policy(self.player_bucket(source)).lockdown != LockdownMode::Strict
    }

    pub fn allows_takeover(&self, sender: u32, target: u32, object_id: u16) -> bool {
        let bucket = self.entity_bucket(object_id);
        self.player_bucket(sender) == bucket && self.player_bucket(target) == bucket
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_zero_is_the_backward_compatible_default() {
        let registry = RoutingBucketRegistry::default();
        assert!(registry.visible(42, 7));
        assert!(registry.allows_client_create(42));
        assert!(registry.policy(0).population_enabled);
    }

    #[test]
    fn strict_lockdown_rejects_client_create() {
        let registry = RoutingBucketRegistry::default();
        registry.set_player_bucket(1, 9);
        registry.set_lockdown(9, LockdownMode::Strict);
        assert!(!registry.allows_client_create(1));
    }

    #[test]
    fn takeover_requires_all_parties_in_the_entity_bucket() {
        let registry = RoutingBucketRegistry::default();
        registry.set_player_bucket(1, 7);
        registry.set_player_bucket(2, 8);
        registry.set_entity_bucket(100, 7);
        assert!(!registry.allows_takeover(1, 2, 100));
        registry.set_player_bucket(2, 7);
        assert!(registry.allows_takeover(1, 2, 100));
    }
}
