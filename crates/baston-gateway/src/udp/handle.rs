//! Public handle + command channel for the UDP/ENet task.

use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum UdpError {
    #[error("failed to bind UDP socket on port {port}: {source}")]
    Bind { port: u16, source: std::io::Error },
    #[error("failed to create ENet host: {0}")]
    HostCreate(String),
}

/// Reserved capacity for reliable control traffic. This queue is deliberately
/// independent from snapshots: saturating state sync must not evict a drop,
/// event, ACK, or native response before ENet can provide reliability.
pub(super) const CONTROL_CAPACITY: usize = 2048;

/// Capacity for supersedable state traffic. Overflow is expected to shed stale
/// frames; a newer interest tick will replace them.
pub(super) const SYNC_CAPACITY: usize = 8192;

#[derive(Debug)]
pub enum UdpCommand {
    /// Send a raw message packet to a connected player.
    SendToSource {
        source: u32,
        channel: u8,
        data: Vec<u8>,
        reliable: bool,
    },
    /// Forcefully drop a player's game connection.
    DropSource { source: u32 },
    /// Wire the embedded voice server (per-player teardown on disconnect).
    /// `advertise` is the `(address, port)` replicated to clients as
    /// `voice_externalAddress`/`voice_externalPort` so their embedded Mumble
    /// connects to us instead of probing the game port.
    SetVoice {
        handle: baston_voice::server::VoiceHandle,
        advertise: Option<(String, u16)>,
    },
    /// Wire the queue scripts submit entity creations and deletions on. Sent
    /// once at startup, after the script host exists.
    SetWorldCommands {
        rx: mpsc::Receiver<baston_scripting::WorldCommand>,
    },
}

/// Cloneable, reliable control-plane handle.
#[derive(Clone)]
pub struct ControlPlaneHandle {
    pub(super) tx: mpsc::Sender<UdpCommand>,
}

impl ControlPlaneHandle {
    pub fn send(&self, source: u32, channel: u8, data: Vec<u8>) {
        self.try_send(
            UdpCommand::SendToSource {
                source,
                channel,
                data,
                reliable: true,
            },
            Some(source),
        );
    }

    fn try_send(&self, command: UdpCommand, source: Option<u32>) {
        match self.tx.try_send(command) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::error!(
                    target: "udp",
                    source,
                    "control-plane command rejected: reserved queue full"
                );
                metrics::counter!(
                    "udp_plane_dropped_total",
                    "plane" => "control",
                    "reliable" => "true"
                )
                .increment(1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
        metrics::gauge!("udp_plane_queue_depth", "plane" => "control")
            .set((self.tx.max_capacity() - self.tx.capacity()) as f64);
    }

    pub fn set_voice(
        &self,
        voice: baston_voice::server::VoiceHandle,
        advertise: Option<(String, u16)>,
    ) {
        self.try_send(
            UdpCommand::SetVoice {
                handle: voice,
                advertise,
            },
            None,
        );
    }

    /// Hand the UDP task the queue of script-issued world mutations.
    pub fn set_world_commands(&self, rx: mpsc::Receiver<baston_scripting::WorldCommand>) {
        self.try_send(UdpCommand::SetWorldCommands { rx }, None);
    }

    pub fn drop_source(&self, source: u32) {
        self.try_send(UdpCommand::DropSource { source }, Some(source));
    }
}

/// Cloneable, unreliable data-plane handle.
#[derive(Clone)]
pub struct SyncPlaneHandle {
    pub(super) tx: mpsc::Sender<UdpCommand>,
}

impl SyncPlaneHandle {
    pub fn send(&self, source: u32, channel: u8, data: Vec<u8>) {
        match self.tx.try_send(UdpCommand::SendToSource {
            source,
            channel,
            data,
            reliable: false,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                metrics::counter!(
                    "udp_plane_dropped_total",
                    "plane" => "sync",
                    "reliable" => "false"
                )
                .increment(1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
        metrics::gauge!("udp_plane_queue_depth", "plane" => "sync")
            .set((self.tx.max_capacity() - self.tx.capacity()) as f64);
    }

    pub fn pressure(&self) -> f64 {
        let maximum = self.tx.max_capacity();
        if maximum == 0 {
            return 0.0;
        }
        (maximum - self.tx.capacity()) as f64 / maximum as f64
    }
}

/// Explicit transport planes for the single-owner ENet task.
#[derive(Clone)]
pub struct UdpHandle {
    control: ControlPlaneHandle,
    sync: SyncPlaneHandle,
}

impl UdpHandle {
    pub(super) fn new(
        control_tx: mpsc::Sender<UdpCommand>,
        sync_tx: mpsc::Sender<UdpCommand>,
    ) -> Self {
        Self {
            control: ControlPlaneHandle { tx: control_tx },
            sync: SyncPlaneHandle { tx: sync_tx },
        }
    }

    /// A single observable receiver is retained for unit tests. Production
    /// construction always supplies distinct queues through [`Self::new`].
    pub fn disconnected() -> (Self, mpsc::Receiver<UdpCommand>) {
        let (tx, rx) = mpsc::channel(SYNC_CAPACITY);
        (Self::new(tx.clone(), tx), rx)
    }

    pub fn control(&self) -> &ControlPlaneHandle {
        &self.control
    }

    pub fn sync(&self) -> &SyncPlaneHandle {
        &self.sync
    }

    /// Compatibility entry point for existing producers. New call sites
    /// should select `control()` or `sync()` explicitly.
    pub fn send_to_source(&self, source: u32, channel: u8, data: Vec<u8>, reliable: bool) {
        if reliable {
            self.control.send(source, channel, data);
        } else {
            self.sync.send(source, channel, data);
        }
    }

    pub fn set_voice(
        &self,
        voice: baston_voice::server::VoiceHandle,
        advertise: Option<(String, u16)>,
    ) {
        self.control.set_voice(voice, advertise);
    }

    /// Hand the UDP task the queue of script-issued world mutations.
    pub fn set_world_commands(&self, rx: mpsc::Receiver<baston_scripting::WorldCommand>) {
        self.control.set_world_commands(rx);
    }

    pub fn drop_source(&self, source: u32) {
        self.control.drop_source(source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn saturated_sync_plane_does_not_consume_control_capacity() {
        let (control_tx, mut control_rx) = mpsc::channel(1);
        let (sync_tx, _sync_rx) = mpsc::channel(1);
        let handle = UdpHandle::new(control_tx, sync_tx);

        handle.sync().send(1, 1, vec![1]);
        handle.sync().send(1, 1, vec![2]); // shed by the full sync queue
        handle.control().drop_source(7);

        assert!(matches!(
            control_rx.recv().await,
            Some(UdpCommand::DropSource { source: 7 })
        ));
    }

    #[tokio::test]
    async fn compatibility_send_selects_the_expected_plane() {
        let (control_tx, mut control_rx) = mpsc::channel(1);
        let (sync_tx, mut sync_rx) = mpsc::channel(1);
        let handle = UdpHandle::new(control_tx, sync_tx);

        handle.send_to_source(3, 0, vec![1], true);
        handle.send_to_source(3, 1, vec![2], false);

        assert!(matches!(
            control_rx.recv().await,
            Some(UdpCommand::SendToSource { reliable: true, .. })
        ));
        assert!(matches!(
            sync_rx.recv().await,
            Some(UdpCommand::SendToSource {
                reliable: false,
                ..
            })
        ));
    }
}
