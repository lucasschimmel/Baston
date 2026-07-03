//! Server → client native dispatch (jalon B4).
//!
//! The stock FiveM client has no dedicated "NativeCall" packet — the server
//! cannot execute GTA natives remotely through the base protocol. BASTON
//! dispatches them over the net-event channel: the server sends the
//! `__baston:invokeNative` event `{ id, hash, args }` to the target client,
//! where the BASTON client shim executes the native locally
//! (`Citizen.invokeNative`) and replies with a `__baston:nativeResult`
//! server event `{ id, result }`.
//!
//! Native hashes below were resolved against `FivemDocs/natives/` — never
//! trust hardcoded hashes from other sources (the roadmap's
//! `NETWORK_REQUEST_CONTROL_OF_ENTITY` hash was wrong).

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

/// Event names used by the native-dispatch shim.
pub const INVOKE_NATIVE_EVENT: &str = "__baston:invokeNative";
pub const NATIVE_RESULT_EVENT: &str = "__baston:nativeResult";

/// Strongly-typed native hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NativeHash(pub u64);

/// Argument/return kinds understood by the dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeType {
    Int,
    Float,
    Bool,
    String,
    Vector3,
    Entity,
    Void,
}

/// One native call en route to a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeCall {
    pub id: u64,
    /// Hex string (`"0x..."`) — msgpack/JS cannot carry u64 losslessly.
    pub hash: String,
    pub args: Vec<serde_json::Value>,
}

/// Signature metadata for validation.
#[derive(Debug, Clone)]
pub struct NativeSignature {
    pub name: &'static str,
    pub hash: NativeHash,
    pub args: &'static [NativeType],
    pub returns: NativeType,
}

/// Phase B native table (spawn sequence only). Hashes verified against
/// `FivemDocs/natives/<NS>/<Name>.md`.
pub const PHASE_B_NATIVES: &[NativeSignature] = &[
    NativeSignature {
        name: "GET_PLAYER_PED",
        hash: NativeHash(0x43A66C31C68491C0),
        args: &[NativeType::Int],
        returns: NativeType::Entity,
    },
    NativeSignature {
        name: "REQUEST_MODEL",
        hash: NativeHash(0x963D27A58DF860AC),
        args: &[NativeType::Int],
        returns: NativeType::Void,
    },
    NativeSignature {
        name: "HAS_MODEL_LOADED",
        hash: NativeHash(0x98A4EB5D89A0C952),
        args: &[NativeType::Int],
        returns: NativeType::Bool,
    },
    NativeSignature {
        name: "SET_PLAYER_MODEL",
        hash: NativeHash(0x00A1CADD00108836),
        args: &[NativeType::Int, NativeType::Int],
        returns: NativeType::Void,
    },
    NativeSignature {
        name: "SET_ENTITY_COORDS",
        hash: NativeHash(0x06843DA7060A026B),
        args: &[
            NativeType::Entity,
            NativeType::Float,
            NativeType::Float,
            NativeType::Float,
            NativeType::Bool,
            NativeType::Bool,
            NativeType::Bool,
            NativeType::Bool,
        ],
        returns: NativeType::Void,
    },
    NativeSignature {
        name: "SET_ENTITY_HEADING",
        hash: NativeHash(0x8E2530AA8ADA980E),
        args: &[NativeType::Entity, NativeType::Float],
        returns: NativeType::Void,
    },
    NativeSignature {
        name: "FREEZE_ENTITY_POSITION",
        hash: NativeHash(0x428CA6DBD1094446),
        args: &[NativeType::Entity, NativeType::Bool],
        returns: NativeType::Void,
    },
    NativeSignature {
        name: "GET_ENTITY_COORDS",
        hash: NativeHash(0x3FEF770D40960D5A),
        args: &[NativeType::Entity, NativeType::Bool],
        returns: NativeType::Vector3,
    },
    NativeSignature {
        // FivemDocs/natives/NETWORK/NetworkRequestControlOfEntity.md
        // (the Phase B roadmap listed 0x3C4BF6C4FC4C6CC3, which is wrong).
        name: "NETWORK_REQUEST_CONTROL_OF_ENTITY",
        hash: NativeHash(0xB69317BF5E782347),
        args: &[NativeType::Entity],
        returns: NativeType::Bool,
    },
];

/// Lookup + arity validation for dispatched natives.
pub struct NativeRegistry {
    by_hash: DashMap<u64, &'static NativeSignature>,
}

impl Default for NativeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeRegistry {
    pub fn new() -> Self {
        let by_hash = DashMap::new();
        for sig in PHASE_B_NATIVES {
            by_hash.insert(sig.hash.0, sig);
        }
        Self { by_hash }
    }

    pub fn get(&self, hash: u64) -> Option<&'static NativeSignature> {
        self.by_hash.get(&hash).map(|s| *s)
    }

    /// Validate a call: known hash and matching argument count.
    pub fn validate(
        &self,
        hash: u64,
        arg_count: usize,
    ) -> Result<&'static NativeSignature, String> {
        let sig = self
            .get(hash)
            .ok_or_else(|| format!("unknown native hash 0x{hash:016X}"))?;
        if arg_count != sig.args.len() {
            return Err(format!(
                "{}: expected {} args, got {arg_count}",
                sig.name,
                sig.args.len()
            ));
        }
        Ok(sig)
    }
}

/// Pending server→client native calls awaiting their result event.
#[derive(Default)]
pub struct PendingNatives {
    waiters: DashMap<u64, tokio::sync::oneshot::Sender<serde_json::Value>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl PendingNatives {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new call; returns (call id, result receiver).
    pub fn register(&self) -> (u64, tokio::sync::oneshot::Receiver<serde_json::Value>) {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.waiters.insert(id, tx);
        (id, rx)
    }

    /// Resolve a call with the client's result. Returns false when unknown
    /// (already timed out or bogus id).
    pub fn resolve(&self, id: u64, result: serde_json::Value) -> bool {
        match self.waiters.remove(&id) {
            Some((_, tx)) => tx.send(result).is_ok(),
            None => false,
        }
    }

    /// Drop a waiter after a timeout.
    pub fn cancel(&self, id: u64) {
        self.waiters.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_validates_known_natives() {
        let registry = NativeRegistry::new();
        let sig = registry.validate(0x43A66C31C68491C0, 1).unwrap();
        assert_eq!(sig.name, "GET_PLAYER_PED");
        assert!(registry.validate(0x43A66C31C68491C0, 2).is_err());
        assert!(registry.validate(0xDEAD_BEEF, 0).is_err());
    }

    #[tokio::test]
    async fn pending_natives_resolve_roundtrip() {
        let pending = PendingNatives::new();
        let (id, rx) = pending.register();
        assert!(pending.resolve(id, serde_json::json!(1234)));
        assert_eq!(rx.await.unwrap(), serde_json::json!(1234));
        assert!(!pending.resolve(id, serde_json::json!(0)), "single-shot");
    }

    #[test]
    fn native_call_serializes_for_shim() {
        let call = NativeCall {
            id: 7,
            hash: format!("0x{:016X}", 0x43A66C31C68491C0u64),
            args: vec![serde_json::json!(1)],
        };
        let json = serde_json::to_value(&call).unwrap();
        assert_eq!(json["hash"], "0x43A66C31C68491C0");
        assert_eq!(json["id"], 7);
    }
}
