//! `Citizen.invokeNative` server-side implementations: the shared CFX natives
//! (KVP, state bags, …) and the server-only natives backed by the synthetic
//! entity store, player directory, and voice control surface.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;

use dashmap::DashMap;

use super::{
    console_buffer_text, RuntimeContext, SharedConvars, SharedEntityWorld, SharedHttp, SharedKvp,
    SharedPlayers, SharedResourceControl, SharedRouting, SharedStateBags, SharedVoice,
    SharedWorldControl, VoiceControl,
};
use super::{rpc, world, NativeState};
use crate::ScriptEntityType;
use crate::{
    entity_from_state_bag_name, entity_state_bag_name, player_from_state_bag_name, RoutingControl,
    RoutingLockdownMode, StateBagSource,
};

fn synthetic_entities() -> &'static DashMap<u32, serde_json::Value> {
    static ENTITIES: OnceLock<DashMap<u32, serde_json::Value>> = OnceLock::new();
    ENTITIES.get_or_init(DashMap::new)
}

fn next_synthetic_entity() -> u32 {
    static NEXT_ENTITY: AtomicU32 = AtomicU32::new(10_000);
    NEXT_ENTITY.fetch_add(1, Ordering::Relaxed)
}

fn json_args(args_json: &str) -> Vec<serde_json::Value> {
    serde_json::from_str(args_json).unwrap_or_default()
}

fn json_arg_string(args: &[serde_json::Value], index: usize) -> String {
    args.get(index)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned()
}

fn json_arg_f64(args: &[serde_json::Value], index: usize) -> f64 {
    args.get(index).and_then(|v| v.as_f64()).unwrap_or_default()
}

pub(super) fn json_arg_i64(args: &[serde_json::Value], index: usize) -> i64 {
    args.get(index).and_then(|v| v.as_i64()).unwrap_or_default()
}

/// Player/net-id argument: natives pass server ids as numbers or numeric
/// strings (`source` is stringly-typed in much of the FiveM ecosystem).
///
/// Shared with [`super::rpc`], which reads the same kind of argument to
/// resolve a context native's target.
pub(super) fn json_arg_netid(args: &[serde_json::Value], index: usize) -> u32 {
    match args.get(index) {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or_default() as u32,
        Some(serde_json::Value::String(s)) => s.trim().parse().unwrap_or_default(),
        _ => 0,
    }
}

pub(super) fn json_arg_bool(args: &[serde_json::Value], index: usize) -> bool {
    match args.get(index) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or_default() != 0,
        _ => false,
    }
}

/// The shared CFX natives (KVP, state bags, convars, resources).
///
/// The JS polyfill knows which dispatcher owns a name and calls this one
/// directly; Lua cannot, and goes through [`cfx_native`].
#[cfg(feature = "js")]
pub(crate) fn cfx_shared_native(
    state: &mut NativeState,
    name: String,
    args_json: String,
) -> String {
    let resource = state.borrow::<RuntimeContext>().resource_name.clone();
    match shared_native_value(state, &name, &args_json) {
        Some(value) => value.to_string(),
        None => unimplemented_native(&name, "void", &resource).to_string(),
    }
}

/// Route a native to whichever dispatcher owns it.
///
/// A caller that does not already know whether a name is shared or
/// server-only — the Lua global resolver, for one — asks here. The shared set
/// is tried first because it is the one whose names are exact.
#[cfg(feature = "lua")]
pub(crate) fn cfx_native(
    state: &mut NativeState,
    name: String,
    result_kind: String,
    args_json: String,
) -> String {
    match shared_native_value(state, &name, &args_json) {
        Some(value) => value.to_string(),
        None => cfx_server_native(state, name, result_kind, args_json),
    }
}

/// `None` means "not one of the shared natives", so the caller can fall
/// through instead of receiving a fabricated neutral value.
fn shared_native_value(
    state: &mut NativeState,
    name: &str,
    args_json: &str,
) -> Option<serde_json::Value> {
    let name = name.to_owned();
    let args_json = args_json.to_owned();
    let args = json_args(&args_json);
    let resource = state.borrow::<RuntimeContext>().resource_name.clone();
    let kvp = Arc::clone(&state.borrow::<SharedKvp>().0);

    let value = match name.as_str() {
        "ADD_CONVAR_CHANGE_LISTENER" => serde_json::json!(0),
        // Kept for direct native callers. The public JS helper uses a
        // dedicated op because functions cannot be serialized through JSON.
        "ADD_STATE_BAG_CHANGE_HANDLER" => {
            let callback_id = json_arg_i64(&args, 2) as u32;
            let cookie = state.borrow::<SharedStateBags>().0.add_handler(
                resource.clone(),
                Some(json_arg_string(&args, 0)),
                Some(json_arg_string(&args, 1)),
                callback_id,
            );
            serde_json::json!(cookie)
        }
        "CANCEL_EVENT"
        | "END_FIND_KVP"
        | "PROFILER_ENTER_SCOPE"
        | "PROFILER_EXIT_SCOPE"
        | "REGISTER_RESOURCE_AS_EVENT_HANDLER"
        | "REMOVE_CONVAR_CHANGE_LISTENER"
        | "TRIGGER_EVENT_INTERNAL" => serde_json::Value::Null,
        "ENSURE_ENTITY_STATE_BAG" => {
            state
                .borrow::<SharedStateBags>()
                .0
                .ensure_bag(entity_state_bag_name(json_arg_netid(&args, 0)));
            serde_json::Value::Null
        }
        "REMOVE_STATE_BAG_CHANGE_HANDLER" => {
            state
                .borrow::<SharedStateBags>()
                .0
                .remove_handler(&resource, json_arg_i64(&args, 0) as u32);
            serde_json::Value::Null
        }
        "DELETE_FUNCTION_REFERENCE" => serde_json::Value::Null,
        "DELETE_RESOURCE_KVP" | "DELETE_RESOURCE_KVP_NO_SYNC" => {
            kvp.remove(
                &resource,
                &json_arg_string(&args, 0),
                !name.ends_with("_NO_SYNC"),
            );
            serde_json::Value::Null
        }
        "DUPLICATE_FUNCTION_REFERENCE" => serde_json::json!(json_arg_string(&args, 0)),
        "EXECUTE_COMMAND" => {
            tracing::warn!(
                target: "commands",
                resource = %resource,
                command = %json_arg_string(&args, 0),
                "ExecuteCommand from a resource is not routed to the host command bus yet"
            );
            serde_json::Value::Null
        }
        "FIND_KVP" => {
            serde_json::json!(kvp.keys_with_prefix(&resource, &json_arg_string(&args, 0)))
        }
        // Force every deferred `_NO_SYNC` write out to disk. Blocking on
        // purpose: the contract is that the data is durable when this returns.
        "FLUSH_RESOURCE_KVP" => {
            kvp.flush();
            serde_json::Value::Null
        }
        "FORMAT_STACK_TRACE" => serde_json::json!(json_arg_string(&args, 0)),
        "GET_ENTITIES_IN_RADIUS"
        | "GET_GAME_POOL"
        | "GET_REGISTERED_COMMANDS"
        | "GET_RESOURCE_COMMANDS" => serde_json::json!([]),
        "GET_STATE_BAG_KEYS" => {
            serde_json::json!(state
                .borrow::<SharedStateBags>()
                .0
                .keys(&json_arg_string(&args, 0)))
        }
        "GET_ENTITY_FROM_STATE_BAG_NAME" => serde_json::json!(entity_from_state_bag_name(
            &json_arg_string(&args, 0)
        )
        .unwrap_or_default()),
        "GET_GAME_BUILD_NUMBER" => serde_json::json!(0),
        "GET_GAME_NAME" => serde_json::json!("gta5"),
        "GET_INSTANCE_ID" => serde_json::json!(0),
        "GET_INVOKING_RESOURCE" => serde_json::json!(resource),
        "GET_PLAYER_FROM_STATE_BAG_NAME" => serde_json::json!(player_from_state_bag_name(
            &json_arg_string(&args, 0)
        )
        .unwrap_or_default()),
        "GET_RESOURCE_KVP_FLOAT" => kvp
            .get(&resource, &json_arg_string(&args, 0))
            .and_then(|v| v.parse::<f64>().ok())
            .map(serde_json::Value::from)
            .unwrap_or_else(|| serde_json::json!(0.0)),
        "GET_RESOURCE_KVP_INT" => kvp
            .get(&resource, &json_arg_string(&args, 0))
            .and_then(|v| v.parse::<i64>().ok())
            .map(serde_json::Value::from)
            .unwrap_or_else(|| serde_json::json!(0)),
        "GET_RESOURCE_KVP_STRING" => kvp
            .get(&resource, &json_arg_string(&args, 0))
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null),
        "GET_STATE_BAG_VALUE" => state
            .borrow::<SharedStateBags>()
            .0
            .get(&json_arg_string(&args, 0), &json_arg_string(&args, 1))
            .unwrap_or(serde_json::Value::Null),
        "IS_ACE_ALLOWED"
        | "IS_PRINCIPAL_ACE_ALLOWED"
        | "PROFILER_IS_RECORDING"
        | "WAS_EVENT_CANCELED" => serde_json::json!(false),
        "STATE_BAG_HAS_KEY" => serde_json::json!(state
            .borrow::<SharedStateBags>()
            .0
            .contains_key(&json_arg_string(&args, 0), &json_arg_string(&args, 1))),
        "IS_DUPLICITY_VERSION" => serde_json::json!(true),
        "SET_RESOURCE_KVP" | "SET_RESOURCE_KVP_NO_SYNC" => {
            kvp.set(
                &resource,
                &json_arg_string(&args, 0),
                json_arg_string(&args, 1),
                !name.ends_with("_NO_SYNC"),
            );
            serde_json::Value::Null
        }
        "SET_RESOURCE_KVP_FLOAT" | "SET_RESOURCE_KVP_FLOAT_NO_SYNC" => {
            kvp.set(
                &resource,
                &json_arg_string(&args, 0),
                json_arg_f64(&args, 1).to_string(),
                !name.ends_with("_NO_SYNC"),
            );
            serde_json::Value::Null
        }
        "SET_RESOURCE_KVP_INT" | "SET_RESOURCE_KVP_INT_NO_SYNC" => {
            kvp.set(
                &resource,
                &json_arg_string(&args, 0),
                json_arg_i64(&args, 1).to_string(),
                !name.ends_with("_NO_SYNC"),
            );
            serde_json::Value::Null
        }
        "SET_STATE_BAG_VALUE" => {
            state.borrow::<SharedStateBags>().0.set(
                json_arg_string(&args, 0),
                json_arg_string(&args, 1),
                args.get(2).cloned().unwrap_or(serde_json::Value::Null),
                json_arg_bool(&args, 4),
                StateBagSource::resource(resource),
            );
            serde_json::Value::Null
        }
        // Shared train/vehicle/entity accessors that need the future entity
        // state-bag bridge. Return neutral values instead of throwing so
        // resources can feature-detect safely — but report the gap, because a
        // silent `null` is indistinguishable from a genuine empty answer.
        _ => return None,
    };

    Some(value)
}

/// The server-only natives: synthetic entities, players, voice, world.
pub(crate) fn cfx_server_native(
    state: &mut NativeState,
    name: String,
    result_kind: String,
    args_json: String,
) -> String {
    let args = json_args(&args_json);
    let entities = synthetic_entities();

    let value = match name.as_str() {
        "CREATE_OBJECT" | "CREATE_OBJECT_NO_OFFSET" => {
            let id = create_entity_handle(
                state,
                ScriptEntityType::Object,
                json_arg_i64(&args, 0) as u32,
                json_arg_position(&args, 1),
                0.0,
                json_arg_bool(&args, 5),
            );
            entities.insert(
                id,
                serde_json::json!({
                    "type": "object",
                    "model": json_arg_i64(&args, 0),
                    "coords": [json_arg_f64(&args, 1), json_arg_f64(&args, 2), json_arg_f64(&args, 3)],
                    "heading": 0.0,
                    "health": 1000,
                    "routing_bucket": 0,
                }),
            );
            shared_routing(state).set_entity_bucket(id, 0);
            serde_json::json!(id)
        }
        "CREATE_PED" => {
            let id = create_entity_handle(
                state,
                ScriptEntityType::Ped,
                json_arg_i64(&args, 1) as u32,
                json_arg_position(&args, 2),
                json_arg_f64(&args, 5) as f32,
                false,
            );
            entities.insert(
                id,
                serde_json::json!({
                    "type": "ped",
                    "model": json_arg_i64(&args, 1),
                    "coords": [json_arg_f64(&args, 2), json_arg_f64(&args, 3), json_arg_f64(&args, 4)],
                    "heading": json_arg_f64(&args, 5),
                    "health": 200,
                    "routing_bucket": 0,
                }),
            );
            shared_routing(state).set_entity_bucket(id, 0);
            serde_json::json!(id)
        }
        "CREATE_PED_INSIDE_VEHICLE" => {
            let id = next_synthetic_entity();
            entities.insert(
                id,
                serde_json::json!({
                    "type": "ped",
                    "vehicle": json_arg_i64(&args, 0),
                    "model": json_arg_i64(&args, 2),
                    "seat": json_arg_i64(&args, 3),
                    "coords": [0.0, 0.0, 0.0],
                    "heading": 0.0,
                    "health": 200,
                    "routing_bucket": 0,
                }),
            );
            shared_routing(state).set_entity_bucket(id, 0);
            serde_json::json!(id)
        }
        "CREATE_VEHICLE" | "CREATE_VEHICLE_SERVER_SETTER" => {
            let coord_offset = if name == "CREATE_VEHICLE_SERVER_SETTER" {
                2
            } else {
                1
            };
            let id = create_entity_handle(
                state,
                ScriptEntityType::Vehicle,
                json_arg_i64(&args, 0) as u32,
                json_arg_position(&args, coord_offset),
                json_arg_f64(&args, coord_offset + 3) as f32,
                false,
            );
            entities.insert(
                id,
                serde_json::json!({
                    "type": "vehicle",
                    "model": json_arg_i64(&args, 0),
                    "coords": [
                        json_arg_f64(&args, coord_offset),
                        json_arg_f64(&args, coord_offset + 1),
                        json_arg_f64(&args, coord_offset + 2)
                    ],
                    "heading": json_arg_f64(&args, coord_offset + 3),
                    "health": 1000,
                    "engine_health": 1000.0,
                    "routing_bucket": 0,
                }),
            );
            shared_routing(state).set_entity_bucket(id, 0);
            serde_json::json!(id)
        }
        "DELETE_ENTITY" | "DELETE_TRAIN" => {
            let id = json_arg_i64(&args, 0) as u32;
            entities.remove(&id);
            state
                .borrow::<SharedWorldControl>()
                .0
                .submit(crate::WorldCommand::Despawn { network_id: id });
            shared_routing(state).remove_entity(id);
            state
                .borrow::<SharedStateBags>()
                .0
                .remove_bag(&entity_state_bag_name(id));
            serde_json::Value::Null
        }
        "DOES_ENTITY_EXIST" => {
            let id = json_arg_i64(&args, 0) as u32;
            serde_json::json!(shared_world(state).exists(id) || entities.contains_key(&id))
        }
        "GET_ALL_OBJECTS" => all_entities(state, ScriptEntityType::Object, "object"),
        "GET_ALL_PEDS" => all_entities(state, ScriptEntityType::Ped, "ped"),
        "GET_ALL_VEHICLES" => all_entities(state, ScriptEntityType::Vehicle, "vehicle"),
        "GET_ENTITY_COORDS" => {
            let id = json_arg_i64(&args, 0) as u32;
            shared_world(state)
                .get(id)
                .map(|entity| serde_json::json!(entity.position))
                .or_else(|| entity_field(id, "coords"))
                .unwrap_or_else(|| serde_json::json!([0.0, 0.0, 0.0]))
        }
        "GET_ENTITY_VELOCITY" => {
            let id = json_arg_i64(&args, 0) as u32;
            shared_world(state)
                .get(id)
                .map(|entity| serde_json::json!(entity.velocity))
                .unwrap_or_else(|| serde_json::json!([0.0, 0.0, 0.0]))
        }
        // A script handle IS the network id (see `entity_world`), so both
        // translations are the identity — but scripts must still be able to
        // call them, and an unknown handle must resolve to 0.
        "NETWORK_GET_NETWORK_ID_FROM_ENTITY" | "NETWORK_GET_ENTITY_FROM_NETWORK_ID" => {
            let id = json_arg_i64(&args, 0) as u32;
            let known = shared_world(state).exists(id) || entities.contains_key(&id);
            serde_json::json!(if known { id } else { 0 })
        }
        "GET_PLAYER_PED" => {
            serde_json::json!(shared_world(state)
                .player_ped(json_arg_netid(&args, 0))
                .unwrap_or(0))
        }
        "GET_ENTITY_HEADING" => entity_field(json_arg_i64(&args, 0) as u32, "heading")
            .unwrap_or_else(|| serde_json::json!(0.0)),
        "GET_ENTITY_HEALTH" => entity_field(json_arg_i64(&args, 0) as u32, "health")
            .unwrap_or_else(|| serde_json::json!(0)),
        "GET_ENTITY_MODEL" => entity_field(json_arg_i64(&args, 0) as u32, "model")
            .unwrap_or_else(|| serde_json::json!(0)),
        "GET_ENTITY_ROUTING_BUCKET" => {
            serde_json::json!(shared_routing(state).entity_bucket(json_arg_netid(&args, 0)))
        }
        "GET_ENTITY_TYPE" => {
            let id = json_arg_i64(&args, 0) as u32;
            if let Some(entity) = shared_world(state).get(id) {
                serde_json::json!(entity.entity_type.as_native())
            } else {
                let ty = entity_field(id, "type").and_then(|v| v.as_str().map(str::to_owned));
                serde_json::json!(match ty.as_deref() {
                    Some("ped") => 1,
                    Some("vehicle") => 2,
                    Some("object") => 3,
                    _ => 0,
                })
            }
        }
        // The two natives that are both server state *and* a client mutation:
        // the synthetic store must record the new value for a script-created
        // handle, and a networked entity's owner must actually be told to move
        // it. Doing only one of the two would silently drop half the effect,
        // so both run (the RPC no-ops when no client owns the handle).
        "SET_ENTITY_COORDS" => {
            entity_update(
                json_arg_i64(&args, 0) as u32,
                "coords",
                serde_json::json!([
                    json_arg_f64(&args, 1),
                    json_arg_f64(&args, 2),
                    json_arg_f64(&args, 3),
                ]),
            );
            rpc::try_dispatch(state, &name, &args);
            serde_json::Value::Null
        }
        "SET_ENTITY_HEADING" => {
            entity_update(
                json_arg_i64(&args, 0) as u32,
                "heading",
                serde_json::json!(json_arg_f64(&args, 1)),
            );
            rpc::try_dispatch(state, &name, &args);
            serde_json::Value::Null
        }
        "SET_ENTITY_HEALTH" => {
            entity_update(
                json_arg_i64(&args, 0) as u32,
                "health",
                serde_json::json!(json_arg_i64(&args, 1)),
            );
            serde_json::Value::Null
        }
        "SET_ENTITY_ROUTING_BUCKET" => {
            let entity = json_arg_netid(&args, 0);
            let bucket = json_arg_netid(&args, 1);
            shared_routing(state).set_entity_bucket(entity, bucket);
            entity_update(entity, "routing_bucket", serde_json::json!(bucket));
            serde_json::Value::Null
        }
        // The owning client is what decides where a context-routed native is
        // dispatched, so this is the keystone of the whole entity surface.
        // `0` means "no owner known" (server-owned or unknown handle), which
        // is the same answer FXServer gives for an unowned entity.
        "NETWORK_GET_ENTITY_OWNER" => {
            serde_json::json!(shared_world(state)
                .owner(json_arg_i64(&args, 0) as u32)
                .unwrap_or(0))
        }
        "DROP_PLAYER" => {
            let source = json_arg_i64(&args, 0) as u32;
            state.borrow::<SharedPlayers>().0.remove(source);
            serde_json::Value::Null
        }
        "GET_HASH_KEY" => serde_json::json!(hash_rage_string(&json_arg_string(&args, 0))),
        "GET_HOST_ID" => serde_json::json!(""),
        "GET_NUM_PLAYER_INDICES" => {
            serde_json::json!(state.borrow::<SharedPlayers>().0.count() as u32)
        }
        "GET_PLAYER_INVINCIBLE" => serde_json::json!(false),
        "GET_PLAYER_ROUTING_BUCKET" => {
            serde_json::json!(shared_routing(state).player_bucket(json_arg_netid(&args, 0)))
        }
        "GET_PLAYER_LAST_MSG" => serde_json::json!(0),
        "GET_PLAYER_TIME_IN_PURSUIT" => serde_json::json!(0),
        "GET_PLAYER_WANTED_LEVEL" => serde_json::json!(0),
        "HAS_ENTITY_BEEN_MARKED_AS_NO_LONGER_NEEDED" => serde_json::json!(false),
        "IS_PLAYER_ACE_ALLOWED" => serde_json::json!(false),
        "IS_PLAYER_USING_SUPER_JUMP" => serde_json::json!(false),
        // Outbound HTTP. Both forms carry the same JSON request object; the
        // length argument of the non-Ex variant is redundant here because the
        // string already arrived whole. Returns the correlation token the
        // script matches its callback against, or 0 when the request could not
        // be queued at all.
        "PERFORM_HTTP_REQUEST_INTERNAL" | "PERFORM_HTTP_REQUEST_INTERNAL_EX" => {
            let resource = state.borrow::<RuntimeContext>().resource_name.clone();
            serde_json::json!(perform_http_request(
                state,
                &resource,
                &json_arg_string(&args, 0)
            ))
        }
        "PRINT_STRUCTURED_TRACE" => {
            tracing::info!(target: "structured-trace", "{}", json_arg_string(&args, 0));
            serde_json::Value::Null
        }
        // --- permanently unavailable, answered rather than reported ---
        //
        // The commerce natives query Rockstar's store through the CFX backend.
        // BASTON has no access to it and never will, so a definitive "no" is
        // the right answer: reporting these as unimplemented would keep
        // suggesting they are coming.
        "CAN_PLAYER_START_COMMERCE_SESSION"
        | "DOES_PLAYER_OWN_SKU"
        | "DOES_PLAYER_OWN_SKU_EXT"
        | "IS_PLAYER_COMMERCE_INFO_LOADED"
        | "IS_PLAYER_COMMERCE_INFO_LOADED_EXT" => serde_json::json!(false),
        "LOAD_PLAYER_COMMERCE_DATA"
        | "LOAD_PLAYER_COMMERCE_DATA_EXT"
        | "REQUEST_PLAYER_COMMERCE_SESSION" => serde_json::Value::Null,
        // Mounts are Red Dead only; on GTA5 the engine answers the same way.
        "GET_MOUNT" => serde_json::json!(0),
        "IS_PED_ON_MOUNT" => serde_json::json!(false),
        // The public `SetHttpHandler` keeps its callback JS-side and registers
        // through a dedicated op, so reaching the raw native means a script
        // invoked it by hash with a handle we cannot call.
        "SET_HTTP_HANDLER" => {
            tracing::warn!(
                target: "http",
                resource = %state.borrow::<RuntimeContext>().resource_name,
                "SET_HTTP_HANDLER invoked as a raw native; use the SetHttpHandler global instead"
            );
            serde_json::Value::Null
        }

        // --- console ---
        "GET_CONSOLE_BUFFER" => serde_json::json!(console_buffer_text()),
        // Console output already goes to the buffer above and to tracing; a
        // per-resource listener would need a JS callback across the op
        // boundary, which this runtime deliberately never does.
        "REGISTER_CONSOLE_LISTENER" => serde_json::Value::Null,

        // --- raw client events ---
        //
        // `TriggerClientEvent` goes through a dedicated op with JSON args; the
        // internal form exists for callers that packed their own msgpack, so
        // the payload is forwarded byte-for-byte instead of re-encoded.
        //
        // The latent variant additionally takes a bytes-per-second budget. The
        // event is delivered whole rather than paced: a resource using it gets
        // correct delivery and, for a large payload, a larger burst than it
        // asked for. Pacing needs a fragmenting sender the transport does not
        // have yet.
        "TRIGGER_CLIENT_EVENT_INTERNAL" | "TRIGGER_LATENT_CLIENT_EVENT_INTERNAL" => {
            let event = json_arg_string(&args, 0);
            let source = json_arg_netid(&args, 1);
            let payload = latin1_bytes(&json_arg_string(&args, 2));
            if name.starts_with("TRIGGER_LATENT") {
                tracing::debug!(
                    target: "events",
                    %event,
                    source,
                    bytes = payload.len(),
                    "latent client event sent whole; rate limiting is not implemented"
                );
            }
            send_raw_client_event(state, source, event, payload);
            serde_json::Value::Null
        }

        // --- moderation ---
        //
        // The engine records the ban in its own database; BASTON has none, so
        // the player is dropped and the ban is logged for whatever the
        // operator's own ban resource does with it. Saying so beats a silent
        // no-op that looks like a working ban.
        "TEMP_BAN_PLAYER" => {
            let source = json_arg_netid(&args, 0);
            let reason = json_arg_string(&args, 1);
            tracing::warn!(
                target: "moderation",
                source,
                %reason,
                "TempBanPlayer dropped the player; BASTON keeps no ban list, so \
                 the ban itself is the calling resource's to persist"
            );
            state.borrow::<SharedPlayers>().0.remove(source);
            serde_json::Value::Null
        }

        // --- password hashing ---
        //
        // bcrypt, as the engine uses, so hashes produced by either are
        // interchangeable. Both calls block the isolate for the cost factor's
        // duration by design: that cost is the whole point, and the engine's
        // natives are synchronous too.
        "GET_PASSWORD_HASH" => {
            match bcrypt::hash(json_arg_string(&args, 0), bcrypt::DEFAULT_COST) {
                Ok(hash) => serde_json::json!(hash),
                Err(e) => {
                    tracing::error!(target: "natives", error = %e, "password hashing failed");
                    serde_json::json!("")
                }
            }
        }
        "VERIFY_PASSWORD_HASH" => {
            // A malformed hash verifies as false rather than erroring: the
            // caller's question is "does this password match", and the answer
            // for a hash we cannot read is no.
            let matches = bcrypt::verify(json_arg_string(&args, 0), &json_arg_string(&args, 1))
                .unwrap_or(false);
            serde_json::json!(matches)
        }

        // --- resource lifecycle ---
        //
        // The transition is queued for the resource manager: it owns the
        // loading, and it is the caller of this very isolate, so doing it
        // inline would re-enter the host. The boolean says the request was
        // accepted, which is also all the engine's own return value means.
        "START_RESOURCE" => serde_json::json!(shared_resource_control(state)
            .submit(crate::ResourceCommand::Start(json_arg_string(&args, 0)))),
        "STOP_RESOURCE" => serde_json::json!(shared_resource_control(state)
            .submit(crate::ResourceCommand::Stop(json_arg_string(&args, 0)))),
        "SCAN_RESOURCE_ROOT" => serde_json::json!(shared_resource_control(state)
            .submit(crate::ResourceCommand::ScanRoot(json_arg_string(&args, 0)))),
        // Every resource is ticked every frame here, so there is nothing to
        // schedule. Answering silently is correct rather than a gap.
        "SCHEDULE_RESOURCE_TICK" => serde_json::Value::Null,
        // Build-time hooks with no server-side counterpart: BASTON serves
        // resource files straight off disk and runs no build tasks.
        "REGISTER_RESOURCE_ASSET" => serde_json::json!(""),
        "REGISTER_RESOURCE_BUILD_TASK_FACTORY" => serde_json::Value::Null,

        // --- server listing metadata ---
        //
        // These are convars in the engine too, and `/info.json` reads them
        // from the same place, so setting them here really does change what
        // the server list shows.
        "SET_GAME_TYPE" => {
            set_convar(state, "sv_gametype", json_arg_string(&args, 0));
            serde_json::Value::Null
        }
        "SET_MAP_NAME" => {
            set_convar(state, "sv_mapname", json_arg_string(&args, 0));
            serde_json::Value::Null
        }
        "FLAG_SERVER_AS_PRIVATE" => {
            set_convar(state, "sv_private", json_arg_bool(&args, 0).to_string());
            serde_json::Value::Null
        }
        "ENABLE_ENHANCED_HOST_SUPPORT" => {
            set_convar(
                state,
                "sv_enhancedHostSupport",
                json_arg_bool(&args, 0).to_string(),
            );
            serde_json::Value::Null
        }

        "SET_PLAYER_ROUTING_BUCKET" => {
            shared_routing(state)
                .set_player_bucket(json_arg_netid(&args, 0), json_arg_netid(&args, 1));
            serde_json::Value::Null
        }
        "SET_ROUTING_BUCKET_ENTITY_LOCKDOWN_MODE" => {
            let bucket = json_arg_netid(&args, 0);
            let mode = json_arg_string(&args, 1);
            if let Some(mode) = RoutingLockdownMode::parse(&mode) {
                shared_routing(state).set_lockdown_mode(bucket, mode);
            } else {
                tracing::warn!(
                    target: "routing-bucket",
                    bucket,
                    %mode,
                    "ignored invalid routing bucket lockdown mode"
                );
            }
            serde_json::Value::Null
        }
        "SET_ROUTING_BUCKET_POPULATION_ENABLED" => {
            shared_routing(state)
                .set_population_enabled(json_arg_netid(&args, 0), json_arg_bool(&args, 1));
            serde_json::Value::Null
        }
        // --- voice (baston-voice via the gateway's VoiceControl impl) ---
        "MUMBLE_CREATE_CHANNEL" => {
            if let Some(voice) = shared_voice(state) {
                voice.create_channel(json_arg_i64(&args, 0) as u32);
            }
            serde_json::Value::Null
        }
        "MUMBLE_DOES_CHANNEL_EXIST" => match shared_voice(state) {
            Some(voice) => {
                serde_json::json!(voice.channel_exists(json_arg_i64(&args, 0) as u32))
            }
            None => serde_json::json!(false),
        },
        "MUMBLE_SET_PLAYER_MUTED" => {
            if let Some(voice) = shared_voice(state) {
                voice.set_player_muted(json_arg_netid(&args, 0), json_arg_bool(&args, 1));
            }
            serde_json::Value::Null
        }
        "MUMBLE_IS_PLAYER_MUTED" => match shared_voice(state) {
            Some(voice) => serde_json::json!(voice.is_player_muted(json_arg_netid(&args, 0))),
            None => serde_json::json!(false),
        },
        "NETWORK_SET_VOICE_PROXIMITY_OVERRIDE_FOR_PLAYER" => {
            if let Some(voice) = shared_voice(state) {
                let pos = [
                    json_arg_f64(&args, 1) as f32,
                    json_arg_f64(&args, 2) as f32,
                    json_arg_f64(&args, 3) as f32,
                ];
                voice.set_proximity_override(json_arg_netid(&args, 0), Some(pos));
            }
            serde_json::Value::Null
        }
        "NETWORK_CLEAR_VOICE_PROXIMITY_OVERRIDE_FOR_PLAYER" => {
            if let Some(voice) = shared_voice(state) {
                voice.set_proximity_override(json_arg_netid(&args, 0), None);
            }
            serde_json::Value::Null
        }
        "NETWORK_GET_VOICE_PROXIMITY_OVERRIDE_FOR_PLAYER" => {
            let pos = shared_voice(state)
                .map(|voice| voice.proximity_override(json_arg_netid(&args, 0)))
                .unwrap_or([0.0; 3]);
            serde_json::json!([pos[0], pos[1], pos[2]])
        }
        _ => {
            // Readers backed by the decoded sync tree — the vehicle and ped
            // state family. Tried first: they answer from the server's own
            // reading of the world, so they must not be routed to a client.
            if let Some(value) = world::try_dispatch(state, &name, &args) {
                value
            }
            // Much of the "server" native surface is really a client mutation
            // the server routes to one client (see [`rpc`]). Try that
            // before declaring the native unimplemented: a native in the CFX
            // context table *is* implemented, it just executes elsewhere. Every
            // one of them returns void, hence the null.
            else if rpc::try_dispatch(state, &name, &args) {
                serde_json::Value::Null
            } else {
                let resource = state.borrow::<RuntimeContext>().resource_name.clone();
                unimplemented_native(&name, &result_kind, &resource)
            }
        }
    };

    value.to_string()
}

/// Queue an outbound HTTP request and return its token.
///
/// `0` means the request never left: either no worker is wired, the request
/// object was malformed, or the queue is saturated. Each case is logged, so a
/// callback that never fires has a cause in the log rather than being a
/// mystery.
fn perform_http_request(state: &NativeState, resource: &str, raw: &str) -> u32 {
    let Some(bridge) = state.borrow::<SharedHttp>().0.clone() else {
        unimplemented_native("PERFORM_HTTP_REQUEST_INTERNAL", "int", resource);
        return 0;
    };
    let Some(spec) = crate::parse_http_request(resource, raw) else {
        tracing::warn!(
            target: "http",
            resource,
            "PerformHttpRequest called with a malformed request object"
        );
        return 0;
    };
    bridge
        .submit(|token| spec.with_token(token))
        .unwrap_or_default()
}

/// Recover the bytes behind a JS binary string.
///
/// The internal event natives carry msgpack that JS holds as a string of code
/// points 0..255. Anything above that never came from a byte buffer, so it is
/// dropped rather than silently mangled into multi-byte UTF-8.
fn latin1_bytes(text: &str) -> Vec<u8> {
    text.chars()
        .filter_map(|c| u8::try_from(c as u32).ok())
        .collect()
}

/// Queue an already-encoded client event on the net bridge.
fn send_raw_client_event(state: &NativeState, source: u32, event: String, payload: Vec<u8>) {
    let net = &state.borrow::<super::SharedNet>().0;
    if net
        .tx
        .try_send(crate::net_bridge::NetOutbound::ClientEventRaw {
            source,
            event: event.clone(),
            payload,
        })
        .is_err()
    {
        tracing::warn!(
            target: "events",
            %event,
            source,
            "raw client event dropped: net bridge full or closed"
        );
    }
}

fn shared_resource_control(state: &NativeState) -> Arc<dyn crate::ResourceControl> {
    Arc::clone(&state.borrow::<SharedResourceControl>().0)
}

/// Write a console variable from a native. Same store `GetConvar` reads and
/// `/info.json` publishes, so a script setting one is immediately visible
/// everywhere the engine would make it visible.
fn set_convar(state: &NativeState, name: &str, value: String) {
    state
        .borrow::<SharedConvars>()
        .0
        .insert(name.to_owned(), value);
}

fn shared_world(state: &NativeState) -> Arc<crate::EntityWorldView> {
    Arc::clone(&state.borrow::<SharedEntityWorld>().0)
}

/// Create a real networked entity, or `None` when no authoritative world is
/// wired (OneSync off).
///
/// The network id is reserved synchronously — a script needs its handle back
/// from `CreateVehicle` immediately — while the entity itself is authored by
/// the game state on its next tick.
fn spawn_networked(
    state: &NativeState,
    entity_type: ScriptEntityType,
    model: u32,
    position: [f32; 3],
    heading: f32,
    dynamic: bool,
) -> Option<u32> {
    let control = &state.borrow::<SharedWorldControl>().0;
    let network_id = control.reserve_network_id()?;
    control.submit(crate::WorldCommand::Spawn {
        network_id,
        entity_type,
        model,
        position,
        heading,
        dynamic,
    });
    Some(network_id)
}

/// The handle a create native returns.
///
/// With an authoritative world, the entity is networked and its handle is its
/// network id. Without one, the entity stays a server-local record so scripts
/// that only read back what they created keep working.
fn create_entity_handle(
    state: &NativeState,
    entity_type: ScriptEntityType,
    model: u32,
    position: [f32; 3],
    heading: f32,
    dynamic: bool,
) -> u32 {
    spawn_networked(state, entity_type, model, position, heading, dynamic)
        .unwrap_or_else(next_synthetic_entity)
}

fn json_arg_position(args: &[serde_json::Value], first: usize) -> [f32; 3] {
    [
        json_arg_f64(args, first) as f32,
        json_arg_f64(args, first + 1) as f32,
        json_arg_f64(args, first + 2) as f32,
    ]
}

/// Union of the networked world and the synthetic (script-created) store.
///
/// The two are distinct populations: the world holds entities clients actually
/// simulate, the synthetic store holds handles a server script created that no
/// client owns yet. A script asking for "all vehicles" means both.
fn all_entities(
    state: &NativeState,
    networked: ScriptEntityType,
    synthetic: &str,
) -> serde_json::Value {
    let mut ids = shared_world(state).ids_of_type(networked);
    if let serde_json::Value::Array(extra) = entity_ids_by_type(synthetic) {
        ids.extend(extra.iter().filter_map(|v| v.as_u64().map(|id| id as u32)));
    }
    ids.sort_unstable();
    ids.dedup();
    serde_json::json!(ids)
}

fn entity_ids_by_type(entity_type: &str) -> serde_json::Value {
    let mut ids: Vec<_> = synthetic_entities()
        .iter()
        .filter_map(|entry| {
            (entry.value().get("type").and_then(|v| v.as_str()) == Some(entity_type))
                .then_some(*entry.key())
        })
        .collect();
    ids.sort_unstable();
    serde_json::json!(ids)
}

fn entity_field(id: u32, field: &str) -> Option<serde_json::Value> {
    synthetic_entities()
        .get(&id)
        .and_then(|entity| entity.get(field).cloned())
}

fn entity_update(id: u32, field: &str, value: serde_json::Value) {
    if let Some(mut entity) = synthetic_entities().get_mut(&id) {
        if let Some(object) = entity.as_object_mut() {
            object.insert(field.to_owned(), value);
        }
    }
}

/// The voice handle installed by the host, if any (voice enabled).
fn shared_voice(state: &NativeState) -> Option<Arc<dyn VoiceControl>> {
    state
        .try_borrow::<SharedVoice>()
        .and_then(|v| v.0.as_ref().map(Arc::clone))
}

fn shared_routing(state: &NativeState) -> Arc<dyn RoutingControl> {
    Arc::clone(&state.borrow::<SharedRouting>().0)
}

/// Natives already reported as unimplemented, so the warning fires once per
/// name instead of once per call — a script polling an unimplemented native in
/// a tick loop would otherwise drown the log.
fn reported_unimplemented() -> &'static DashMap<String, ()> {
    static REPORTED: OnceLock<DashMap<String, ()>> = OnceLock::new();
    REPORTED.get_or_init(DashMap::new)
}

/// Fallback for a native BASTON does not implement.
///
/// Returning a neutral value rather than throwing is deliberate: a resource
/// that calls one unimplemented native should degrade, not die. But a silent
/// neutral value is indistinguishable from a real answer, so every distinct
/// native is reported once and counted forever — an unimplemented native is
/// now a visible fact instead of a plausible zero.
fn unimplemented_native(name: &str, result_kind: &str, resource: &str) -> serde_json::Value {
    if reported_unimplemented()
        .insert(name.to_owned(), ())
        .is_none()
    {
        tracing::warn!(
            target: "natives",
            native = name,
            resource,
            returns = result_kind,
            "native is not implemented — returning a neutral value"
        );
    }
    metrics::counter!("script_native_unimplemented_total", "native" => name.to_owned())
        .increment(1);
    default_native_value(result_kind)
}

fn default_native_value(result_kind: &str) -> serde_json::Value {
    match result_kind.to_ascii_lowercase().as_str() {
        "void" => serde_json::Value::Null,
        "bool" | "boolean" => serde_json::json!(false),
        "char*" | "const char*" | "string" => serde_json::json!(""),
        "float" | "double" => serde_json::json!(0.0),
        "object" => serde_json::json!([]),
        // The JS side destructures a vector result (`const [x, y, z] = ...`).
        // Falling through to a scalar would make every unimplemented vector
        // getter a type error at the call site rather than a neutral answer.
        "vector3" => serde_json::json!([0.0, 0.0, 0.0]),
        "vector2" => serde_json::json!([0.0, 0.0]),
        "vector4" => serde_json::json!([0.0, 0.0, 0.0, 0.0]),
        _ => serde_json::json!(0),
    }
}

fn hash_rage_string(input: &str) -> u32 {
    let mut hash = 0u32;
    for byte in input.bytes() {
        hash = hash.wrapping_add(byte as u32);
        hash = hash.wrapping_add(hash << 10);
        hash ^= hash >> 6;
    }
    hash = hash.wrapping_add(hash << 3);
    hash ^= hash >> 11;
    hash.wrapping_add(hash << 15)
}
