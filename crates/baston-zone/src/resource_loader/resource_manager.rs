//! `ResourceManager` — discovery, dependency-ordered startup, and the
//! Unloaded → Loading → Started → Stopped lifecycle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use baston_scripting::{ScriptHost, ScriptSource};
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
}

impl ResourceManager {
    pub fn new(script_host: ScriptHost, resources_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            script_host,
            resources: Mutex::new(HashMap::new()),
            resources_dir,
        })
    }

    pub fn resources_dir(&self) -> &Path {
        &self.resources_dir
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
        let mut scripts = Vec::with_capacity(script_paths.len());
        for rel in script_paths {
            let path = root.join(rel);
            let code =
                tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|source| ZoneError::ScriptRead {
                        path: path.clone(),
                        source,
                    })?;
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

    /// Map an absolute file path back to the resource that owns it (used by
    /// the hot-reload watcher).
    pub async fn resource_for_path(&self, path: &Path) -> Option<String> {
        // Watcher events carry absolute paths while roots may be relative
        // (and Windows canonicalize adds a \\?\ prefix) — canonicalize both
        // sides so the prefix comparison is meaningful.
        let path = std::fs::canonicalize(path).ok()?;
        let resources = self.resources.lock().await;
        resources
            .iter()
            .find(|(_, e)| {
                std::fs::canonicalize(&e.discovered.root).is_ok_and(|root| path.starts_with(root))
            })
            .map(|(n, _)| n.clone())
    }
}
