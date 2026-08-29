//! Persistent per-resource key/value storage.
//!
//! Scripts treat `SetResourceKvp` as durable: it is where character data,
//! settings and progression live between restarts. Baston held it in a process
//! map, so every value written since boot vanished on shutdown — silently, with
//! nothing in the logs to suggest anything had been lost.
//!
//! ## Synchronous and deferred writes
//!
//! The native surface already distinguishes the two, and now so does the
//! store. A plain `SetResourceKvp` writes through: it returns once the value is
//! on disk. The `_NO_SYNC` variants only mark the store dirty, which is what
//! makes a bulk write of ten thousand keys fast, and `FlushResourceKvp`
//! forces the pending state out. A background sweep also flushes a dirty store
//! periodically, so a crash costs at most one sweep interval rather than the
//! whole session.
//!
//! ## On-disk shape
//!
//! One JSON object per resource, so the file stays readable and a resource's
//! data can be inspected or removed by hand. Writes go through a temporary
//! file and a rename, so an interrupted flush leaves the previous snapshot
//! intact rather than a truncated one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dashmap::DashMap;

/// Separator between the resource name and the key in the flat map. A resource
/// name cannot contain a NUL, so the pair can never be ambiguous.
const SEPARATOR: char = '\0';

/// Per-resource key/value store, shared by every resource isolate.
pub struct KvpStore {
    entries: DashMap<String, String>,
    /// Where the store persists. `None` keeps it in memory — used by tests and
    /// by any embedding that has not configured a path.
    path: Option<PathBuf>,
    /// Set by deferred writes, cleared by a successful flush.
    dirty: AtomicBool,
}

impl KvpStore {
    /// A store that never touches the disk.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            entries: DashMap::new(),
            path: None,
            dirty: AtomicBool::new(false),
        }
    }

    /// Open the store at `path`, loading whatever is already there.
    ///
    /// A missing file is an empty store. An unreadable or corrupt one is
    /// reported and then treated as empty: refusing to boot over unparseable
    /// KVP would take the whole server down for data that scripts are expected
    /// to tolerate losing, but it must never be lost *quietly*.
    #[must_use]
    pub fn open(path: PathBuf) -> Self {
        let entries = match std::fs::read_to_string(&path) {
            Ok(raw) => {
                match serde_json::from_str::<BTreeMap<String, BTreeMap<String, String>>>(&raw) {
                    Ok(by_resource) => {
                        let entries = DashMap::new();
                        for (resource, keys) in by_resource {
                            for (key, value) in keys {
                                entries.insert(flat_key(&resource, &key), value);
                            }
                        }
                        tracing::info!(
                            target: "kvp",
                            path = %path.display(),
                            entries = entries.len(),
                            "resource KVP loaded"
                        );
                        entries
                    }
                    Err(error) => {
                        tracing::error!(
                            target: "kvp",
                            path = %path.display(),
                            %error,
                            "resource KVP is unreadable and was NOT loaded; starting empty"
                        );
                        DashMap::new()
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DashMap::new(),
            Err(error) => {
                tracing::error!(
                    target: "kvp",
                    path = %path.display(),
                    %error,
                    "resource KVP could not be read; starting empty"
                );
                DashMap::new()
            }
        };
        Self {
            entries,
            path: Some(path),
            dirty: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn get(&self, resource: &str, key: &str) -> Option<String> {
        self.entries
            .get(&flat_key(resource, key))
            .map(|value| value.clone())
    }

    /// Store a value. `sync` writes through to disk before returning.
    pub fn set(&self, resource: &str, key: &str, value: String, sync: bool) {
        self.entries.insert(flat_key(resource, key), value);
        self.mark_dirty(sync);
    }

    /// Remove a value. `sync` writes through to disk before returning.
    pub fn remove(&self, resource: &str, key: &str, sync: bool) {
        self.entries.remove(&flat_key(resource, key));
        self.mark_dirty(sync);
    }

    /// Keys of `resource` starting with `prefix`, sorted so `FindKvp` walks a
    /// stable sequence.
    #[must_use]
    pub fn keys_with_prefix(&self, resource: &str, prefix: &str) -> Vec<String> {
        let scope = flat_key(resource, prefix);
        let mut keys: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| entry.key().starts_with(&scope))
            .filter_map(|entry| key_of(entry.key()).map(str::to_owned))
            .collect();
        keys.sort();
        keys
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    fn mark_dirty(&self, sync: bool) {
        self.dirty.store(true, Ordering::Release);
        if sync {
            self.flush();
        }
    }

    /// Persist the store if anything is pending.
    ///
    /// Failure is logged and counted, and the store stays dirty so the next
    /// flush retries: a full disk must not silently turn durable writes into
    /// volatile ones.
    pub fn flush(&self) {
        let Some(path) = &self.path else {
            self.dirty.store(false, Ordering::Release);
            return;
        };
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Err(error) = self.write_to(path) {
            self.dirty.store(true, Ordering::Release);
            tracing::error!(
                target: "kvp",
                path = %path.display(),
                %error,
                "resource KVP could not be written; data is still only in memory"
            );
            metrics::counter!("kvp_flush_failures_total").increment(1);
        }
    }

    fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let mut by_resource: BTreeMap<&str, BTreeMap<&str, &str>> = BTreeMap::new();
        // Hold the guards for the whole serialization so borrowed strs stay
        // valid; the map is small and this runs off the hot path.
        let snapshot: Vec<_> = self
            .entries
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        for (flat, value) in &snapshot {
            if let (Some(resource), Some(key)) = (resource_of(flat), key_of(flat)) {
                by_resource.entry(resource).or_default().insert(key, value);
            }
        }
        let encoded = serde_json::to_vec_pretty(&by_resource)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        // Temp + rename: an interrupted write leaves the previous snapshot
        // whole instead of a half-written file.
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        std::fs::write(&temporary, encoded)?;
        std::fs::rename(&temporary, path)
    }

    /// Flush a dirty store on a fixed cadence, so deferred writes reach the
    /// disk even when a resource never calls `FlushResourceKvp`.
    pub fn spawn_periodic_flush(store: Arc<Self>, interval: std::time::Duration) {
        if store.path.is_none() {
            return;
        }
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let store = Arc::clone(&store);
                // Serializing and writing is blocking work; keep it off the
                // async worker that other tasks share.
                if tokio::task::spawn_blocking(move || store.flush())
                    .await
                    .is_err()
                {
                    tracing::error!(target: "kvp", "KVP flush task failed");
                }
            }
        });
    }
}

impl Default for KvpStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

fn flat_key(resource: &str, key: &str) -> String {
    format!("{resource}{SEPARATOR}{key}")
}

fn resource_of(flat: &str) -> Option<&str> {
    flat.split_once(SEPARATOR).map(|(resource, _)| resource)
}

fn key_of(flat: &str) -> Option<&str> {
    flat.split_once(SEPARATOR).map(|(_, key)| key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "baston-kvp-test-{name}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn values_are_scoped_per_resource() {
        let store = KvpStore::in_memory();
        store.set("a", "shared", "from-a".into(), false);
        store.set("b", "shared", "from-b".into(), false);

        assert_eq!(store.get("a", "shared").as_deref(), Some("from-a"));
        assert_eq!(store.get("b", "shared").as_deref(), Some("from-b"));
        assert_eq!(store.get("c", "shared"), None);
    }

    /// The whole point: a value written before a restart is still there after.
    #[test]
    fn values_survive_a_restart() {
        let path = temp_path("restart");
        {
            let store = KvpStore::open(path.clone());
            store.set("axiom-core", "character:42", "{\"cash\":100}".into(), true);
            assert!(!store.is_dirty(), "a synchronous write flushed");
        }

        let reopened = KvpStore::open(path.clone());

        assert_eq!(
            reopened.get("axiom-core", "character:42").as_deref(),
            Some("{\"cash\":100}")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Deferred writes are what makes a bulk import fast; they must still land
    /// once the resource asks for them to.
    #[test]
    fn deferred_writes_land_on_flush() {
        let path = temp_path("deferred");
        {
            let store = KvpStore::open(path.clone());
            for index in 0..100 {
                store.set("bulk", &format!("key:{index}"), index.to_string(), false);
            }
            assert!(store.is_dirty(), "deferred writes leave the store dirty");
            // Nothing is on disk yet.
            assert!(KvpStore::open(path.clone()).is_empty());

            store.flush();
            assert!(!store.is_dirty());
        }

        let reopened = KvpStore::open(path.clone());
        assert_eq!(reopened.len(), 100);
        assert_eq!(reopened.get("bulk", "key:7").as_deref(), Some("7"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removal_persists_too() {
        let path = temp_path("removal");
        {
            let store = KvpStore::open(path.clone());
            store.set("r", "keep", "1".into(), false);
            store.set("r", "drop", "2".into(), false);
            store.flush();
            store.remove("r", "drop", true);
        }

        let reopened = KvpStore::open(path.clone());
        assert_eq!(reopened.get("r", "keep").as_deref(), Some("1"));
        assert_eq!(reopened.get("r", "drop"), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prefix_search_is_scoped_and_sorted() {
        let store = KvpStore::in_memory();
        store.set("r", "user:b", "1".into(), false);
        store.set("r", "user:a", "1".into(), false);
        store.set("r", "other", "1".into(), false);
        store.set("elsewhere", "user:z", "1".into(), false);

        assert_eq!(
            store.keys_with_prefix("r", "user:"),
            vec!["user:a", "user:b"]
        );
        assert_eq!(
            store.keys_with_prefix("r", ""),
            vec!["other", "user:a", "user:b"]
        );
    }

    /// Unparseable stored data must not take the server down, but it must not
    /// disappear without a word either.
    #[test]
    fn a_corrupt_file_starts_empty_rather_than_panicking() {
        let path = temp_path("corrupt");
        std::fs::write(&path, b"{not json").unwrap();

        let store = KvpStore::open(path.clone());

        assert!(store.is_empty());
        // And the store is still usable afterwards.
        store.set("r", "k", "v".into(), true);
        assert_eq!(
            KvpStore::open(path.clone()).get("r", "k").as_deref(),
            Some("v")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_in_memory_store_never_stays_dirty() {
        let store = KvpStore::in_memory();
        store.set("r", "k", "v".into(), false);
        store.flush();
        assert!(!store.is_dirty());
    }
}
