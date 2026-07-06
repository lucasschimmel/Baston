//! Per-resource `stream/` asset cache: recursive scan, RSC header parse,
//! SHA1 hash, keyed by basename.
//!
//! Mirrors FXServer's ResourceStreamComponent: every file under
//! `<resource>/stream/` (any depth) becomes a streaming entry advertised in
//! `getConfiguration.streamFiles` and downloadable at
//! `/files/<resource>/<basename>` (FilesHttpHandler.cpp falls back to the
//! streaming list when the name is not a regular file). Invalidation uses the
//! same mtime+size fingerprint as `PackfileCache`, which also covers hot
//! reload.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use baston_protocol::connection::StreamFileEntry;
use baston_zone::ResourceManager;
use dashmap::DashMap;
use sha1::{Digest, Sha1};
use tokio::io::AsyncReadExt;

/// Source-file fingerprint: (path, mtime nanos since epoch, size).
type Fingerprint = Vec<(PathBuf, u128, u64)>;

/// RSC container magics, little-endian u32 of the first four bytes
/// (ResourceStreamComponent.cpp AddStreamingFile).
const RSC7_MAGIC: u32 = 0x3743_5352;
const RSC5_MAGIC: u32 = 0x0543_5352;
const RSC8_MAGIC: u32 = 0x3843_5352;

/// Companion suffix: `X.stream_raw` holds the RSC header for `X` and is never
/// an entry of its own.
const STREAM_RAW_SUFFIX: &str = ".stream_raw";

/// One scanned streaming asset: the wire entry plus where it lives on disk
/// (needed by the download route, which only receives the basename).
#[derive(Debug, Clone)]
pub struct StreamAsset {
    pub entry: StreamFileEntry,
    pub on_disk: PathBuf,
}

/// All streaming assets of one resource, keyed by basename.
#[derive(Debug, Default)]
pub struct CachedStreamSet {
    pub assets: BTreeMap<String, StreamAsset>,
}

#[derive(Default)]
pub struct StreamCache {
    entries: DashMap<String, (Fingerprint, Arc<CachedStreamSet>)>,
}

/// Collect every regular file under `dir`, any depth, sorted for a stable
/// fingerprint. Missing/unreadable dirs yield an empty list.
async fn walk_stream_dir(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(current) = stack.pop() {
        let Ok(mut rd) = tokio::fs::read_dir(&current).await else {
            continue;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            match entry.file_type().await {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => files.push(path),
                _ => {}
            }
        }
    }
    files.sort();
    files
}

async fn fingerprint(files: &[PathBuf]) -> Fingerprint {
    let mut fp = Fingerprint::with_capacity(files.len());
    for path in files {
        if let Ok(meta) = tokio::fs::metadata(path).await {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            fp.push((path.clone(), mtime, meta.len()));
        } else {
            fp.push((path.clone(), 0, 0));
        }
    }
    fp
}

/// Parse an RSC container header (first 16 bytes). Returns
/// `(version, pages_virtual, pages_physical)` for RSC5/RSC7/RSC8, `None` for
/// raw assets. The RSC5 physical-pages-from-version quirk is faithful to
/// ResourceStreamComponent.cpp.
fn parse_rsc_header(head: &[u8]) -> Option<(u32, u32, u32)> {
    if head.len() < 16 {
        return None;
    }
    let word = |i: usize| u32::from_le_bytes(head[i..i + 4].try_into().unwrap());
    let (magic, version, virt, phys) = (word(0), word(4), word(8), word(12));
    match magic {
        RSC7_MAGIC | RSC8_MAGIC => Some((version, virt, phys)),
        RSC5_MAGIC => Some((version, virt, version)),
        _ => None,
    }
}

/// SHA1 of the file content, streamed in 8 KiB chunks like FXServer.
async fn sha1_file(path: &Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Scan one file into a streaming asset. `None` when it can't be an entry
/// (companion file, unreadable, oversized).
async fn scan_file(path: &Path) -> Option<(String, StreamAsset)> {
    let basename = path.file_name()?.to_str()?.to_owned();
    if basename.contains(STREAM_RAW_SUFFIX) {
        return None;
    }

    let meta = tokio::fs::metadata(path).await.ok()?;
    let Ok(size) = u32::try_from(meta.len()) else {
        tracing::warn!(target: "gateway", file = %path.display(), "stream file exceeds 4 GiB; skipped");
        return None;
    };

    // RSC header comes from the `.stream_raw` companion when present
    // (raw page data shipped separately), from the file itself otherwise.
    let raw_companion = path.with_file_name(format!("{basename}{STREAM_RAW_SUFFIX}"));
    let header_path = if tokio::fs::metadata(&raw_companion).await.is_ok() {
        raw_companion
    } else {
        path.to_owned()
    };
    let mut head = [0u8; 16];
    let header = match tokio::fs::File::open(&header_path).await {
        Ok(mut f) => {
            let mut filled = 0;
            while filled < head.len() {
                match f.read(&mut head[filled..]).await {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(_) => break,
                }
            }
            parse_rsc_header(&head[..filled])
        }
        Err(_) => None,
    };

    let hash = match sha1_file(path).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(target: "gateway", file = %path.display(), error = %e, "stream file unreadable; skipped");
            return None;
        }
    };

    let entry = match header {
        Some((version, virt, phys)) => StreamFileEntry {
            hash,
            rsc_flags: size,
            rsc_version: version,
            size,
            rsc_pages_virtual: Some(virt),
            rsc_pages_physical: Some(phys),
            encrypted: Some(false),
        },
        None => StreamFileEntry {
            hash,
            rsc_flags: size,
            rsc_version: 0,
            size,
            rsc_pages_virtual: None,
            rsc_pages_physical: None,
            encrypted: None,
        },
    };
    Some((
        basename,
        StreamAsset {
            entry,
            on_disk: path.to_owned(),
        },
    ))
}

impl StreamCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (scanning or rescanning if stale) the stream set for a started
    /// resource. `None` when the resource is unknown; an empty set when it
    /// has no `stream/` folder.
    pub async fn get(
        &self,
        resource_manager: &ResourceManager,
        resource: &str,
    ) -> Option<Arc<CachedStreamSet>> {
        let (root, _manifest) = resource_manager.started_resource(resource).await?;
        let stream_dir = root.join("stream");

        let files = walk_stream_dir(&stream_dir).await;
        let fp = fingerprint(&files).await;
        if let Some(entry) = self.entries.get(resource) {
            if entry.0 == fp {
                return Some(Arc::clone(&entry.1));
            }
        }

        let mut assets = BTreeMap::new();
        for path in &files {
            if let Some((basename, asset)) = scan_file(path).await {
                if let Some(prev) = assets.insert(basename.clone(), asset) {
                    tracing::warn!(
                        target: "gateway",
                        resource,
                        basename,
                        shadowed = %prev.on_disk.display(),
                        "duplicate stream basename; later path wins"
                    );
                }
            }
        }

        if !assets.is_empty() {
            tracing::info!(
                target: "gateway",
                resource,
                count = assets.len(),
                "stream assets scanned"
            );
        }
        let cached = Arc::new(CachedStreamSet { assets });
        self.entries
            .insert(resource.to_owned(), (fp, Arc::clone(&cached)));
        Some(cached)
    }

    /// Resolve a basename to its on-disk path, for the download route.
    pub async fn resolve(
        &self,
        resource_manager: &ResourceManager,
        resource: &str,
        basename: &str,
    ) -> Option<PathBuf> {
        let set = self.get(resource_manager, resource).await?;
        set.assets.get(basename).map(|a| a.on_disk.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal RSC7 container: magic, version 2, virtPages 0x11, physPages
    /// 0x22, then arbitrary payload.
    fn rsc7_bytes() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&RSC7_MAGIC.to_le_bytes());
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&0x11u32.to_le_bytes());
        b.extend_from_slice(&0x22u32.to_le_bytes());
        b.extend_from_slice(b"payload");
        b
    }

    #[test]
    fn rsc_header_variants() {
        let mut rsc5 = rsc7_bytes();
        rsc5[..4].copy_from_slice(&RSC5_MAGIC.to_le_bytes());
        // RSC5 quirk: physical pages mirror the version field.
        assert_eq!(parse_rsc_header(&rsc5), Some((2, 0x11, 2)));
        assert_eq!(parse_rsc_header(&rsc7_bytes()), Some((2, 0x11, 0x22)));
        let mut rsc8 = rsc7_bytes();
        rsc8[..4].copy_from_slice(&RSC8_MAGIC.to_le_bytes());
        assert_eq!(parse_rsc_header(&rsc8), Some((2, 0x11, 0x22)));
        assert_eq!(parse_rsc_header(b"plain text file "), None);
        assert_eq!(parse_rsc_header(b"short"), None);
    }

    #[tokio::test]
    async fn scan_rsc7_and_raw_files() {
        let dir = tempfile::tempdir().unwrap();
        let rsc = dir.path().join("model.yft");
        tokio::fs::write(&rsc, rsc7_bytes()).await.unwrap();
        let raw = dir.path().join("readme.txt");
        tokio::fs::write(&raw, b"not an rsc").await.unwrap();

        let (name, asset) = scan_file(&rsc).await.unwrap();
        assert_eq!(name, "model.yft");
        let e = &asset.entry;
        assert_eq!(e.size, rsc7_bytes().len() as u32);
        assert_eq!(e.rsc_flags, e.size);
        assert_eq!(e.rsc_version, 2);
        assert_eq!(e.rsc_pages_virtual, Some(0x11));
        assert_eq!(e.rsc_pages_physical, Some(0x22));
        assert_eq!(e.encrypted, Some(false));
        assert_eq!(e.hash.len(), 40);

        let (_, asset) = scan_file(&raw).await.unwrap();
        let e = &asset.entry;
        assert_eq!(e.rsc_version, 0);
        assert_eq!(e.rsc_flags, e.size);
        assert_eq!(e.rsc_pages_virtual, None);
        assert_eq!(e.encrypted, None);
    }

    #[tokio::test]
    async fn stream_raw_companion_supplies_header_and_is_not_an_entry() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("big.ydr");
        tokio::fs::write(&main, b"raw page data without header")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("big.ydr.stream_raw"), rsc7_bytes())
            .await
            .unwrap();

        // Companion is skipped as an entry of its own.
        assert!(scan_file(&dir.path().join("big.ydr.stream_raw"))
            .await
            .is_none());

        // Main file takes its RSC metadata from the companion but hashes and
        // sizes its own content.
        let (_, asset) = scan_file(&main).await.unwrap();
        let e = &asset.entry;
        assert_eq!(e.rsc_version, 2);
        assert_eq!(e.rsc_pages_virtual, Some(0x11));
        assert_eq!(e.size, 28);
        let expected = hex::encode(Sha1::digest(b"raw page data without header"));
        assert_eq!(e.hash, expected);
    }

    #[tokio::test]
    async fn walk_is_recursive_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join("sub/deep"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.ytd"), b"b")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("sub/deep/a.yft"), b"a")
            .await
            .unwrap();

        let files = walk_stream_dir(dir.path()).await;
        assert_eq!(files.len(), 2);
        assert!(files.windows(2).all(|w| w[0] <= w[1]));

        // Missing dir → empty, no error.
        assert!(walk_stream_dir(&dir.path().join("nope")).await.is_empty());
    }
}
