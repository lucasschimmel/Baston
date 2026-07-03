//! `manifest.json` discovery and parsing.

use std::path::{Path, PathBuf};

use baston_protocol::ResourceManifest;

use crate::ZoneError;

/// A resource found on disk: its parsed manifest plus its root directory.
#[derive(Debug, Clone)]
pub struct DiscoveredResource {
    pub manifest: ResourceManifest,
    pub root: PathBuf,
}

/// Parse a single resource directory's `manifest.json`.
pub async fn parse_manifest(resource_root: &Path) -> Result<ResourceManifest, ZoneError> {
    let manifest_path = resource_root.join("manifest.json");
    let raw = tokio::fs::read_to_string(&manifest_path)
        .await
        .map_err(|source| ZoneError::ManifestRead {
            path: manifest_path.clone(),
            source,
        })?;
    let manifest: ResourceManifest =
        serde_json::from_str(&raw).map_err(|source| ZoneError::ManifestParse {
            path: manifest_path,
            source,
        })?;
    Ok(manifest)
}

/// Scan `resources_dir` for subdirectories containing a valid `manifest.json`.
/// Invalid manifests are logged and skipped (one bad resource must not take
/// the server down).
pub async fn discover(resources_dir: &Path) -> Result<Vec<DiscoveredResource>, ZoneError> {
    let mut entries =
        tokio::fs::read_dir(resources_dir)
            .await
            .map_err(|source| ZoneError::ManifestRead {
                path: resources_dir.to_owned(),
                source,
            })?;

    let mut found = Vec::new();
    while let Some(entry) =
        entries
            .next_entry()
            .await
            .map_err(|source| ZoneError::ManifestRead {
                path: resources_dir.to_owned(),
                source,
            })?
    {
        let root = entry.path();
        if !root.is_dir() || !root.join("manifest.json").is_file() {
            continue;
        }
        match parse_manifest(&root).await {
            Ok(manifest) => found.push(DiscoveredResource { manifest, root }),
            Err(e) => {
                tracing::warn!(target: "resources", path = %root.display(), error = %e, "skipping resource with invalid manifest");
            }
        }
    }
    Ok(found)
}
