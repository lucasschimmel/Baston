//! Bridge between script runtimes and the game transport.
//!
//! Runtimes emit outbound traffic (client events, native calls) through an
//! mpsc channel; the gateway forwards it over ENet and resolves pending
//! native calls when `__baston:nativeResult` comes back.

use std::sync::Arc;

use baston_protocol::native::PendingNatives;
use tokio::sync::mpsc;

/// Outbound message from a script runtime to the game transport.
#[derive(Debug)]
pub enum NetOutbound {
    /// `TriggerClientEvent(name, source, ...args)` — args as JSON array text.
    ClientEvent {
        source: u32,
        event: String,
        args_json: String,
    },
}

/// Cloneable bridge handed to every runtime and to the gateway.
#[derive(Clone)]
pub struct NetBridge {
    pub tx: mpsc::UnboundedSender<NetOutbound>,
    pub pending_natives: Arc<PendingNatives>,
}

impl NetBridge {
    /// Create a bridge plus the receiving end the gateway drains.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<NetOutbound>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                pending_natives: Arc::new(PendingNatives::new()),
            },
            rx,
        )
    }
}
