//! Bridge between script runtimes and the game transport.
//!
//! Runtimes emit outbound traffic (client events, native calls) through an
//! mpsc channel; the gateway forwards it over ENet and resolves pending
//! native calls when `__baston:nativeResult` comes back.

use std::sync::Arc;

use baston_protocol::native::PendingNatives;
use tokio::sync::mpsc;

/// Who a server → client event is for.
///
/// FiveM spells "everyone" as a source of `-1`, which scripts pass constantly:
/// `TriggerClientEvent('chat:addMessage', -1, ...)`. Carrying that as a number
/// all the way down would mean every layer remembering that one value is not a
/// player id — so it stops being a number here, at the first boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTarget {
    /// Every connected client.
    All,
    /// One player, by source id.
    One(u32),
}

impl EventTarget {
    /// Read the value a script passed. Negative means everyone, as in FiveM.
    pub fn from_script(raw: i64) -> Self {
        match u32::try_from(raw) {
            Ok(source) => Self::One(source),
            Err(_) => Self::All,
        }
    }

    /// Back to the number a script would have written, for relaying over a
    /// wire that carries JSON rather than Rust types.
    pub fn to_script(self) -> i64 {
        match self {
            Self::All => -1,
            Self::One(source) => i64::from(source),
        }
    }
}

impl std::fmt::Display for EventTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::All => f.write_str("all"),
            Self::One(source) => write!(f, "{source}"),
        }
    }
}

/// Outbound message from a script runtime to the game transport.
#[derive(Debug)]
pub enum NetOutbound {
    /// `TriggerClientEvent(name, target, ...args)` — args as JSON array text.
    ClientEvent {
        target: EventTarget,
        event: String,
        args_json: String,
    },
    /// `TriggerClientEventInternal` — the payload is already msgpack, packed
    /// by the caller. Kept distinct from [`Self::ClientEvent`] so the gateway
    /// does not re-encode bytes that are already in wire form.
    ClientEventRaw {
        target: EventTarget,
        event: String,
        payload: Vec<u8>,
    },
}

#[cfg(test)]
mod tests {
    use super::EventTarget;

    #[test]
    fn minus_one_is_everyone_and_survives_a_round_trip() {
        assert_eq!(EventTarget::from_script(-1), EventTarget::All);
        assert_eq!(EventTarget::All.to_script(), -1);
    }

    /// Any negative value, not only -1: a script computing a target from an
    /// index that came out empty should broadcast rather than address a player
    /// four billion places away.
    #[test]
    fn any_negative_target_means_everyone() {
        for raw in [-1, -2, -999, i64::MIN] {
            assert_eq!(EventTarget::from_script(raw), EventTarget::All, "{raw}");
        }
    }

    #[test]
    fn a_source_id_stays_itself() {
        assert_eq!(EventTarget::from_script(7), EventTarget::One(7));
        assert_eq!(EventTarget::One(7).to_script(), 7);
    }

    /// Beyond u32 there is no player, and wrapping into one would deliver an
    /// event to whoever happens to hold that id.
    #[test]
    fn a_target_too_large_for_a_source_is_not_wrapped_into_one() {
        assert_eq!(
            EventTarget::from_script(i64::from(u32::MAX) + 1),
            EventTarget::All
        );
    }
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
