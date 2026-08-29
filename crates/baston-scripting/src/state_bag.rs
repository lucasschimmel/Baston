//! Shared state-bag storage and routing-bucket control surfaces.
//!
//! These primitives deliberately do not depend on `baston-zone`: the script
//! host can use the in-memory implementation today and the authoritative zone
//! can install another [`RoutingControl`] implementation later.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

/// Bounds protect the host when a resource produces changes faster than
/// callbacks or the network can consume them.
const MAX_PENDING_DELIVERIES_PER_RESOURCE: usize = 4_096;
const MAX_REPLICATED_CHANGES: usize = 16_384;

/// Metadata attached to a state-bag mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateBagSource {
    /// Resource that initiated the write, when it originated from scripting.
    pub resource: Option<String>,
    /// Network player that initiated the write, when known.
    pub player: Option<u32>,
}

impl StateBagSource {
    pub fn resource(name: impl Into<String>) -> Self {
        Self {
            resource: Some(name.into()),
            player: None,
        }
    }
}

/// One versioned state-bag mutation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateBagChange {
    pub bag: String,
    pub key: String,
    pub value: serde_json::Value,
    pub version: u64,
    pub replicated: bool,
    pub source: StateBagSource,
}

/// A callback delivery. The callback itself remains in the JS isolate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateBagDelivery {
    pub callback_id: u32,
    pub change: StateBagChange,
}

#[derive(Debug, Clone)]
struct Handler {
    resource: String,
    callback_id: u32,
    key_filter: Option<String>,
    bag_filter: Option<String>,
}

#[derive(Default)]
struct StateBagInner {
    bags: HashMap<String, BTreeMap<String, (serde_json::Value, u64)>>,
    handlers: HashMap<u32, Handler>,
    pending: HashMap<String, VecDeque<StateBagDelivery>>,
    replicated: VecDeque<StateBagChange>,
}

/// Thread-safe, process-wide state-bag store shared by every resource runtime.
#[derive(Clone, Default)]
pub struct StateBagStore {
    inner: Arc<Mutex<StateBagInner>>,
    next_version: Arc<AtomicU64>,
    next_cookie: Arc<AtomicU32>,
}

impl StateBagStore {
    /// Ensure a bag exists without producing a mutation.
    pub fn ensure_bag(&self, bag: impl Into<String>) {
        self.lock().bags.entry(bag.into()).or_default();
    }

    pub fn get(&self, bag: &str, key: &str) -> Option<serde_json::Value> {
        self.lock()
            .bags
            .get(bag)
            .and_then(|values| values.get(key))
            .map(|(value, _)| value.clone())
    }

    pub fn contains_key(&self, bag: &str, key: &str) -> bool {
        self.lock()
            .bags
            .get(bag)
            .is_some_and(|values| values.contains_key(key))
    }

    pub fn keys(&self, bag: &str) -> Vec<String> {
        self.lock()
            .bags
            .get(bag)
            .map(|values| values.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Store a value and notify matching local handlers. Only writes with
    /// `replicated == true` enter the networking drain.
    pub fn set(
        &self,
        bag: impl Into<String>,
        key: impl Into<String>,
        value: serde_json::Value,
        replicated: bool,
        source: StateBagSource,
    ) -> StateBagChange {
        let bag = bag.into();
        let key = key.into();
        let version = self.next_version.fetch_add(1, Ordering::Relaxed) + 1;
        let change = StateBagChange {
            bag: bag.clone(),
            key: key.clone(),
            value: value.clone(),
            version,
            replicated,
            source,
        };

        let mut inner = self.lock();
        inner
            .bags
            .entry(bag)
            .or_default()
            .insert(key, (value, version));
        let deliveries: Vec<_> = inner
            .handlers
            .values()
            .filter(|handler| {
                handler
                    .key_filter
                    .as_ref()
                    .is_none_or(|filter| filter == &change.key)
                    && handler
                        .bag_filter
                        .as_ref()
                        .is_none_or(|filter| filter == &change.bag)
            })
            .map(|handler| {
                (
                    handler.resource.clone(),
                    StateBagDelivery {
                        callback_id: handler.callback_id,
                        change: change.clone(),
                    },
                )
            })
            .collect();
        for (resource, delivery) in deliveries {
            let queue = inner.pending.entry(resource).or_default();
            if queue.len() >= MAX_PENDING_DELIVERIES_PER_RESOURCE {
                queue.pop_front();
                metrics::counter!(
                    "state_bag_changes_dropped_total",
                    "queue" => "callback"
                )
                .increment(1);
            }
            queue.push_back(delivery);
        }
        if replicated {
            // State replication is last-write-wins. Coalescing an unsent
            // mutation for the same key preserves the newest version while
            // preventing a hot key from consuming the entire bounded queue.
            if let Some(index) = inner
                .replicated
                .iter()
                .rposition(|pending| pending.bag == change.bag && pending.key == change.key)
            {
                inner.replicated.remove(index);
            } else if inner.replicated.len() >= MAX_REPLICATED_CHANGES {
                inner.replicated.pop_front();
                metrics::counter!(
                    "state_bag_changes_dropped_total",
                    "queue" => "replication"
                )
                .increment(1);
            }
            inner.replicated.push_back(change.clone());
        }
        change
    }

    /// Register a handler and return a process-unique, non-zero cookie.
    pub fn add_handler(
        &self,
        resource: impl Into<String>,
        key_filter: Option<String>,
        bag_filter: Option<String>,
        callback_id: u32,
    ) -> u32 {
        let cookie = loop {
            let candidate = self.next_cookie.fetch_add(1, Ordering::Relaxed) + 1;
            if candidate != 0 {
                break candidate;
            }
        };
        self.lock().handlers.insert(
            cookie,
            Handler {
                resource: resource.into(),
                callback_id,
                key_filter: normalize_filter(key_filter),
                bag_filter: normalize_filter(bag_filter),
            },
        );
        cookie
    }

    /// Remove a handler only when it belongs to the calling resource.
    pub fn remove_handler(&self, resource: &str, cookie: u32) -> bool {
        let mut inner = self.lock();
        if inner
            .handlers
            .get(&cookie)
            .is_some_and(|handler| handler.resource == resource)
        {
            inner.handlers.remove(&cookie);
            true
        } else {
            false
        }
    }

    pub fn drain_deliveries(&self, resource: &str, limit: usize) -> Vec<StateBagDelivery> {
        let mut inner = self.lock();
        let Some(queue) = inner.pending.get_mut(resource) else {
            return Vec::new();
        };
        let count = limit.min(queue.len());
        let deliveries = queue.drain(..count).collect();
        if queue.is_empty() {
            inner.pending.remove(resource);
        }
        deliveries
    }

    pub fn pending_deliveries(&self) -> usize {
        self.lock().pending.values().map(VecDeque::len).sum()
    }

    pub fn drain_replicated(&self, limit: usize) -> Vec<StateBagChange> {
        let mut inner = self.lock();
        let count = limit.min(inner.replicated.len());
        inner.replicated.drain(..count).collect()
    }

    pub fn cleanup_resource(&self, resource: &str) {
        let mut inner = self.lock();
        inner
            .handlers
            .retain(|_, handler| handler.resource != resource);
        inner.pending.remove(resource);
    }

    pub fn remove_bag(&self, bag: &str) {
        self.lock().bags.remove(bag);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StateBagInner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

fn normalize_filter(filter: Option<String>) -> Option<String> {
    filter.and_then(|value| (!value.is_empty()).then_some(value))
}

pub fn entity_state_bag_name(entity: u32) -> String {
    format!("entity:{entity}")
}

pub fn player_state_bag_name(player: u32) -> String {
    format!("player:{player}")
}

pub fn entity_from_state_bag_name(name: &str) -> Option<u32> {
    parse_bag_id(name, "entity:")
}

pub fn player_from_state_bag_name(name: &str) -> Option<u32> {
    parse_bag_id(name, "player:")
}

fn parse_bag_id(name: &str, prefix: &str) -> Option<u32> {
    name.strip_prefix(prefix)?.parse().ok()
}

/// CFX routing-bucket lockdown policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingLockdownMode {
    #[default]
    Inactive,
    Relaxed,
    Strict,
}

impl RoutingLockdownMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "inactive" => Some(Self::Inactive),
            "relaxed" => Some(Self::Relaxed),
            "strict" => Some(Self::Strict),
            _ => None,
        }
    }
}

/// Zone-independent routing control used by server scripting natives.
pub trait RoutingControl: Send + Sync {
    /// Monotonic change marker for cheap control-plane synchronization.
    fn revision(&self) -> u64;
    fn entity_bucket(&self, entity: u32) -> u32;
    fn set_entity_bucket(&self, entity: u32, bucket: u32);
    fn remove_entity(&self, entity: u32);
    fn player_bucket(&self, player: u32) -> u32;
    fn set_player_bucket(&self, player: u32, bucket: u32);
    fn lockdown_mode(&self, bucket: u32) -> RoutingLockdownMode;
    fn set_lockdown_mode(&self, bucket: u32, mode: RoutingLockdownMode);
    fn population_enabled(&self, bucket: u32) -> bool;
    fn set_population_enabled(&self, bucket: u32, enabled: bool);
}

/// Default thread-safe implementation. Missing assignments resolve to bucket
/// zero and missing policies match FXServer's permissive defaults.
#[derive(Default)]
pub struct InMemoryRoutingControl {
    entities: DashMap<u32, u32>,
    players: DashMap<u32, u32>,
    lockdown: DashMap<u32, RoutingLockdownMode>,
    population: DashMap<u32, bool>,
    revision: AtomicU64,
}

impl RoutingControl for InMemoryRoutingControl {
    fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    fn entity_bucket(&self, entity: u32) -> u32 {
        self.entities.get(&entity).map(|value| *value).unwrap_or(0)
    }

    fn set_entity_bucket(&self, entity: u32, bucket: u32) {
        if bucket == 0 {
            self.entities.remove(&entity);
        } else {
            self.entities.insert(entity, bucket);
        }
        self.revision.fetch_add(1, Ordering::Release);
    }

    fn remove_entity(&self, entity: u32) {
        self.entities.remove(&entity);
        self.revision.fetch_add(1, Ordering::Release);
    }

    fn player_bucket(&self, player: u32) -> u32 {
        self.players.get(&player).map(|value| *value).unwrap_or(0)
    }

    fn set_player_bucket(&self, player: u32, bucket: u32) {
        if bucket == 0 {
            self.players.remove(&player);
        } else {
            self.players.insert(player, bucket);
        }
        self.revision.fetch_add(1, Ordering::Release);
    }

    fn lockdown_mode(&self, bucket: u32) -> RoutingLockdownMode {
        self.lockdown
            .get(&bucket)
            .map(|value| *value)
            .unwrap_or_default()
    }

    fn set_lockdown_mode(&self, bucket: u32, mode: RoutingLockdownMode) {
        self.lockdown.insert(bucket, mode);
        self.revision.fetch_add(1, Ordering::Release);
    }

    fn population_enabled(&self, bucket: u32) -> bool {
        self.population
            .get(&bucket)
            .map(|value| *value)
            .unwrap_or(true)
    }

    fn set_population_enabled(&self, bucket: u32, enabled: bool) {
        self.population.insert(bucket, enabled);
        self.revision.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_filters_and_replication_are_explicit() {
        let store = StateBagStore::default();
        let cookie = store.add_handler(
            "resource-a",
            Some("health".into()),
            Some("entity:42".into()),
            7,
        );
        assert_ne!(cookie, 0);

        let local = store.set(
            "entity:42",
            "health",
            serde_json::json!(90),
            false,
            StateBagSource::resource("writer"),
        );
        let replicated = store.set(
            "entity:42",
            "health",
            serde_json::json!(80),
            true,
            StateBagSource::resource("writer"),
        );
        store.set(
            "entity:42",
            "armour",
            serde_json::json!(50),
            true,
            StateBagSource::resource("writer"),
        );

        assert!(replicated.version > local.version);
        let deliveries = store.drain_deliveries("resource-a", 32);
        assert_eq!(deliveries.len(), 2);
        assert!(deliveries.iter().all(|item| item.callback_id == 7));
        let outbound = store.drain_replicated(32);
        assert_eq!(outbound.len(), 2);
        assert!(outbound.iter().all(|change| change.replicated));
    }

    #[test]
    fn replication_is_coalesced_and_callback_backlog_is_bounded() {
        let store = StateBagStore::default();
        store.add_handler("slow-resource", None, None, 1);

        for version in 0..(MAX_PENDING_DELIVERIES_PER_RESOURCE + 3) {
            store.set(
                "global",
                "hot-key",
                serde_json::json!(version),
                true,
                StateBagSource::resource("writer"),
            );
        }

        let outbound = store.drain_replicated(usize::MAX);
        assert_eq!(outbound.len(), 1);
        assert_eq!(
            outbound[0].value,
            serde_json::json!(MAX_PENDING_DELIVERIES_PER_RESOURCE + 2)
        );
        assert_eq!(
            store.drain_deliveries("slow-resource", usize::MAX).len(),
            MAX_PENDING_DELIVERIES_PER_RESOURCE
        );
    }

    #[test]
    fn cookies_are_resource_scoped_and_cleanup_is_complete() {
        let store = StateBagStore::default();
        let cookie = store.add_handler("owner", None, None, 1);
        assert!(!store.remove_handler("other", cookie));
        assert!(store.remove_handler("owner", cookie));
        let cookie = store.add_handler("owner", None, None, 2);
        store.set(
            "global",
            "key",
            serde_json::json!("value"),
            false,
            StateBagSource::resource("writer"),
        );
        assert_eq!(store.pending_deliveries(), 1);
        store.cleanup_resource("owner");
        assert!(!store.remove_handler("owner", cookie));
        assert_eq!(store.pending_deliveries(), 0);
    }

    #[test]
    fn bag_name_helpers_are_strict() {
        assert_eq!(entity_state_bag_name(12), "entity:12");
        assert_eq!(player_state_bag_name(34), "player:34");
        assert_eq!(entity_from_state_bag_name("entity:12"), Some(12));
        assert_eq!(player_from_state_bag_name("player:34"), Some(34));
        assert_eq!(entity_from_state_bag_name("player:12"), None);
        assert_eq!(player_from_state_bag_name("player:not-a-number"), None);
    }

    #[test]
    fn routing_defaults_and_policies_are_safe() {
        let routing = InMemoryRoutingControl::default();
        let initial_revision = routing.revision();
        assert_eq!(routing.player_bucket(1), 0);
        assert_eq!(routing.entity_bucket(2), 0);
        assert_eq!(routing.lockdown_mode(9), RoutingLockdownMode::Inactive);
        assert!(routing.population_enabled(9));
        routing.set_player_bucket(1, 9);
        routing.set_entity_bucket(2, 9);
        routing.set_lockdown_mode(9, RoutingLockdownMode::Strict);
        routing.set_population_enabled(9, false);
        assert!(routing.revision() > initial_revision);
        assert_eq!(routing.player_bucket(1), 9);
        assert_eq!(routing.entity_bucket(2), 9);
        assert_eq!(routing.lockdown_mode(9), RoutingLockdownMode::Strict);
        assert!(!routing.population_enabled(9));
        routing.set_player_bucket(1, 0);
        routing.set_entity_bucket(2, 0);
        assert_eq!(routing.player_bucket(1), 0);
        assert_eq!(routing.entity_bucket(2), 0);
    }
}
