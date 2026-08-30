//! Engine-neutral state behind the CFX natives.
//!
//! The natives are the largest body of logic in BASTON's scripting layer and
//! the part that must behave identically whichever VM invoked them. They used
//! to reach their dependencies through `deno_core::OpState`, which tied every
//! native to V8 and would have forced a second implementation for Lua
//! (ADR-002, Tier 2).
//!
//! [`NativeState`] is the same service locator with the engine removed: a
//! type-map the host fills once and both runtimes borrow from. Keeping the
//! locator shape — rather than flattening the services into a struct — is
//! deliberate: it made the JS natives' migration a signature change instead of
//! a rewrite of every call site, so the V8 path that this refactor must not
//! regress kept its exact semantics.

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;

use crate::deferrals::DeferralRegistry;
use crate::observability::Observability;
use crate::{RoutingControl, StateBagStore};

/// Type-map of everything a native may need.
///
/// One per resource runtime, owned by that runtime's thread — hence no
/// interior locking. Values are inserted by the host before any script runs;
/// a missing value is a wiring bug in BASTON, not a script error, so the
/// borrows panic rather than returning an `Option` nobody could act on.
#[derive(Default)]
pub struct NativeState {
    entries: HashMap<TypeId, Box<dyn Any>>,
}

impl NativeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a value, replacing any previous one of the same type.
    ///
    /// Replacement is load-bearing: the host installs placeholder services at
    /// construction (an in-memory KVP, a no-op world control) and swaps in the
    /// real ones once the process has them.
    pub fn put<T: 'static>(&mut self, value: T) {
        self.entries.insert(TypeId::of::<T>(), Box::new(value));
    }

    pub fn borrow<T: 'static>(&self) -> &T {
        self.try_borrow()
            .unwrap_or_else(|| missing::<T>())
    }

    pub fn borrow_mut<T: 'static>(&mut self) -> &mut T {
        if !self.entries.contains_key(&TypeId::of::<T>()) {
            missing::<T>();
        }
        self.entries
            .get_mut(&TypeId::of::<T>())
            .and_then(|slot| slot.downcast_mut())
            .unwrap_or_else(|| missing::<T>())
    }

    /// Borrow a service that legitimately may not be installed.
    pub fn try_borrow<T: 'static>(&self) -> Option<&T> {
        self.entries
            .get(&TypeId::of::<T>())
            .and_then(|slot| slot.downcast_ref())
    }

    pub fn contains<T: 'static>(&self) -> bool {
        self.entries.contains_key(&TypeId::of::<T>())
    }
}

#[cold]
#[inline(never)]
fn missing<T: 'static>() -> ! {
    panic!(
        "NativeState is missing {} — the script host must install every \
         service before a resource runs",
        std::any::type_name::<T>()
    )
}

/// Per-runtime context: everything that belongs to one resource rather than to
/// the process.
pub struct RuntimeContext {
    pub resource_name: String,
    /// Millisecond epoch for `GetGameTimer()` — the script host start instant.
    pub host_started_at: Instant,
    /// Events queued by `TriggerEvent` during script execution; drained by the
    /// host after each dispatch and re-broadcast to every runtime.
    pub queued_events: VecDeque<(String, String)>,
    /// Event names with at least one registered handler (bookkeeping).
    pub handled_events: HashSet<String>,
    /// Export names registered by this resource (bookkeeping).
    pub exports: HashSet<String>,
    /// Server commands registered by this resource via `RegisterCommand`.
    pub commands: HashMap<String, bool>,
    /// Whether this resource registered a `RegisterZoneTransferState` callback.
    pub has_zone_transfer_state: bool,
    /// JSON collected during the last `collectZoneTransferState` dispatch
    /// (jalon D4 handoff).
    pub collected_transfer_state: Option<String>,
    /// Handler exceptions caught by the runtime during the current dispatch.
    pub handler_errors: u64,
}

impl RuntimeContext {
    /// A context for `resource_name`, with the bookkeeping empty.
    pub fn new(resource_name: &str, host_started_at: Instant) -> Self {
        Self {
            resource_name: resource_name.to_owned(),
            host_started_at,
            queued_events: VecDeque::new(),
            handled_events: HashSet::new(),
            exports: HashSet::new(),
            commands: HashMap::new(),
            has_zone_transfer_state: false,
            collected_transfer_state: None,
            handler_errors: 0,
        }
    }
}

/// The authoritative surfaces every resource isolate shares, installed once
/// per isolate before any resource script runs.
///
/// All of it is cheap to clone (`Arc` handles or handles over one) — the host
/// builds a fresh value per resource thread.
pub struct SharedGameState {
    pub state_bags: StateBagStore,
    pub routing: Arc<dyn RoutingControl>,
    pub entity_world: Arc<crate::EntityWorldView>,
    pub world_control: Arc<dyn crate::WorldControl>,
    pub kvp: Arc<crate::KvpStore>,
    /// `None` until a composition root wires an outbound HTTP worker.
    pub http: Option<crate::HttpBridge>,
    pub http_handlers: Arc<crate::HttpHandlerRegistry>,
    pub resource_control: Arc<dyn crate::ResourceControl>,
}

/// Shared deferral registry handle (one per process, cloned into every
/// runtime).
// Read by the JS deferral ops. Installed on both engines regardless: the
// Lua prelude will expose deferrals against the same registry.
#[cfg_attr(not(feature = "js"), allow(dead_code))]
pub struct SharedDeferrals(pub Arc<DeferralRegistry>);

/// Shared player directory handle (owned by the gateway, read by player
/// natives).
pub struct SharedPlayers(pub Arc<baston_protocol::PlayerDirectory>);

/// Net bridge handle (client events + native dispatch).
pub struct SharedNet(pub crate::net_bridge::NetBridge);

/// Shared runtime observability collector.
pub struct SharedObservability(pub Arc<Observability>);

/// Shared console variables (`GetConvar*` / `SetConvar*`).
pub struct SharedConvars(pub Arc<DashMap<String, String>>);

/// Shared resource snapshot (`GetResourceState`, `LoadResourceFile`, ...).
pub struct SharedResources(pub crate::resource_registry::ResourceRegistry);

/// Shared state-bag store (one per script host).
#[derive(Clone)]
pub struct SharedStateBags(pub StateBagStore);

/// Shared routing-bucket control surface.
#[derive(Clone)]
pub struct SharedRouting(pub Arc<dyn RoutingControl>);

/// Shared read-only mirror of the authoritative networked world, backing the
/// entity natives. Empty until a game state publishes into it, in which case
/// entity natives report "no such entity" rather than fabricating an answer.
#[derive(Clone)]
pub struct SharedEntityWorld(pub Arc<crate::EntityWorldView>);

/// Write side of the world: entity creation and deletion from scripts.
#[derive(Clone)]
pub struct SharedWorldControl(pub Arc<dyn crate::WorldControl>);

/// Persistent per-resource key/value store (`*ResourceKvp*` natives).
#[derive(Clone)]
pub struct SharedKvp(pub Arc<crate::KvpStore>);

/// Outbound HTTP bridge (`PerformHttpRequest`).
///
/// `None` means no worker is wired, in which case the natives refuse instead
/// of handing back a token nobody will ever resolve.
#[derive(Clone)]
pub struct SharedHttp(pub Option<crate::HttpBridge>);

/// Resource lifecycle control (`StartResource`, `StopResource`).
#[derive(Clone)]
pub struct SharedResourceControl(pub Arc<dyn crate::ResourceControl>);

/// Inbound HTTP handler registry (`SetHttpHandler`). Shared with the gateway,
/// which owns the route that feeds it.
#[derive(Clone)]
#[cfg_attr(not(feature = "js"), allow(dead_code))]
pub struct SharedHttpHandlers(pub Arc<crate::HttpHandlerRegistry>);

/// Server-side voice control surface backing the `MUMBLE_*` natives. The
/// gateway implements this on the baston-voice handle so baston-scripting
/// stays decoupled from the voice crate. `None` = voice disabled: the natives
/// keep returning neutral defaults (stub behaviour).
pub trait VoiceControl: Send + Sync {
    fn create_channel(&self, id: u32);
    fn channel_exists(&self, id: u32) -> bool;
    fn set_player_muted(&self, netid: u32, muted: bool);
    fn is_player_muted(&self, netid: u32) -> bool;
    fn set_proximity_override(&self, netid: u32, position: Option<[f32; 3]>);
    fn proximity_override(&self, netid: u32) -> [f32; 3];
}

/// Voice control surface, absent when the voice module is off.
pub struct SharedVoice(pub Option<Arc<dyn VoiceControl>>);

#[cfg(test)]
mod tests {
    use super::*;

    struct A(u32);
    struct B(&'static str);

    #[test]
    fn values_round_trip_by_type() {
        let mut state = NativeState::new();
        state.put(A(7));
        state.put(B("hello"));
        assert_eq!(state.borrow::<A>().0, 7);
        assert_eq!(state.borrow::<B>().0, "hello");
    }

    #[test]
    fn put_replaces_a_previous_value() {
        // The host installs placeholders at construction and swaps in the real
        // services later; replacement has to be the documented behaviour.
        let mut state = NativeState::new();
        state.put(A(1));
        state.put(A(2));
        assert_eq!(state.borrow::<A>().0, 2);
    }

    #[test]
    fn borrow_mut_mutates_in_place() {
        let mut state = NativeState::new();
        state.put(A(1));
        state.borrow_mut::<A>().0 = 42;
        assert_eq!(state.borrow::<A>().0, 42);
    }

    #[test]
    fn try_borrow_reports_absence_without_panicking() {
        let state = NativeState::new();
        assert!(state.try_borrow::<A>().is_none());
        assert!(!state.contains::<A>());
    }

    #[test]
    #[should_panic(expected = "NativeState is missing")]
    fn borrowing_an_uninstalled_service_panics_with_the_type_name() {
        NativeState::new().borrow::<A>();
    }
}
