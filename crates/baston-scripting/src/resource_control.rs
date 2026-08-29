//! Lifecycle control a script can exercise over other resources.
//!
//! `StartResource`, `StopResource` and `ScanResourceRoot` are how admin panels
//! and `ensure`-style tooling work, but the resource manager lives above the
//! script host: it *calls* the host to load a resource, so a native cannot
//! reach back into it synchronously without re-entering the very isolate the
//! native was invoked from.
//!
//! So the native answers what it can know immediately — does the resource
//! exist, is it running — and queues the actual transition for the manager to
//! perform on its own task. That matches the engine, whose `StartResource`
//! also returns before the resource has finished loading.

use tokio::sync::mpsc;

/// A lifecycle transition a script asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceCommand {
    Start(String),
    Stop(String),
    Restart(String),
    /// Rescan a directory for resources (`ScanResourceRoot`).
    ScanRoot(String),
}

/// Queue depth. A resource looping on `StartResource` hits this instead of
/// growing the queue until the process dies.
const COMMAND_CAPACITY: usize = 256;

/// The write side scripts see.
pub trait ResourceControl: Send + Sync {
    /// Queue a transition. Returns whether it was accepted for execution —
    /// not whether it has happened.
    fn submit(&self, command: ResourceCommand) -> bool;
}

/// No manager wired: every request is refused rather than silently dropped, so
/// a script's `StartResource` returns false instead of appearing to work.
pub struct NoResourceControl;

impl ResourceControl for NoResourceControl {
    fn submit(&self, _command: ResourceCommand) -> bool {
        false
    }
}

/// Channel-backed control, paired with the receiver its owner drains.
pub struct QueuedResourceControl {
    tx: mpsc::Sender<ResourceCommand>,
}

impl QueuedResourceControl {
    #[must_use]
    pub fn new() -> (Self, mpsc::Receiver<ResourceCommand>) {
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        (Self { tx }, rx)
    }
}

impl ResourceControl for QueuedResourceControl {
    fn submit(&self, command: ResourceCommand) -> bool {
        match self.tx.try_send(command) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(
                    target: "resources",
                    error = %e,
                    "resource lifecycle command dropped: the queue is full or the manager is gone"
                );
                metrics::counter!("script_resource_commands_dropped_total").increment(1);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_submitted_command_reaches_the_manager() {
        let (control, mut rx) = QueuedResourceControl::new();
        assert!(control.submit(ResourceCommand::Start("chat".into())));
        assert_eq!(rx.recv().await, Some(ResourceCommand::Start("chat".into())));
    }

    /// Without a manager the native must report failure, not pretend.
    #[test]
    fn the_inert_control_refuses() {
        assert!(!NoResourceControl.submit(ResourceCommand::Stop("chat".into())));
    }

    /// A dead receiver means nobody will ever run the command; saying so is
    /// what lets a script fall back instead of waiting forever.
    #[tokio::test]
    async fn a_dropped_manager_is_reported() {
        let (control, rx) = QueuedResourceControl::new();
        drop(rx);
        assert!(!control.submit(ResourceCommand::Start("chat".into())));
    }
}
