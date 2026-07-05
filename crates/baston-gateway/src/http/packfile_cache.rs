//! Per-resource `resource.rpf` packfile cache with SHA1 hashes.
//!
//! FXServer builds one RPF2 archive per resource containing its client-side
//! files, and `getConfiguration` advertises `{ "resource.rpf": sha1 }`
//! (`ResourceFilesComponent.cpp`). BASTON builds the archive in memory and
//! invalidates by fingerprinting the source files (mtime + size), which also
//! covers hot reload.

use std::path::PathBuf;
use std::sync::Arc;

use baston_protocol::ResourceManifest;
use baston_zone::packfile::{build_rpf, generate_fxmanifest, PackfileInput};
use baston_zone::ResourceManager;
use dashmap::DashMap;
use sha1::{Digest, Sha1};

/// Source-file fingerprint: (path, mtime nanos since epoch, size).
type Fingerprint = Vec<(PathBuf, u128, u64)>;

pub struct CachedPackfile {
    pub bytes: Arc<Vec<u8>>,
    pub sha1_hex: String,
}

#[derive(Default)]
pub struct PackfileCache {
    entries: DashMap<String, (Fingerprint, Arc<CachedPackfile>)>,
}

/// The client-visible files of a resource: manifest-declared `files` +
/// `client_scripts`, deduplicated, order-stable.
fn client_file_list(manifest: &ResourceManifest) -> Vec<String> {
    let mut list: Vec<String> = manifest
        .client_scripts
        .iter()
        .chain(manifest.files.iter())
        .cloned()
        .collect();
    list.sort();
    list.dedup();
    list
}

async fn fingerprint(root: &std::path::Path, files: &[String]) -> Fingerprint {
    let mut fp = Fingerprint::with_capacity(files.len());
    for rel in files {
        let path = root.join(rel);
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            fp.push((path, mtime, meta.len()));
        } else {
            fp.push((path, 0, 0));
        }
    }
    fp
}

impl PackfileCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (building or rebuilding if stale) the packfile for a started
    /// resource. `None` when the resource is unknown, or when it has no
    /// client files and `manifest_only_fallback` is false. With the fallback
    /// on, a resource with no client files still gets an RPF containing just
    /// the generated fxmanifest.lua — needed so stream-only resources (car
    /// packs, clothing) can be mounted by the client.
    pub async fn get(
        &self,
        resource_manager: &ResourceManager,
        resource: &str,
        manifest_only_fallback: bool,
    ) -> Option<Arc<CachedPackfile>> {
        let (root, manifest) = resource_manager.started_resource(resource).await?;
        let files = client_file_list(&manifest);
        if files.is_empty() && !manifest_only_fallback {
            return None;
        }

        let fp = fingerprint(&root, &files).await;
        if let Some(entry) = self.entries.get(resource) {
            if entry.0 == fp {
                return Some(Arc::clone(&entry.1));
            }
        }

        // (Re)build: generated fxmanifest.lua + every client file.
        let mut inputs = vec![PackfileInput {
            path: "fxmanifest.lua".to_owned(),
            data: generate_fxmanifest(&manifest).into_bytes(),
        }];
        for rel in &files {
            let path = root.join(rel);
            match tokio::fs::read(&path).await {
                Ok(data) => inputs.push(PackfileInput {
                    path: rel.clone(),
                    data,
                }),
                Err(e) => {
                    tracing::warn!(target: "gateway", resource, file = %path.display(), error = %e, "client file unreadable; skipped from packfile");
                }
            }
        }

        let bytes = build_rpf(inputs);
        let sha1_hex = hex::encode(Sha1::digest(&bytes));
        let cached = Arc::new(CachedPackfile {
            bytes: Arc::new(bytes),
            sha1_hex,
        });
        tracing::info!(
            target: "gateway",
            resource,
            size = cached.bytes.len(),
            sha1 = %cached.sha1_hex,
            "resource packfile built"
        );
        self.entries
            .insert(resource.to_owned(), (fp, Arc::clone(&cached)));
        Some(cached)
    }
}
