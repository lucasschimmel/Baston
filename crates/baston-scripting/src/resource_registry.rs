use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use baston_protocol::ResourceManifest;

/// FXServer-style resource state strings exposed to scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptResourceState {
    Uninitialized,
    Starting,
    Started,
    Stopped,
    Missing,
    Unknown,
}

impl ScriptResourceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uninitialized => "uninitialized",
            Self::Starting => "starting",
            Self::Started => "started",
            Self::Stopped => "stopped",
            Self::Missing => "missing",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
struct ResourceRecord {
    root: PathBuf,
    manifest: ResourceManifest,
    state: ScriptResourceState,
}

#[derive(Debug, Default)]
struct Inner {
    resources: HashMap<String, ResourceRecord>,
}

/// Synchronous snapshot used by deno ops for resource natives.
#[derive(Debug, Clone, Default)]
pub struct ResourceRegistry {
    inner: Arc<RwLock<Inner>>,
}

impl ResourceRegistry {
    pub fn upsert_resource(
        &self,
        name: String,
        root: PathBuf,
        manifest: ResourceManifest,
        state: ScriptResourceState,
    ) {
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resources
            .insert(
                name,
                ResourceRecord {
                    root,
                    manifest,
                    state,
                },
            );
    }

    pub fn set_state(&self, name: &str, state: ScriptResourceState) {
        if let Some(record) = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resources
            .get_mut(name)
        {
            record.state = state;
        }
    }

    pub fn count(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resources
            .len()
    }

    /// Every loaded resource name, ascending, so repeated reads give scripts a
    /// stable order.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resources
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    pub fn name_at(&self, index: usize) -> String {
        self.names().get(index).cloned().unwrap_or_default()
    }

    pub fn state(&self, name: &str) -> &'static str {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resources
            .get(name)
            .map(|record| record.state.as_str())
            .unwrap_or(ScriptResourceState::Missing.as_str())
    }

    pub fn path(&self, name: &str) -> String {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resources
            .get(name)
            .map(|record| record.root.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn metadata_count(&self, name: &str, key: &str) -> u32 {
        self.metadata_values(name, key).len() as u32
    }

    pub fn metadata_value(&self, name: &str, key: &str, index: usize) -> String {
        self.metadata_values(name, key)
            .get(index)
            .cloned()
            .unwrap_or_default()
    }

    fn metadata_values(&self, name: &str, key: &str) -> Vec<String> {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(record) = guard.resources.get(name) else {
            return Vec::new();
        };
        match key {
            "version" => record.manifest.version.iter().cloned().collect(),
            "dependency" => record.manifest.dependencies.clone(),
            "server_script" => record.manifest.server_scripts.clone(),
            "client_script" => record.manifest.client_scripts.clone(),
            "file" => record.manifest.files.clone(),
            _ => Vec::new(),
        }
    }

    pub fn load_file(&self, name: &str, file_name: &str) -> Option<String> {
        let path = self.resource_file_path(name, file_name)?;
        std::fs::read_to_string(path).ok()
    }

    pub fn save_file(&self, name: &str, file_name: &str, data: &str, data_len: i32) -> bool {
        let Some(path) = self.resource_file_path(name, file_name) else {
            return false;
        };
        let bytes = if data_len < 0 {
            data.as_bytes()
        } else {
            let len = data_len as usize;
            &data.as_bytes()[..data.len().min(len)]
        };
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return false;
            }
        }
        std::fs::write(path, bytes).is_ok()
    }

    fn resource_file_path(&self, name: &str, file_name: &str) -> Option<PathBuf> {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = guard.resources.get(name)?.root.clone();
        drop(guard);
        let rel = sanitize_relative_path(file_name)?;
        Some(root.join(rel))
    }
}

fn sanitize_relative_path(raw: &str) -> Option<PathBuf> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}
