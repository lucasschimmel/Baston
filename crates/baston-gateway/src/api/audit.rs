//! Append-only JSONL audit log for control-plane actions.
//!
//! Every control action (kick, resource start/stop/restart, zone drain) is
//! recorded with the key name that performed it — including denied attempts.
//! Writes go through an unbounded channel to a dedicated writer task so the
//! request path never blocks on disk IO; on writer failure the records are
//! surfaced via `tracing::error!` rather than silently dropped.

use std::path::PathBuf;

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

#[derive(Debug, Serialize)]
pub struct AuditRecord {
    /// Unix epoch milliseconds (no chrono dependency; unambiguous, greppable).
    pub ts_ms: u64,
    /// Key name from the ring (`"admin"` for the legacy token).
    pub key: String,
    /// e.g. `"player.kick"`, `"resource.start"`, `"zone.drain"`.
    pub action: String,
    /// e.g. `"source:42"`, `"resource:carpack"`, `"zone:zone_a"`.
    pub target: String,
    /// `"ok"`, `"denied"`, or an error summary.
    pub outcome: String,
}

#[derive(Clone)]
pub struct AuditLog {
    tx: Option<mpsc::UnboundedSender<AuditRecord>>,
}

impl AuditLog {
    /// No-op logger (tests, or audit disabled by an empty path).
    pub fn disabled() -> Self {
        Self { tx: None }
    }

    /// Spawn the writer task, appending JSONL to `path`.
    pub fn spawn(path: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<AuditRecord>();
        tokio::spawn(async move {
            let mut file = match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!(target: "api", path = %path.display(), error = %e,
                        "audit log unavailable — control actions will only appear in tracing output");
                    // Drain and surface, so records are never silently lost.
                    while let Some(r) = rx.recv().await {
                        tracing::error!(target: "api", record = ?r, "audit (log file unavailable)");
                    }
                    return;
                }
            };
            while let Some(record) = rx.recv().await {
                let mut line = match serde_json::to_vec(&record) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!(target: "api", error = %e, "audit serialize failed");
                        continue;
                    }
                };
                line.push(b'\n');
                if let Err(e) = file.write_all(&line).await {
                    tracing::error!(target: "api", error = %e, record = ?record, "audit write failed");
                }
            }
        });
        Self { tx: Some(tx) }
    }

    pub fn record(&self, key: &str, action: &str, target: &str, outcome: &str) {
        metrics::counter!(
            "baston_api_audit_total",
            "action" => action.to_owned(),
            "outcome" => if outcome == "ok" { "ok" } else { "error" },
        )
        .increment(1);
        tracing::info!(target: "api", key, action, target, outcome, "audit");
        if let Some(tx) = &self.tx {
            let record = AuditRecord {
                ts_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                key: key.to_owned(),
                action: action.to_owned(),
                target: target.to_owned(),
                outcome: outcome.to_owned(),
            };
            // Unbounded send only fails when the writer died; already logged.
            let _ = tx.send(record);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_are_appended_as_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::spawn(path.clone());
        log.record("panel", "player.kick", "source:7", "ok");
        log.record("panel", "resource.stop", "resource:carpack", "denied");

        // Writer is async; poll briefly for both lines.
        let mut content = String::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
            if content.lines().count() == 2 {
                break;
            }
        }
        let lines: Vec<serde_json::Value> = content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["key"], "panel");
        assert_eq!(lines[0]["action"], "player.kick");
        assert_eq!(lines[0]["target"], "source:7");
        assert_eq!(lines[0]["outcome"], "ok");
        assert!(lines[0]["ts_ms"].as_u64().unwrap() > 0);
        assert_eq!(lines[1]["outcome"], "denied");
    }

    #[tokio::test]
    async fn disabled_log_is_a_no_op() {
        AuditLog::disabled().record("k", "a", "t", "ok");
    }
}
