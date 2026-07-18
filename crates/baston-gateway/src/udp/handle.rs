//! Public handle + command channel for the UDP/ENet task.

use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum UdpError {
    #[error("failed to bind UDP socket on port {port}: {source}")]
    Bind { port: u16, source: std::io::Error },
    #[error("failed to create ENet host: {0}")]
    HostCreate(String),
}

/// Commands other subsystems (native dispatch, client events) send to the
/// UDP task.
///
/// Bounded so a stalled ENet pump can't let queued commands (mostly outbound
/// snapshots) grow without limit. Overflow drops unreliable packets silently
/// and logs reliable/drop commands.
pub(super) const CMD_CAPACITY: usize = 8192;

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
    SetVoice(baston_voice::server::VoiceHandle),
}

/// Cloneable handle to the UDP task.
#[derive(Clone)]
pub struct UdpHandle {
    pub(super) cmd_tx: mpsc::Sender<UdpCommand>,
}

impl UdpHandle {
    /// Handle wired to nothing — sends are dropped. For tests that need a
    /// `StateAggregator` without an ENet host.
    pub fn disconnected() -> (Self, mpsc::Receiver<UdpCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CAPACITY);
        (Self { cmd_tx }, cmd_rx)
    }

    pub fn send_to_source(&self, source: u32, channel: u8, data: Vec<u8>, reliable: bool) {
        match self.cmd_tx.try_send(UdpCommand::SendToSource {
            source,
            channel,
            data,
            reliable,
        }) {
            Ok(()) => {}
            // Unreliable packets are safe to drop under overload; a dropped
            // reliable packet is a real problem, so surface it.
            Err(mpsc::error::TrySendError::Full(_)) => {
                if reliable {
                    tracing::warn!(target: "udp", source, "reliable send dropped: command queue full");
                }
                metrics::counter!("udp_cmd_dropped_total").increment(1);
            }
            // Server task gone (shutdown) — stay silent.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// Attach the voice server so player disconnects tear their voice
    /// session down.
    pub fn set_voice(&self, voice: baston_voice::server::VoiceHandle) {
        if self.cmd_tx.try_send(UdpCommand::SetVoice(voice)).is_err() {
            tracing::warn!(target: "udp", "voice handle not delivered: queue full or closed");
        }
    }

    pub fn drop_source(&self, source: u32) {
        if self
            .cmd_tx
            .try_send(UdpCommand::DropSource { source })
            .is_err()
        {
            tracing::warn!(target: "udp", source, "drop command not delivered: queue full or closed");
        }
    }
}
