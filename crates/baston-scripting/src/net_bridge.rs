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
    /// `TriggerClientEventInternal` — the payload is already msgpack, packed
    /// by the caller. Kept distinct from [`Self::ClientEvent`] so the gateway
    /// does not re-encode bytes that are already in wire form.
    ClientEventRaw {
        source: u32,
        event: String,
        payload: Vec<u8>,
    },
}

/// Bounded so a script emitting client events / native calls in a tight loop
/// applies backpressure (drop + log on overflow) instead of growing the queue
/// without limit and pushing the process toward OOM.
const NET_BRIDGE_CAPACITY: usize = 2048;

/// Cloneable bridge handed to every runtime and to the gateway.
#[derive(Clone)]
pub struct NetBridge {
    pub tx: mpsc::Sender<NetOutbound>,
    pub pending_natives: Arc<PendingNatives>,
}

impl NetBridge {
    /// Create a bridge plus the receiving end the gateway drains.
    pub fn new() -> (Self, mpsc::Receiver<NetOutbound>) {
        let (tx, rx) = mpsc::channel(NET_BRIDGE_CAPACITY);
        (
            Self {
                tx,
                pending_natives: Arc::new(PendingNatives::new()),
            },
            rx,
        )
    }
}
