//! `ResourceManager` — discovery, dependency-ordered startup, and the
//! Unloaded → Loading → Started → Stopped lifecycle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use baston_core::script_decryptor::{PlainDecryptor, ScriptDecryptor};
use baston_scripting::{Observability, ScriptHost, ScriptSource};
use tokio::sync::Mutex;

use super::manifest::{discover, DiscoveredResource};
use super::topo::topo_sort;
use crate::ZoneError;

/// Lifecycle state of a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceState {
    Unloaded,
    Loading,
    Started,
    Stopped,
    Error,
}

struct ResourceEntry {
    discovered: DiscoveredResource,
    state: ResourceState,
}

/// Manages every resource under the configured `resources/` directory.
pub struct ResourceManager {
    script_host: ScriptHost,
    resources: Mutex<HashMap<String, ResourceEntry>>,
    resources_dir: PathBuf,
    /// Script decryptor. `PlainDecryptor` by default; the escrow plugin
    /// replaces it via [`ResourceManager::set_script_decryptor`]. Behind a
    /// `RwLock` so the plugin can install itself after construction on the
    /// shared `Arc<Self>`; the guard is always dropped before any `.await`.
    decryptor: RwLock<Arc<dyn ScriptDecryptor>>,
}

impl ResourceManager {
    pub fn new(script_host: ScriptHost, resources_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            script_host,
            resources: Mutex::new(HashMap::new()),
            resources_dir,
            decryptor: RwLock::new(Arc::new(PlainDecryptor)),
        })
    }

    pub fn resources_dir(&self) -> &Path {
        &self.resources_dir
    }

    pub fn observability(&self) -> Arc<Observability> {
        self.script_host.observability()
    }

    /// Install an external script decryptor (called by `baston-escrow-plugin`).
    /// Takes `&self` so it works through the shared `Arc<ResourceManager>`.
    pub fn set_script_decryptor(&self, decryptor: Arc<dyn ScriptDecryptor>) {
        let supports_encrypted = decryptor.supports_encrypted();
        // `RwLock` write poisoning only happens if a reader panicked while
        // holding the lock; the read path just clones an Arc and never panics,
        // so recover the guard rather than propagating.
        *self
            .decryptor
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = decryptor;
        tracing::info!(
            supports_encrypted,
            "script decryptor replaced (escrow plugin active)"
        );
    }

    /// Snapshot the current decryptor (cheap `Arc` clone), releasing the lock
    /// immediately so it is never held across an `.await`.
    fn decryptor(&self) -> Arc<dyn ScriptDecryptor> {
        self.decryptor
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Scan the resources directory and register everything found (state
    /// `Unloaded`). Returns the discovered resource names.
    pub async fn discover(&self) -> Result<Vec<String>, ZoneError> {
        let found = discover(&self.resources_dir).await?;
        let mut resources = self.resources.lock().await;
        let mut names = Vec::with_capacity(found.len());
        for discovered in found {
            let name = discovered.manifest.name.clone();
            tracing::info!(target: "resources", resource = %name, path = %discovered.root.display(), "discovered resource");
            resources
                .entry(name.clone())
                .and_modify(|e| e.discovered = discovered.clone())
                .or_insert(ResourceEntry {
                    discovered,
                    state: ResourceState::Unloaded,
                });
            names.push(name);
        }
        Ok(names)
    }

    /// Start every discovered resource in dependency order.
    pub async fn start_all(&self) -> Result<(), ZoneError> {
        let graph: HashMap<String, Vec<String>> = {
            let resources = self.resources.lock().await;
            resources
                .iter()
                .map(|(name, e)| (name.clone(), e.discovered.manifest.dependencies.clone()))
                .collect()
        };
        for name in topo_sort(&graph)? {
            self.start(&name).await?;
        }
        Ok(())
    }

    /// Start one resource: read its server scripts and load them into a
    /// fresh isolate.
    pub async fn start(&self, name: &str) -> Result<(), ZoneError> {
        let (root, script_paths) = {
            let mut resources = self.resources.lock().await;
            let entry = resources
                .get_mut(name)
                .ok_or_else(|| ZoneError::UnknownResource(name.to_owned()))?;
            if entry.state == ResourceState::Started {
                return Ok(());
            }
            entry.state = ResourceState::Loading;
            (
                entry.discovered.root.clone(),
                entry.discovered.manifest.server_scripts.clone(),
            )
        };

        let result = self.load_scripts(name, &root, &script_paths).await;
        let mut resources = self.resources.lock().await;
        if let Some(entry) = resources.get_mut(name) {
            entry.state = if result.is_ok() {
                ResourceState::Started
            } else {
                ResourceState::Error
            };
        }
        result
    }

    async fn load_scripts(
        &self,
        name: &str,
        root: &Path,
        script_paths: &[String],
    ) -> Result<(), ZoneError> {
        let decryptor = self.decryptor();
        let mut scripts = Vec::with_capacity(script_paths.len());
        for rel in script_paths {
            let path = root.join(rel);
            let raw = tokio::fs::read(&path)
                .await
                .map_err(|source| ZoneError::ScriptRead {
                    path: path.clone(),
                    source,
                })?;

            let encrypted = baston_core::script_decryptor::is_cfx_encrypted(&raw);
            let started = Instant::now();
            let bytes = match decryptor.decrypt(name, rel, &raw, None) {
                Ok(bytes) => {
                    let status = if encrypted { "decrypted" } else { "plain" };
                    metrics::counter!("baston_scripts_loaded_total", "status" => status)
                        .increment(1);
                    if encrypted {
                        metrics::histogram!("baston_decrypt_duration_seconds")
                            .record(started.elapsed().as_secs_f64());
                        tracing::info!(
                            target: "resources",
                            resource = name,
                            file = rel,
                            plain_bytes = bytes.len(),
                            encrypted_bytes = raw.len(),
                            "resource script decrypted OK"
                        );
                    }
                    bytes
                }
                Err(source) => {
                    metrics::counter!("baston_scripts_loaded_total", "status" => "error")
                        .increment(1);
                    return Err(ZoneError::Decrypt {
                        path: path.clone(),
                        source,
                    });
                }
            };

            let code = String::from_utf8(bytes)
                .map_err(|_| ZoneError::ScriptNotUtf8 { path: path.clone() })?;
            scripts.push(ScriptSource {
                path: rel.clone(),
                code,
            });
        }
        self.script_host.load_resource(name, scripts).await?;
        Ok(())
    }

    /// Stop a started resource (fires `onResourceStop`, destroys the isolate).
    pub async fn stop(&self, name: &str) -> Result<(), ZoneError> {
        {
            let resources = self.resources.lock().await;
            let entry = resources
                .get(name)
                .ok_or_else(|| ZoneError::UnknownResource(name.to_owned()))?;
            if entry.state != ResourceState::Started {
                return Ok(());
            }
        }
        self.script_host.unload_resource(name).await?;
        let mut resources = self.resources.lock().await;
        if let Some(entry) = resources.get_mut(name) {
            entry.state = ResourceState::Stopped;
        }
        Ok(())
    }

    pub async fn execute_command(
        &self,
        command: &str,
        source: u32,
        args: Vec<String>,
        raw: String,
    ) -> Result<(), baston_scripting::ScriptError> {
        self.script_host
            .execute_command(command, source, args, raw)
            .await
    }

    /// Stop (if started) then start — idempotent "make it run".
    pub async fn ensure(&self, name: &str) -> Result<(), ZoneError> {
        self.stop(name).await?;
        self.start(name).await
    }

    /// Restart = ensure, re-reading manifest and scripts from disk.
    pub async fn restart(&self, name: &str) -> Result<(), ZoneError> {
        // Re-parse the manifest so script list changes are picked up too.
        let root = {
            let resources = self.resources.lock().await;
            resources
                .get(name)
                .ok_or_else(|| ZoneError::UnknownResource(name.to_owned()))?
                .discovered
                .root
                .clone()
        };
        if let Ok(manifest) = super::manifest::parse_manifest(&root).await {
            let mut resources = self.resources.lock().await;
            if let Some(entry) = resources.get_mut(name) {
                entry.discovered.manifest = manifest;
            }
        }
        self.ensure(name).await
    }

    /// Current state of every known resource.
    pub async fn status(&self) -> Vec<(String, ResourceState)> {
        let resources = self.resources.lock().await;
        let mut out: Vec<_> = resources
            .iter()
            .map(|(n, e)| (n.clone(), e.state))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Names of started resources (for `/info.json`).
    pub async fn started_names(&self) -> Vec<String> {
        self.status()
            .await
            .into_iter()
            .filter(|(_, s)| *s == ResourceState::Started)
            .map(|(n, _)| n)
            .collect()
    }

    /// Root directory + parsed manifest of a started resource (used by the
    /// gateway to build the client packfile and `getConfiguration`).
    pub async fn started_resource(
        &self,
        name: &str,
    ) -> Option<(PathBuf, baston_protocol::ResourceManifest)> {
        let resources = self.resources.lock().await;
        resources
            .get(name)
            .filter(|e| e.state == ResourceState::Started)
            .map(|e| (e.discovered.root.clone(), e.discovered.manifest.clone()))
    }

    /// Map an absolute file path back to the resource that owns it (used by
    /// the hot-reload watcher).
    pub async fn resource_for_path(&self, path: &Path) -> Option<String> {
        // Watcher events carry absolute paths while roots may be relative
        // (and Windows canonicalize adds a \\?\ prefix) — canonicalize both
        // sides so the prefix comparison is meaningful.
        let path = std::fs::canonicalize(path).ok()?;
        // Snapshot (name, root) under the lock, then run the blocking
        // `canonicalize` off-lock so a slow filesystem stat can't stall every
        // other `resources` access (start/stop/status) behind this one call.
        let roots: Vec<(String, PathBuf)> = {
            let resources = self.resources.lock().await;
            resources
                .iter()
                .map(|(n, e)| (n.clone(), e.discovered.root.clone()))
                .collect()
        };
        roots.into_iter().find_map(|(name, root)| {
            let root = std::fs::canonicalize(&root).ok()?;
            path.starts_with(&root).then_some(name)
        })
    }
}
