//! Dev hot reload: watch `resources/**/dist/**/*.js` and restart the owning
//! resource on change.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use super::resource_manager::ResourceManager;
use crate::ZoneError;

/// Ignore duplicate FS events for the same resource within this window
/// (editors typically emit several events per save).
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Start the watcher. Returns the watcher handle — keep it alive for as long
/// as hot reload should stay active.
pub fn spawn_hot_reload(manager: Arc<ResourceManager>) -> Result<RecommendedWatcher, ZoneError> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        match res {
            Ok(event) => {
                let relevant = matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                );
                let js_paths: Vec<PathBuf> = event
                    .paths
                    .iter()
                    .filter(|p| p.extension().is_some_and(|e| e == "js"))
                    .cloned()
                    .collect();
                if relevant && !js_paths.is_empty() {
                    // Receiver dropped means hot reload was shut down — fine.
                    let _ = tx.send(js_paths);
                }
            }
            Err(e) => tracing::warn!(target: "hot-reload", error = %e, "watch error"),
        }
    })
    .map_err(|e| ZoneError::Watcher(e.to_string()))?;

    watcher
        .watch(manager.resources_dir(), RecursiveMode::Recursive)
        .map_err(|e| ZoneError::Watcher(e.to_string()))?;
    tracing::info!(target: "hot-reload", dir = %manager.resources_dir().display(), "hot reload active");

    tokio::spawn(async move {
        let mut last_reload: Option<(String, Instant)> = None;
        while let Some(paths) = rx.recv().await {
            for path in paths {
                let Some(resource) = manager.resource_for_path(&path).await else {
                    continue;
                };
                if let Some((last_name, at)) = &last_reload {
                    if *last_name == resource && at.elapsed() < DEBOUNCE {
                        continue;
                    }
                }
                last_reload = Some((resource.clone(), Instant::now()));
                let started = Instant::now();
                match manager.restart(&resource).await {
                    Ok(()) => {
                        let ms = started.elapsed().as_millis();
                        tracing::info!(target: "hot-reload", "[baston] hot-reloaded {resource} ({ms}ms)");
                        println!("[baston] hot-reloaded {resource} ({ms}ms)");
                    }
                    Err(e) => {
                        tracing::error!(target: "hot-reload", %resource, error = %e, "hot reload failed");
                    }
                }
            }
        }
    });

    Ok(watcher)
}
