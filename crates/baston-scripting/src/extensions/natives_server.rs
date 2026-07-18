//! `Citizen.invokeNative` server-side implementations: the shared CFX natives
//! (KVP, state bags, …) and the server-only natives backed by the synthetic
//! entity store, player directory, and voice control surface.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;

use dashmap::DashMap;
use deno_core::{op2, OpState};

use super::{RuntimeContext, SharedPlayers, SharedVoice, VoiceControl};

fn resource_kvp() -> &'static DashMap<String, String> {
    static KVP: OnceLock<DashMap<String, String>> = OnceLock::new();
    KVP.get_or_init(DashMap::new)
}

fn synthetic_entities() -> &'static DashMap<u32, serde_json::Value> {
    static ENTITIES: OnceLock<DashMap<u32, serde_json::Value>> = OnceLock::new();
    ENTITIES.get_or_init(DashMap::new)
}

fn next_synthetic_entity() -> u32 {
    static NEXT_ENTITY: AtomicU32 = AtomicU32::new(10_000);
    NEXT_ENTITY.fetch_add(1, Ordering::Relaxed)
}

fn kvp_key(resource: &str, key: &str) -> String {
    format!("{resource}\0{key}")
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

fn json_arg_i64(args: &[serde_json::Value], index: usize) -> i64 {
    args.get(index).and_then(|v| v.as_i64()).unwrap_or_default()
}

/// Player/net-id argument: natives pass server ids as numbers or numeric
/// strings (`source` is stringly-typed in much of the FiveM ecosystem).
fn json_arg_netid(args: &[serde_json::Value], index: usize) -> u32 {
    match args.get(index) {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or_default() as u32,
        Some(serde_json::Value::String(s)) => s.trim().parse().unwrap_or_default(),
        _ => 0,
    }
}

fn json_arg_bool(args: &[serde_json::Value], index: usize) -> bool {
    match args.get(index) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or_default() != 0,
        _ => false,
    }
}

#[op2]
#[string]
pub(super) fn op_cfx_shared_native(
    state: &mut OpState,
    #[string] name: String,
    #[string] args_json: String,
) -> String {
    let args = json_args(&args_json);
    let resource = state.borrow::<RuntimeContext>().resource_name.clone();
    let kvp = resource_kvp();

    let value = match name.as_str() {
        "ADD_CONVAR_CHANGE_LISTENER" | "ADD_STATE_BAG_CHANGE_HANDLER" => {
            // Callback dispatch is not wired yet; return a stable no-op cookie.
            serde_json::json!(0)
        }
        "CANCEL_EVENT"
        | "END_FIND_KVP"
        | "ENSURE_ENTITY_STATE_BAG"
        | "PROFILER_ENTER_SCOPE"
        | "PROFILER_EXIT_SCOPE"
        | "REGISTER_RESOURCE_AS_EVENT_HANDLER"
        | "REMOVE_CONVAR_CHANGE_LISTENER"
        | "REMOVE_STATE_BAG_CHANGE_HANDLER"
        | "TRIGGER_EVENT_INTERNAL" => serde_json::Value::Null,
        "DELETE_FUNCTION_REFERENCE" => serde_json::Value::Null,
        "DELETE_RESOURCE_KVP" | "DELETE_RESOURCE_KVP_NO_SYNC" => {
            kvp.remove(&kvp_key(&resource, &json_arg_string(&args, 0)));
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
            let prefix = json_arg_string(&args, 0);
            let mut keys: Vec<_> = kvp
                .iter()
                .filter_map(|entry| {
                    let (res, key) = entry.key().split_once('\0')?;
                    (res == resource.as_str() && key.starts_with(&prefix)).then(|| key.to_owned())
                })
                .collect();
            keys.sort();
            serde_json::json!(keys)
        }
        "FORMAT_STACK_TRACE" => serde_json::json!(json_arg_string(&args, 0)),
        "GET_ENTITIES_IN_RADIUS"
        | "GET_GAME_POOL"
        | "GET_REGISTERED_COMMANDS"
        | "GET_RESOURCE_COMMANDS"
        | "GET_STATE_BAG_KEYS" => serde_json::json!([]),
        "GET_ENTITY_FROM_STATE_BAG_NAME" => serde_json::json!(0),
        "GET_GAME_BUILD_NUMBER" => serde_json::json!(0),
        "GET_GAME_NAME" => serde_json::json!("gta5"),
        "GET_INSTANCE_ID" => serde_json::json!(0),
        "GET_INVOKING_RESOURCE" => serde_json::json!(resource),
        "GET_PLAYER_FROM_STATE_BAG_NAME" => serde_json::json!(0),
        "GET_RESOURCE_KVP_FLOAT" => kvp
            .get(&kvp_key(&resource, &json_arg_string(&args, 0)))
            .and_then(|v| v.parse::<f64>().ok())
            .map(serde_json::Value::from)
            .unwrap_or_else(|| serde_json::json!(0.0)),
        "GET_RESOURCE_KVP_INT" => kvp
            .get(&kvp_key(&resource, &json_arg_string(&args, 0)))
            .and_then(|v| v.parse::<i64>().ok())
            .map(serde_json::Value::from)
            .unwrap_or_else(|| serde_json::json!(0)),
        "GET_RESOURCE_KVP_STRING" => kvp
            .get(&kvp_key(&resource, &json_arg_string(&args, 0)))
            .map(|v| serde_json::json!(v.value().clone()))
            .unwrap_or(serde_json::Value::Null),
        "GET_STATE_BAG_VALUE" => serde_json::Value::Null,
        "IS_ACE_ALLOWED"
        | "IS_PRINCIPAL_ACE_ALLOWED"
        | "PROFILER_IS_RECORDING"
        | "STATE_BAG_HAS_KEY"
        | "WAS_EVENT_CANCELED" => serde_json::json!(false),
        "IS_DUPLICITY_VERSION" => serde_json::json!(true),
        "SET_RESOURCE_KVP" | "SET_RESOURCE_KVP_NO_SYNC" => {
            kvp.insert(
                kvp_key(&resource, &json_arg_string(&args, 0)),
                json_arg_string(&args, 1),
            );
            serde_json::Value::Null
        }
        "SET_RESOURCE_KVP_FLOAT" | "SET_RESOURCE_KVP_FLOAT_NO_SYNC" => {
            kvp.insert(
                kvp_key(&resource, &json_arg_string(&args, 0)),
                json_arg_f64(&args, 1).to_string(),
            );
            serde_json::Value::Null
        }
        "SET_RESOURCE_KVP_INT" | "SET_RESOURCE_KVP_INT_NO_SYNC" => {
            kvp.insert(
                kvp_key(&resource, &json_arg_string(&args, 0)),
                json_arg_i64(&args, 1).to_string(),
            );
            serde_json::Value::Null
        }
        "SET_STATE_BAG_VALUE" => serde_json::Value::Null,
        // Shared train/vehicle/entity accessors that need the future entity
        // state-bag bridge. Return neutral values instead of throwing so
        // resources can feature-detect safely.
        _ => serde_json::Value::Null,
    };

    value.to_string()
}

#[op2]
#[string]
pub(super) fn op_cfx_server_native(
    state: &mut OpState,
    #[string] name: String,
    #[string] result_kind: String,
    #[string] args_json: String,
) -> String {
    let args = json_args(&args_json);
    let entities = synthetic_entities();

    let value = match name.as_str() {
        "CREATE_OBJECT" | "CREATE_OBJECT_NO_OFFSET" => {
            let id = next_synthetic_entity();
            entities.insert(
                id,
                serde_json::json!({
                    "type": "object",
                    "model": json_arg_i64(&args, 0),
                    "coords": [json_arg_f64(&args, 1), json_arg_f64(&args, 2), json_arg_f64(&args, 3)],
                    "heading": 0.0,
                    "health": 1000,
                }),
            );
            serde_json::json!(id)
        }
        "CREATE_PED" => {
            let id = next_synthetic_entity();
            entities.insert(
                id,
                serde_json::json!({
                    "type": "ped",
                    "model": json_arg_i64(&args, 1),
                    "coords": [json_arg_f64(&args, 2), json_arg_f64(&args, 3), json_arg_f64(&args, 4)],
                    "heading": json_arg_f64(&args, 5),
                    "health": 200,
                }),
            );
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
                }),
            );
            serde_json::json!(id)
        }
        "CREATE_VEHICLE" | "CREATE_VEHICLE_SERVER_SETTER" => {
            let id = next_synthetic_entity();
            let coord_offset = if name == "CREATE_VEHICLE_SERVER_SETTER" {
                2
            } else {
                1
            };
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
                }),
            );
            serde_json::json!(id)
        }
        "DELETE_ENTITY" | "DELETE_TRAIN" => {
            entities.remove(&(json_arg_i64(&args, 0) as u32));
            serde_json::Value::Null
        }
        "DOES_ENTITY_EXIST" => {
            serde_json::json!(entities.contains_key(&(json_arg_i64(&args, 0) as u32)))
        }
        "GET_ALL_OBJECTS" => entity_ids_by_type("object"),
        "GET_ALL_PEDS" => entity_ids_by_type("ped"),
        "GET_ALL_VEHICLES" => entity_ids_by_type("vehicle"),
        "GET_ENTITY_COORDS" => entity_field(json_arg_i64(&args, 0) as u32, "coords")
            .unwrap_or_else(|| serde_json::json!([0.0, 0.0, 0.0])),
        "GET_ENTITY_HEADING" => entity_field(json_arg_i64(&args, 0) as u32, "heading")
            .unwrap_or_else(|| serde_json::json!(0.0)),
        "GET_ENTITY_HEALTH" => entity_field(json_arg_i64(&args, 0) as u32, "health")
            .unwrap_or_else(|| serde_json::json!(0)),
        "GET_ENTITY_MODEL" => entity_field(json_arg_i64(&args, 0) as u32, "model")
            .unwrap_or_else(|| serde_json::json!(0)),
        "GET_ENTITY_TYPE" => {
            let id = json_arg_i64(&args, 0) as u32;
            let ty = entity_field(id, "type").and_then(|v| v.as_str().map(str::to_owned));
            serde_json::json!(match ty.as_deref() {
                Some("ped") => 1,
                Some("vehicle") => 2,
                Some("object") => 3,
                _ => 0,
            })
        }
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
            serde_json::Value::Null
        }
        "SET_ENTITY_HEADING" => {
            entity_update(
                json_arg_i64(&args, 0) as u32,
                "heading",
                serde_json::json!(json_arg_f64(&args, 1)),
            );
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
        "NETWORK_GET_ENTITY_OWNER" => serde_json::json!(0),
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
        "GET_PLAYER_LAST_MSG" => serde_json::json!(0),
        "GET_PLAYER_TIME_IN_PURSUIT" => serde_json::json!(0),
        "GET_PLAYER_WANTED_LEVEL" => serde_json::json!(0),
        "GET_SELECTED_PED_WEAPON" | "GET_CURRENT_PED_WEAPON" => serde_json::json!(0),
        "HAS_ENTITY_BEEN_MARKED_AS_NO_LONGER_NEEDED" => serde_json::json!(false),
        "IS_PLAYER_ACE_ALLOWED" => serde_json::json!(false),
        "IS_PLAYER_USING_SUPER_JUMP" => serde_json::json!(false),
        "PRINT_STRUCTURED_TRACE" => {
            tracing::info!(target: "structured-trace", "{}", json_arg_string(&args, 0));
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
        _ => default_native_value(&result_kind),
    };

    value.to_string()
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
fn shared_voice(state: &OpState) -> Option<Arc<dyn VoiceControl>> {
    state
        .try_borrow::<SharedVoice>()
        .and_then(|v| v.0.as_ref().map(Arc::clone))
}

fn default_native_value(result_kind: &str) -> serde_json::Value {
    match result_kind.to_ascii_lowercase().as_str() {
        "void" => serde_json::Value::Null,
        "bool" | "boolean" => serde_json::json!(false),
        "char*" | "const char*" | "string" => serde_json::json!(""),
        "float" | "double" => serde_json::json!(0.0),
        "object" => serde_json::json!([]),
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
