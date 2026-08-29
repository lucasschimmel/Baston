//! The generated table is only trustworthy if it still says what the spec says.
//! These tests re-read the vendored JSON at test time, so a stale `table.rs` —
//! someone editing it by hand, or forgetting to re-run the generator after
//! refreshing the artifact — fails the build instead of silently dispatching
//! natives to the wrong client with the wrong hash.

use super::table::CTX_NATIVES;
use super::{lookup, RpcContext};

/// The exact bytes vendored from FiveM artifact 35419.
const SPEC_JSON: &str = include_str!("../../../assets/rpc_natives.json");

fn spec_ctx_entries() -> Vec<serde_json::Value> {
    let spec: Vec<serde_json::Value> = serde_json::from_str(SPEC_JSON).expect("spec parses");
    spec.into_iter()
        .filter(|entry| entry.get("type").and_then(|t| t.as_str()) == Some("ctx"))
        .collect()
}

#[test]
fn table_covers_every_ctx_entry_of_the_spec() {
    let entries = spec_ctx_entries();
    assert_eq!(entries.len(), 69, "artifact 35419 has 69 ctx natives");
    assert_eq!(CTX_NATIVES.len(), entries.len());

    for entry in entries {
        let name = entry["name"].as_str().expect("name");
        let native = lookup(name).unwrap_or_else(|| panic!("{name} missing from the table"));

        let hash = entry["hash"].as_str().expect("hash");
        assert_eq!(
            format!("0x{:016X}", native.hash),
            format!("0x{:0>16}", hash.trim_start_matches("0x").to_uppercase()),
            "{name}: hash drifted from the spec"
        );

        let idx = entry["ctx"]["idx"].as_u64().expect("ctx.idx") as usize;
        let expected = match entry["ctx"]["type"].as_str().expect("ctx.type") {
            "Entity" => RpcContext::Entity { idx },
            "Player" => RpcContext::Player { idx },
            "ObjRef" => RpcContext::ObjectRef { idx },
            "ObjDel" => RpcContext::ObjectDelete { idx },
            other => panic!("{name}: unknown ctx type {other}"),
        };
        assert_eq!(native.context, expected, "{name}: context drifted");

        assert_eq!(
            native.arg_count,
            entry["args"].as_array().expect("args").len(),
            "{name}: argument count drifted"
        );
    }
}

/// The entity/object *constructors* need a server-side handle registry, which
/// does not exist yet — dispatching them would hand scripts a handle nothing
/// can resolve. They must stay out of the table.
#[test]
fn entity_and_object_constructors_are_out_of_scope() {
    for name in ["CREATE_PED", "CREATE_VEHICLE", "ADD_BLIP_FOR_COORD"] {
        assert!(lookup(name).is_none(), "{name} must not be RPC-dispatched");
    }
}

/// `lookup` binary-searches, which is only correct on a sorted table.
#[test]
fn table_is_sorted_by_name() {
    assert!(
        CTX_NATIVES
            .windows(2)
            .all(|pair| pair[0].name < pair[1].name),
        "table.rs must stay sorted by name"
    );
    assert!(lookup("SET_PED_ARMOUR").is_some());
    assert!(lookup("NOT_A_NATIVE").is_none());
}

/// Spot-check the two context kinds BASTON actually routes, so a regression in
/// the generator's ctx mapping cannot pass unnoticed.
#[test]
fn context_kinds_are_mapped_from_the_spec() {
    assert_eq!(
        lookup("SET_VEHICLE_DOORS_LOCKED").expect("present").context,
        RpcContext::Entity { idx: 0 }
    );
    assert_eq!(
        lookup("SET_PLAYER_WANTED_LEVEL").expect("present").context,
        RpcContext::Player { idx: 0 }
    );
    assert_eq!(
        lookup("REMOVE_BLIP").expect("present").context,
        RpcContext::ObjectDelete { idx: 0 }
    );
}

// ── Routing behaviour ──
//
// A correct table is not enough: these natives mutate one player's game from
// another player's script, so *which* client receives the call is a
// correctness property of its own. A wrong target lands someone's action on a
// stranger's screen.

use super::{resolve_target, SkipReason};
use crate::{EntitySummary, EntityWorldView, ScriptEntityType};

fn world_with(network_id: u32, owner: u32) -> EntityWorldView {
    let world = EntityWorldView::new();
    world.publish([EntitySummary {
        network_id,
        owner,
        entity_type: ScriptEntityType::Vehicle,
        position: [0.0; 3],
        velocity: [0.0; 3],
        routing_bucket: 0,
        health: None,
        max_health: None,
        armour: None,
        model: None,
        heading: None,
        desired_heading: None,
        sync: Default::default(),
    }]);
    world
}

#[test]
fn entity_context_routes_to_the_owning_client() {
    let native = lookup("SET_VEHICLE_DOORS_LOCKED").expect("an Entity-context native");
    let RpcContext::Entity { idx } = native.context else {
        panic!("expected an entity context");
    };
    let mut args = vec![serde_json::json!(0); native.arg_count];
    args[idx] = serde_json::json!(4321);

    assert_eq!(resolve_target(&world_with(4321, 9), native, &args), Ok(9));
}

/// Before the first sync tick — or on a server running without OneSync — the
/// mirror is empty. A script calling these natives then is early, not wrong:
/// the call must be dropped, never sent to an arbitrary client.
#[test]
fn an_unowned_entity_is_not_dispatched_anywhere() {
    let native = lookup("SET_VEHICLE_DOORS_LOCKED").expect("an Entity-context native");
    let args = vec![serde_json::json!(4321); native.arg_count];

    assert_eq!(
        resolve_target(&EntityWorldView::new(), native, &args),
        Err(SkipReason::NoOwner),
        "an empty world routes nowhere"
    );
    assert_eq!(
        resolve_target(&world_with(4321, 0), native, &args),
        Err(SkipReason::NoOwner),
        "an entity nobody simulates routes nowhere"
    );
}

#[test]
fn player_context_routes_to_the_given_net_id() {
    let native = CTX_NATIVES
        .iter()
        .find(|native| matches!(native.context, RpcContext::Player { .. }))
        .expect("the spec has Player-context natives");
    let RpcContext::Player { idx } = native.context else {
        unreachable!()
    };
    let mut args = vec![serde_json::json!(0); native.arg_count];

    // The argument already is the target: no entity mirror is consulted.
    args[idx] = serde_json::json!(12);
    assert_eq!(
        resolve_target(&EntityWorldView::new(), native, &args),
        Ok(12)
    );

    args[idx] = serde_json::json!(0);
    assert_eq!(
        resolve_target(&EntityWorldView::new(), native, &args),
        Err(SkipReason::NoTarget)
    );
}

/// Blip-context natives stay in the table so coverage stays honest, but they
/// route on a server-created handle BASTON does not track yet. They must skip
/// explicitly rather than resolve to something plausible.
#[test]
fn blip_contexts_are_refused_rather_than_guessed() {
    for native in CTX_NATIVES.iter().filter(|native| {
        matches!(
            native.context,
            RpcContext::ObjectRef { .. } | RpcContext::ObjectDelete { .. }
        )
    }) {
        let args = vec![serde_json::json!(1); native.arg_count];
        assert_eq!(
            resolve_target(&EntityWorldView::new(), native, &args),
            Err(SkipReason::UnsupportedContext),
            "{} must not guess a target",
            native.name
        );
    }
}
