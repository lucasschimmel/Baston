//! Natives answered from the decoded sync tree: the vehicle and ped readers.
//!
//! These all follow the same shape — look the entity up in the world mirror,
//! read one node's worth of state, map it to what the native returns — so they
//! live together rather than swelling [`super::natives_server`].
//!
//! ## Why they used to be missing
//!
//! Every one of them needs a node the server had to *decode*, not relay:
//! `CVehicleGameStateDataNode`, `CVehicleHealthDataNode`,
//! `CVehicleAppearanceDataNode`, `CVehicleDamageStatusDataNode` and
//! `CPedGameStateDataNode`. Until those decoders existed there was nothing to
//! answer with, and the natives fell into the unimplemented fallback.
//!
//! ## An entity with no state yet
//!
//! A vehicle that has not sent the relevant node — one a script just created,
//! or one whose owner has not synced it — has no answer to give. Each native
//! then returns the same neutral value the engine does, so a script sees "not
//! known yet" rather than a fabricated reading.

use deno_core::OpState;

use super::natives_server::{json_arg_bool, json_arg_i64, json_arg_netid};
use super::SharedEntityWorld;
use crate::entity_world::EntitySummary;
use baston_protocol::rage::sync_parse::{
    PedGameState, VehicleAppearance, VehicleDamage, VehicleGameState, VehicleHealth,
};

/// Try to answer a native from the world mirror.
///
/// `None` means "not one of ours" and the caller falls through to the rest of
/// its dispatch.
pub(super) fn try_dispatch(
    state: &OpState,
    name: &str,
    args: &[serde_json::Value],
) -> Option<serde_json::Value> {
    match name {
        // --- vehicle game state ---
        "GET_IS_VEHICLE_ENGINE_RUNNING" => Some(serde_json::json!(
            vehicle_state(state, args).is_some_and(|v| v.engine_on)
        )),
        "IS_VEHICLE_ENGINE_STARTING" => Some(serde_json::json!(
            vehicle_state(state, args).is_some_and(|v| v.engine_starting)
        )),
        "IS_VEHICLE_SIREN_ON" => Some(serde_json::json!(
            vehicle_state(state, args).is_some_and(|v| v.siren_on)
        )),
        "GET_VEHICLE_DOOR_LOCK_STATUS" => Some(serde_json::json!(
            vehicle_state(state, args).map_or(0, |v| u32::from(v.lock_status))
        )),
        // `-1` (every player) is the only value the engine reports as locked;
        // a per-player mask needs a player argument the native does not take.
        "GET_VEHICLE_DOORS_LOCKED_FOR_PLAYER" => Some(serde_json::json!(vehicle_state(
            state, args
        )
        .is_some_and(|v| v.has_lock && v.locked_players == -1))),
        "GET_VEHICLE_DOOR_STATUS" => {
            let door = json_arg_i64(args, 1);
            Some(serde_json::json!(vehicle_state(state, args)
                .filter(|v| v.doors_open != 0)
                .and_then(|v| usize::try_from(door)
                    .ok()
                    .and_then(|d| v.door_positions.get(d).copied()))
                .unwrap_or(0)))
        }
        "GET_VEHICLE_LIGHTS_STATE" => {
            let vehicle = vehicle_state(state, args);
            // The JS binding returns [retval, lightsOn, highbeamsOn]; the
            // engine's out-params become an array here.
            Some(serde_json::json!([
                vehicle.is_some(),
                vehicle.is_some_and(|v| v.lights_on),
                vehicle.is_some_and(|v| v.highbeams_on),
            ]))
        }
        // Divergence from FiveM, deliberate: upstream gates this read on
        // `defaultHeadlights`, which its own parser only sets when *no*
        // custom colour was sent — so the native there always answers 0.
        // Reporting the colour the vehicle actually sent is strictly more
        // information and cannot break a caller that expects 0.
        "GET_VEHICLE_HEADLIGHTS_COLOUR" => Some(serde_json::json!(vehicle_state(state, args)
            .filter(|v| !v.default_headlights)
            .map_or(0, |v| u32::from(v.headlights_colour)))),
        "GET_VEHICLE_RADIO_STATION_INDEX" => Some(serde_json::json!(
            vehicle_state(state, args).map_or(0, |v| u32::from(v.radio_station))
        )),
        "HAS_VEHICLE_BEEN_OWNED_BY_PLAYER" => Some(serde_json::json!(
            vehicle_state(state, args).is_some_and(|v| v.has_been_owned_by_player)
        )),

        // --- vehicle health ---
        "GET_VEHICLE_BODY_HEALTH" => Some(serde_json::json!(
            vehicle_health(state, args).map_or(0.0, |v| v.body_health as f64)
        )),
        "GET_VEHICLE_ENGINE_HEALTH" => Some(serde_json::json!(
            vehicle_health(state, args).map_or(0.0, |v| v.engine_health as f64)
        )),
        "GET_VEHICLE_PETROL_TANK_HEALTH" => Some(serde_json::json!(
            vehicle_health(state, args).map_or(0.0, |v| v.petrol_tank_health as f64)
        )),
        "GET_VEHICLE_TOTAL_REPAIRS" => Some(serde_json::json!(
            vehicle_health(state, args).map_or(0, |v| u32::from(v.total_repairs))
        )),
        "IS_VEHICLE_TYRE_BURST" => {
            let tyre = json_arg_i64(args, 1);
            let completely = json_arg_bool(args, 2);
            Some(serde_json::json!(vehicle_health(state, args)
                .filter(|v| !v.tyres_fine)
                .and_then(|v| usize::try_from(tyre)
                    .ok()
                    .and_then(|t| v.tyre_status.get(t).copied()))
                // 2 is "running on the rim", 1 is "burst but still there".
                .is_some_and(|status| if completely {
                    status == 2
                } else {
                    status == 1
                })))
        }

        // --- vehicle appearance ---
        "GET_VEHICLE_COLOURS" => Some(serde_json::json!(
            appearance(state, args).map_or([0, 0], |a| [a.primary_colour, a.secondary_colour])
        )),
        "GET_VEHICLE_EXTRA_COLOURS" => Some(serde_json::json!(
            appearance(state, args).map_or([0, 0], |a| [a.pearl_colour, a.wheel_colour])
        )),
        "GET_VEHICLE_INTERIOR_COLOUR" => Some(serde_json::json!(
            appearance(state, args).map_or(0, |a| u32::from(a.interior_colour))
        )),
        "GET_VEHICLE_DASHBOARD_COLOUR" => Some(serde_json::json!(
            appearance(state, args).map_or(0, |a| u32::from(a.dashboard_colour))
        )),
        "GET_IS_VEHICLE_PRIMARY_COLOUR_CUSTOM" => Some(serde_json::json!(
            appearance(state, args).is_some_and(|a| a.is_primary_colour_rgb)
        )),
        "GET_IS_VEHICLE_SECONDARY_COLOUR_CUSTOM" => Some(serde_json::json!(appearance(
            state, args
        )
        .is_some_and(|a| a.is_secondary_colour_rgb))),
        "GET_VEHICLE_CUSTOM_PRIMARY_COLOUR" => Some(serde_json::json!(
            appearance(state, args).map_or([0; 3], |a| a.primary_rgb)
        )),
        "GET_VEHICLE_CUSTOM_SECONDARY_COLOUR" => Some(serde_json::json!(
            appearance(state, args).map_or([0; 3], |a| a.secondary_rgb)
        )),
        "GET_VEHICLE_DIRT_LEVEL" => Some(serde_json::json!(
            appearance(state, args).map_or(0.0, |a| f64::from(a.dirt_level))
        )),
        "GET_VEHICLE_LIVERY" => Some(serde_json::json!(
            appearance(state, args).map_or(0, |a| i32::from(a.livery_index))
        )),
        "GET_VEHICLE_ROOF_LIVERY" => Some(serde_json::json!(
            appearance(state, args).map_or(0, |a| i32::from(a.roof_livery_index))
        )),
        "GET_VEHICLE_WHEEL_TYPE" => Some(serde_json::json!(
            appearance(state, args).map_or(0, |a| u32::from(a.wheel_type))
        )),
        // 255 is the engine's "untinted" marker and surfaces as -1.
        "GET_VEHICLE_WINDOW_TINT" => {
            Some(serde_json::json!(appearance(state, args).map_or(0, |a| {
                if a.window_tint_index == 255 {
                    -1
                } else {
                    i32::from(a.window_tint_index)
                }
            })))
        }
        "GET_VEHICLE_TYRE_SMOKE_COLOR" => Some(serde_json::json!(
            appearance(state, args).map_or([0; 3], |a| a.tyre_smoke_colour)
        )),
        "GET_VEHICLE_NUMBER_PLATE_TEXT" => Some(serde_json::json!(
            appearance(state, args).map_or(String::new(), |a| a.plate_text())
        )),
        "GET_VEHICLE_NUMBER_PLATE_TEXT_INDEX" => Some(serde_json::json!(
            appearance(state, args).map_or(0, |a| a.number_plate_text_index)
        )),
        "GET_VEHICLE_HORN_TYPE" => Some(serde_json::json!(
            appearance(state, args).map_or(0, |a| a.horn_type_hash)
        )),
        "GET_VEHICLE_NEON_COLOUR" => Some(serde_json::json!(
            appearance(state, args).map_or([0; 3], |a| a.neon_colour)
        )),
        // The neon index is the native's own first argument (0 = left, 1 =
        // right, 2 = front, 3 = back). Upstream reads argument 0 here, which
        // is the entity handle — a bug no caller can be relying on.
        "GET_VEHICLE_NEON_ENABLED" => {
            let side = json_arg_i64(args, 1);
            Some(serde_json::json!(appearance(state, args)
                .filter(|a| a.has_neon_lights)
                .and_then(|a| usize::try_from(side)
                    .ok()
                    .and_then(|s| a.neon_sides.get(s).copied()))
                .unwrap_or(false)))
        }
        // The mask is inverted on the wire: a set bit turns the extra *off*.
        "IS_VEHICLE_EXTRA_TURNED_ON" => {
            let extra = json_arg_i64(args, 1);
            Some(serde_json::json!(appearance(state, args).is_some_and(
                |a| {
                    match u32::try_from(extra + 1).ok().filter(|shift| *shift < 16) {
                        Some(shift) => (1u16 << shift) & a.extras == 0,
                        None => false,
                    }
                }
            )))
        }

        // --- vehicle damage ---
        "HAS_VEHICLE_BEEN_DAMAGED_BY_BULLETS" => Some(serde_json::json!(
            damage(state, args).is_some_and(|d| d.damaged_by_bullets)
        )),
        "IS_VEHICLE_WINDOW_INTACT" => {
            let window = json_arg_i64(args, 1);
            Some(serde_json::json!(damage(state, args).is_some_and(|d| {
                match usize::try_from(window)
                    .ok()
                    .and_then(|w| d.windows_broken.get(w).copied())
                {
                    // No window was broken at all, so this one is intact.
                    Some(_) if !d.any_window_broken => true,
                    Some(broken) => !broken,
                    None => false,
                }
            })))
        }

        // --- ped occupancy ---
        "GET_VEHICLE_PED_IS_IN" => {
            let want_last = json_arg_bool(args, 1);
            let summary = entity(state, args)?;
            let current = summary
                .sync
                .ped_game_state
                .filter(|p| p.cur_vehicle >= 0)
                .map(|p| p.cur_vehicle as u32);
            Some(serde_json::json!(if want_last {
                summary.sync.last_vehicle.map(u32::from).or(current)
            } else {
                current
            }
            .unwrap_or(0)))
        }
        "IS_PED_IN_ANY_VEHICLE" => Some(serde_json::json!(
            ped_state(state, args).is_some_and(|p| p.cur_vehicle >= 0)
        )),
        "IS_PED_IN_VEHICLE" => {
            let vehicle = json_arg_netid(args, 1) as i32;
            Some(serde_json::json!(ped_state(state, args)
                .is_some_and(
                    |p| p.cur_vehicle >= 0 && p.cur_vehicle == vehicle
                )))
        }
        "GET_SEAT_PED_IS_USING" => Some(serde_json::json!(
            ped_state(state, args).map_or(-1, |p| script_seat(p.cur_vehicle_seat))
        )),
        "GET_PED_IN_VEHICLE_SEAT" => {
            let vehicle = json_arg_netid(args, 0);
            let seat = json_arg_i64(args, 1) as i32;
            Some(serde_json::json!(world(state)
                .occupant(vehicle, seat)
                .unwrap_or(0)))
        }
        "GET_LAST_PED_IN_VEHICLE_SEAT" => {
            let vehicle = json_arg_netid(args, 0);
            let seat = json_arg_i64(args, 1) as i32;
            Some(serde_json::json!(world(state)
                .last_occupant(vehicle, seat)
                .unwrap_or(0)))
        }

        // --- ped state ---
        "GET_SELECTED_PED_WEAPON" | "GET_CURRENT_PED_WEAPON" => Some(serde_json::json!(ped_state(
            state, args
        )
        .map_or(0, |p| p.cur_weapon))),
        "IS_PED_HANDCUFFED" => Some(serde_json::json!(
            ped_state(state, args).is_some_and(|p| p.is_handcuffed)
        )),
        "IS_FLASH_LIGHT_ON" => Some(serde_json::json!(
            ped_state(state, args).is_some_and(|p| p.is_flashlight_on)
        )),
        "IS_PED_USING_ACTION_MODE" => Some(serde_json::json!(
            ped_state(state, args).is_some_and(|p| p.action_mode_enabled)
        )),
        "GET_PED_STEALTH_MOVEMENT" => Some(serde_json::json!(
            ped_state(state, args).is_some_and(|p| p.stealth_mode_enabled)
        )),
        "IS_PED_A_PLAYER" => {
            let handle = json_arg_netid(args, 0);
            let world = world(state);
            Some(serde_json::json!(world.get(handle).is_some_and(
                |summary| world.player_ped(summary.owner) == Some(handle)
            )))
        }
        "GET_PED_ARMOUR" => Some(serde_json::json!(entity(state, args)
            .and_then(|e| e.armour)
            .unwrap_or(0.0))),
        "GET_PED_MAX_HEALTH" | "GET_ENTITY_MAX_HEALTH" => {
            Some(serde_json::json!(entity(state, args)
                .and_then(|e| e.max_health)
                .unwrap_or(0.0)))
        }

        // --- entity kinematics ---
        "GET_ENTITY_SPEED" => Some(serde_json::json!(entity(state, args).map_or(0.0, |e| {
            let [x, y, z] = e.velocity;
            f64::from(x.mul_add(x, y.mul_add(y, z * z)).sqrt())
        }))),
        // The sync tree carries a full orientation, but only the heading
        // survives the compressed quaternion we decode, so pitch and roll are
        // reported as zero rather than invented.
        "GET_ENTITY_ROTATION" => Some(serde_json::json!([
            0.0,
            0.0,
            entity(state, args).and_then(|e| e.heading).unwrap_or(0.0)
        ])),
        _ => None,
    }
}

fn world(state: &OpState) -> std::sync::Arc<crate::EntityWorldView> {
    std::sync::Arc::clone(&state.borrow::<SharedEntityWorld>().0)
}

fn entity(state: &OpState, args: &[serde_json::Value]) -> Option<EntitySummary> {
    world(state).get(json_arg_netid(args, 0))
}

fn vehicle_state(state: &OpState, args: &[serde_json::Value]) -> Option<VehicleGameState> {
    entity(state, args)?.sync.vehicle_game_state
}

fn vehicle_health(state: &OpState, args: &[serde_json::Value]) -> Option<VehicleHealth> {
    entity(state, args)?.sync.vehicle_health
}

fn appearance(state: &OpState, args: &[serde_json::Value]) -> Option<VehicleAppearance> {
    entity(state, args)?.sync.vehicle_appearance
}

fn damage(state: &OpState, args: &[serde_json::Value]) -> Option<VehicleDamage> {
    entity(state, args)?.sync.vehicle_damage
}

fn ped_state(state: &OpState, args: &[serde_json::Value]) -> Option<PedGameState> {
    entity(state, args)?.sync.ped_game_state
}

/// Wire seats are biased by two so that "entering" and "none" fit an unsigned
/// field; scripts address the driver as `-1`.
fn script_seat(raw_seat: i32) -> i32 {
    raw_seat - crate::entity_world::SEAT_INDEX_BIAS
}
