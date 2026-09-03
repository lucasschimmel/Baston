---
title: "Server native coverage"
description: "Which CFX server natives BASTON implements, and which it does not."
---

Generated: 2026-07-09

Sources:
- Official Cfx.re native reference UI: https://docs.fivem.net/natives/
- Official machine-readable CFX native catalog: https://runtime.fivem.net/doc/natives_cfx.json
- Checked `https://runtime.fivem.net/doc/natives.json` as well: it contains GTA natives and no usable server/shared API-set split for this task.

Comparison rule:
- Official scope: `apiset in {server, shared}` from `natives_cfx.json`.
- Considered already present in Baston if the native name exists as a public JS global in `crates/baston-scripting/assets/bootstrap.js` or as a validated entry in `crates/baston-protocol/src/native.rs`.
- Name comparison is normalized (`GET_CONVAR` == `GetConvar`).

Implementation note:
- Many server natives now route through the generated compatibility shim in `bootstrap.js` and `op_cfx_server_native`; world-state-backed behavior is implemented for the synthetic entity paths, while unsupported subsystems return typed neutral values instead of throwing.

:::danger[This table counts reachable names, not implementations]
The rule above asks whether a name is *callable*. Every name is, because
`bootstrap.js` generates a global for each one and unknown names fall through to
a typed neutral value. So a native that answers `nil` for every argument counts
here exactly like one that reads real state, and "Remaining to implement: 0"
means "nothing throws", not "everything works".

It also only looks at the **JS** surface. `GET_PLAYER_IDENTIFIER` and its
neighbours are listed below as present and were, for JavaScript — the ops were
wired for V8 only. In Lua the same names fell through to the neutral value, so
`GetPlayerIdentifiers` returned `nil` and every connecting player was rejected
by `cfx-server-data`'s `player-data`. The table said 360/360 throughout.

Read a row as "this name will not crash a script". To know whether a native
does its job, read the code or a test.
:::

Official server/shared natives: 360 (286 server, 74 shared).
Reachable in Baston under this rule: 360.
Genuinely implemented: not measured by this document.

## Already present in Baston

| API set | Native | Baston symbol | Kind |
|---|---|---|---|
| server | `ADD_BLIP_FOR_COORD` | `AddBlipForCoord` | public JS global |
| server | `ADD_BLIP_FOR_ENTITY` | `AddBlipForEntity` | public JS global |
| server | `ADD_BLIP_FOR_RADIUS` | `AddBlipForRadius` | public JS global |
| server | `ADD_PED_DECORATION_FROM_HASHES` | `AddPedDecorationFromHashes` | public JS global |
| server | `APPLY_FORCE_TO_ENTITY` | `ApplyForceToEntity` | public JS global |
| server | `CAN_PLAYER_START_COMMERCE_SESSION` | `CanPlayerStartCommerceSession` | public JS global |
| server | `CLEAR_PED_PROP` | `ClearPedProp` | public JS global |
| server | `CLEAR_PED_SECONDARY_TASK` | `ClearPedSecondaryTask` | public JS global |
| server | `CLEAR_PED_TASKS` | `ClearPedTasks` | public JS global |
| server | `CLEAR_PED_TASKS_IMMEDIATELY` | `ClearPedTasksImmediately` | public JS global |
| server | `CLEAR_PLAYER_WANTED_LEVEL` | `ClearPlayerWantedLevel` | public JS global |
| server | `CREATE_OBJECT` | `CreateObject` | public JS global |
| server | `CREATE_OBJECT_NO_OFFSET` | `CreateObjectNoOffset` | public JS global |
| server | `CREATE_PED` | `CreatePed` | public JS global |
| server | `CREATE_PED_INSIDE_VEHICLE` | `CreatePedInsideVehicle` | public JS global |
| server | `CREATE_VEHICLE` | `CreateVehicle` | public JS global |
| server | `CREATE_VEHICLE_SERVER_SETTER` | `CreateVehicleServerSetter` | public JS global |
| server | `DELETE_ENTITY` | `DeleteEntity` | public JS global |
| server | `DELETE_TRAIN` | `DeleteTrain` | public JS global |
| server | `DOES_BOAT_SINK_WHEN_WRECKED` | `DoesBoatSinkWhenWrecked` | public JS global |
| server | `DOES_ENTITY_EXIST` | `DoesEntityExist` | public JS global |
| server | `DOES_PLAYER_EXIST` | `DoesPlayerExist` | public JS global |
| server | `DOES_PLAYER_OWN_SKU` | `DoesPlayerOwnSku` | public JS global |
| server | `DOES_PLAYER_OWN_SKU_EXT` | `DoesPlayerOwnSkuExt` | public JS global |
| server | `DROP_PLAYER` | `DropPlayer` | public JS global |
| server | `ENABLE_ENHANCED_HOST_SUPPORT` | `EnableEnhancedHostSupport` | public JS global |
| server | `FLAG_SERVER_AS_PRIVATE` | `FlagServerAsPrivate` | public JS global |
| server | `FLUSH_RESOURCE_KVP` | `FlushResourceKvp` | public JS global |
| server | `FREEZE_ENTITY_POSITION` | `FreezeEntityPosition` | public JS global |
| server | `GET_AIR_DRAG_MULTIPLIER_FOR_PLAYERS_VEHICLE` | `GetAirDragMultiplierForPlayersVehicle` | public JS global |
| server | `GET_ALL_OBJECTS` | `GetAllObjects` | public JS global |
| server | `GET_ALL_PEDS` | `GetAllPeds` | public JS global |
| server | `GET_ALL_VEHICLES` | `GetAllVehicles` | public JS global |
| server | `GET_CONSOLE_BUFFER` | `GetConsoleBuffer` | public JS global |
| server | `GET_CURRENT_PED_WEAPON` | `GetCurrentPedWeapon` | public JS global |
| server | `GET_ENTITY_ATTACHED_TO` | `GetEntityAttachedTo` | public JS global |
| server | `GET_ENTITY_COLLISION_DISABLED` | `GetEntityCollisionDisabled` | public JS global |
| server | `GET_ENTITY_COORDS` | `GetEntityCoords` | public JS global |
| server | `GET_ENTITY_HEADING` | `GetEntityHeading` | public JS global |
| server | `GET_ENTITY_HEALTH` | `GetEntityHealth` | public JS global |
| server | `GET_ENTITY_MAX_HEALTH` | `GetEntityMaxHealth` | public JS global |
| server | `GET_ENTITY_MODEL` | `GetEntityModel` | public JS global |
| server | `GET_ENTITY_ORPHAN_MODE` | `GetEntityOrphanMode` | public JS global |
| server | `GET_ENTITY_POPULATION_TYPE` | `GetEntityPopulationType` | public JS global |
| server | `GET_ENTITY_REMOTE_SYNCED_SCENES_ALLOWED` | `GetEntityRemoteSyncedScenesAllowed` | public JS global |
| server | `GET_ENTITY_ROTATION` | `GetEntityRotation` | public JS global |
| server | `GET_ENTITY_ROTATION_VELOCITY` | `GetEntityRotationVelocity` | public JS global |
| server | `GET_ENTITY_ROUTING_BUCKET` | `GetEntityRoutingBucket` | public JS global |
| server | `GET_ENTITY_SCRIPT` | `GetEntityScript` | public JS global |
| server | `GET_ENTITY_SPEED` | `GetEntitySpeed` | public JS global |
| server | `GET_ENTITY_TYPE` | `GetEntityType` | public JS global |
| server | `GET_ENTITY_VELOCITY` | `GetEntityVelocity` | public JS global |
| server | `GET_GAME_TIMER` | `GetGameTimer` | public JS global |
| server | `GET_HASH_KEY` | `GetHashKey` | public JS global |
| server | `GET_HELI_BODY_HEALTH` | `GetHeliBodyHealth` | public JS global |
| server | `GET_HELI_DISABLE_EXPLODE_FROM_BODY_DAMAGE` | `GetHeliDisableExplodeFromBodyDamage` | public JS global |
| server | `GET_HELI_ENGINE_HEALTH` | `GetHeliEngineHealth` | public JS global |
| server | `GET_HELI_GAS_TANK_HEALTH` | `GetHeliGasTankHealth` | public JS global |
| server | `GET_HELI_MAIN_ROTOR_DAMAGE_SCALE` | `GetHeliMainRotorDamageScale` | public JS global |
| server | `GET_HELI_MAIN_ROTOR_HEALTH` | `GetHeliMainRotorHealth` | public JS global |
| server | `GET_HELI_PITCH_CONTROL` | `GetHeliPitchControl` | public JS global |
| server | `GET_HELI_REAR_ROTOR_DAMAGE_SCALE` | `GetHeliRearRotorDamageScale` | public JS global |
| server | `GET_HELI_REAR_ROTOR_HEALTH` | `GetHeliRearRotorHealth` | public JS global |
| server | `GET_HELI_ROLL_CONTROL` | `GetHeliRollControl` | public JS global |
| server | `GET_HELI_TAIL_ROTOR_DAMAGE_SCALE` | `GetHeliTailRotorDamageScale` | public JS global |
| server | `GET_HELI_TAIL_ROTOR_HEALTH` | `GetHeliTailRotorHealth` | public JS global |
| server | `GET_HELI_THROTTLE_CONTROL` | `GetHeliThrottleControl` | public JS global |
| server | `GET_HELI_YAW_CONTROL` | `GetHeliYawControl` | public JS global |
| server | `GET_HOST_ID` | `GetHostId` | public JS global |
| server | `GET_IS_HELI_ENGINE_RUNNING` | `GetIsHeliEngineRunning` | public JS global |
| server | `GET_IS_VEHICLE_ENGINE_RUNNING` | `GetIsVehicleEngineRunning` | public JS global |
| server | `GET_IS_VEHICLE_PRIMARY_COLOUR_CUSTOM` | `GetIsVehiclePrimaryColourCustom` | public JS global |
| server | `GET_IS_VEHICLE_SECONDARY_COLOUR_CUSTOM` | `GetIsVehicleSecondaryColourCustom` | public JS global |
| server | `GET_LANDING_GEAR_STATE` | `GetLandingGearState` | public JS global |
| server | `GET_LAST_PED_IN_VEHICLE_SEAT` | `GetLastPedInVehicleSeat` | public JS global |
| server | `GET_MOUNT` | `GetMount` | public JS global |
| server | `GET_NET_TYPE_FROM_ENTITY` | `GetNetTypeFromEntity` | public JS global |
| server | `GET_NUM_PLAYER_IDENTIFIERS` | `GetNumPlayerIdentifiers` | public JS global |
| server | `GET_NUM_PLAYER_INDICES` | `GetNumPlayerIndices` | public JS global |
| server | `GET_NUM_PLAYER_TOKENS` | `GetNumPlayerTokens` | public JS global |
| server | `GET_PASSWORD_HASH` | `GetPasswordHash` | public JS global |
| server | `GET_PED_ARMOUR` | `GetPedArmour` | public JS global |
| server | `GET_PED_CAUSE_OF_DEATH` | `GetPedCauseOfDeath` | public JS global |
| server | `GET_PED_DESIRED_HEADING` | `GetPedDesiredHeading` | public JS global |
| server | `GET_PED_IN_VEHICLE_SEAT` | `GetPedInVehicleSeat` | public JS global |
| server | `GET_PED_MAX_HEALTH` | `GetPedMaxHealth` | public JS global |
| server | `GET_PED_RELATIONSHIP_GROUP_HASH` | `GetPedRelationshipGroupHash` | public JS global |
| server | `GET_PED_SCRIPT_TASK_COMMAND` | `GetPedScriptTaskCommand` | public JS global |
| server | `GET_PED_SCRIPT_TASK_STAGE` | `GetPedScriptTaskStage` | public JS global |
| server | `GET_PED_SOURCE_OF_DAMAGE` | `GetPedSourceOfDamage` | public JS global |
| server | `GET_PED_SOURCE_OF_DEATH` | `GetPedSourceOfDeath` | public JS global |
| server | `GET_PED_SPECIFIC_TASK_TYPE` | `GetPedSpecificTaskType` | public JS global |
| server | `GET_PED_STEALTH_MOVEMENT` | `GetPedStealthMovement` | public JS global |
| server | `GET_PLAYER_CAMERA_ROTATION` | `GetPlayerCameraRotation` | public JS global |
| server | `GET_PLAYER_ENDPOINT` | `GetPlayerEndpoint` | public JS global |
| server | `GET_PLAYER_FAKE_WANTED_LEVEL` | `GetPlayerFakeWantedLevel` | public JS global |
| server | `GET_PLAYER_FOCUS_POS` | `GetPlayerFocusPos` | public JS global |
| server | `GET_PLAYER_FROM_INDEX` | `GetPlayerFromIndex` | public JS global |
| server | `GET_PLAYER_GUID` | `GetPlayerGuid` | public JS global |
| server | `GET_PLAYER_IDENTIFIER` | `GetPlayerIdentifier` | public JS global |
| server | `GET_PLAYER_IDENTIFIER_BY_TYPE` | `GetPlayerIdentifierByType` | public JS global |
| server | `GET_PLAYER_INVINCIBLE` | `GetPlayerInvincible` | public JS global |
| server | `GET_PLAYER_LAST_MSG` | `GetPlayerLastMsg` | public JS global |
| server | `GET_PLAYER_MAX_ARMOUR` | `GetPlayerMaxArmour` | public JS global |
| server | `GET_PLAYER_MAX_HEALTH` | `GetPlayerMaxHealth` | public JS global |
| server | `GET_PLAYER_NAME` | `GetPlayerName` | public JS global |
| server | `GET_PLAYER_PED` | `GetPlayerPed` | public JS global |
| server | `GET_PLAYER_PEER_STATISTICS` | `GetPlayerPeerStatistics` | public JS global |
| server | `GET_PLAYER_PING` | `GetPlayerPing` | public JS global |
| server | `GET_PLAYER_ROUTING_BUCKET` | `GetPlayerRoutingBucket` | public JS global |
| server | `GET_PLAYER_TEAM` | `GetPlayerTeam` | public JS global |
| server | `GET_PLAYER_TIME_IN_PURSUIT` | `GetPlayerTimeInPursuit` | public JS global |
| server | `GET_PLAYER_TIME_ONLINE` | `GetPlayerTimeOnline` | public JS global |
| server | `GET_PLAYER_TOKEN` | `GetPlayerToken` | public JS global |
| server | `GET_PLAYER_WANTED_CENTRE_POSITION` | `GetPlayerWantedCentrePosition` | public JS global |
| server | `GET_PLAYER_WANTED_LEVEL` | `GetPlayerWantedLevel` | public JS global |
| server | `GET_RESOURCE_PATH` | `GetResourcePath` | public JS global |
| server | `GET_SEAT_PED_IS_USING` | `GetSeatPedIsUsing` | public JS global |
| server | `GET_SELECTED_PED_WEAPON` | `GetSelectedPedWeapon` | public JS global |
| server | `GET_THRUSTER_SIDE_RCS_THROTTLE` | `GetThrusterSideRcsThrottle` | public JS global |
| server | `GET_THRUSTER_THROTTLE` | `GetThrusterThrottle` | public JS global |
| server | `GET_TRAIN_BACKWARD_CARRIAGE` | `GetTrainBackwardCarriage` | public JS global |
| server | `GET_TRAIN_CARRIAGE_ENGINE` | `GetTrainCarriageEngine` | public JS global |
| server | `GET_TRAIN_CARRIAGE_INDEX` | `GetTrainCarriageIndex` | public JS global |
| server | `GET_TRAIN_FORWARD_CARRIAGE` | `GetTrainForwardCarriage` | public JS global |
| server | `GET_VEHICLE_BODY_HEALTH` | `GetVehicleBodyHealth` | public JS global |
| server | `GET_VEHICLE_COLOURS` | `GetVehicleColours` | public JS global |
| server | `GET_VEHICLE_CUSTOM_PRIMARY_COLOUR` | `GetVehicleCustomPrimaryColour` | public JS global |
| server | `GET_VEHICLE_CUSTOM_SECONDARY_COLOUR` | `GetVehicleCustomSecondaryColour` | public JS global |
| server | `GET_VEHICLE_DASHBOARD_COLOUR` | `GetVehicleDashboardColour` | public JS global |
| server | `GET_VEHICLE_DIRT_LEVEL` | `GetVehicleDirtLevel` | public JS global |
| server | `GET_VEHICLE_DOORS_LOCKED_FOR_PLAYER` | `GetVehicleDoorsLockedForPlayer` | public JS global |
| server | `GET_VEHICLE_DOOR_LOCK_STATUS` | `GetVehicleDoorLockStatus` | public JS global |
| server | `GET_VEHICLE_DOOR_STATUS` | `GetVehicleDoorStatus` | public JS global |
| server | `GET_VEHICLE_ENGINE_HEALTH` | `GetVehicleEngineHealth` | public JS global |
| server | `GET_VEHICLE_EXTRA_COLOURS` | `GetVehicleExtraColours` | public JS global |
| server | `GET_VEHICLE_FLIGHT_NOZZLE_POSITION` | `GetVehicleFlightNozzlePosition` | public JS global |
| server | `GET_VEHICLE_HEADLIGHTS_COLOUR` | `GetVehicleHeadlightsColour` | public JS global |
| server | `GET_VEHICLE_HOMING_LOCKON_STATE` | `GetVehicleHomingLockonState` | public JS global |
| server | `GET_VEHICLE_HORN_TYPE` | `GetVehicleHornType` | public JS global |
| server | `GET_VEHICLE_INTERIOR_COLOUR` | `GetVehicleInteriorColour` | public JS global |
| server | `GET_VEHICLE_LIGHTS_STATE` | `GetVehicleLightsState` | public JS global |
| server | `GET_VEHICLE_LIVERY` | `GetVehicleLivery` | public JS global |
| server | `GET_VEHICLE_LOCK_ON_TARGET` | `GetVehicleLockOnTarget` | public JS global |
| server | `GET_VEHICLE_NEON_COLOUR` | `GetVehicleNeonColour` | public JS global |
| server | `GET_VEHICLE_NEON_ENABLED` | `GetVehicleNeonEnabled` | public JS global |
| server | `GET_VEHICLE_NUMBER_PLATE_TEXT` | `GetVehicleNumberPlateText` | public JS global |
| server | `GET_VEHICLE_NUMBER_PLATE_TEXT_INDEX` | `GetVehicleNumberPlateTextIndex` | public JS global |
| server | `GET_VEHICLE_PED_IS_IN` | `GetVehiclePedIsIn` | public JS global |
| server | `GET_VEHICLE_PETROL_TANK_HEALTH` | `GetVehiclePetrolTankHealth` | public JS global |
| server | `GET_VEHICLE_RADIO_STATION_INDEX` | `GetVehicleRadioStationIndex` | public JS global |
| server | `GET_VEHICLE_ROOF_LIVERY` | `GetVehicleRoofLivery` | public JS global |
| server | `GET_VEHICLE_TOTAL_REPAIRS` | `GetVehicleTotalRepairs` | public JS global |
| server | `GET_VEHICLE_TYRE_SMOKE_COLOR` | `GetVehicleTyreSmokeColor` | public JS global |
| server | `GET_VEHICLE_WHEEL_TYPE` | `GetVehicleWheelType` | public JS global |
| server | `GET_VEHICLE_WINDOW_TINT` | `GetVehicleWindowTint` | public JS global |
| server | `GIVE_WEAPON_COMPONENT_TO_PED` | `GiveWeaponComponentToPed` | public JS global |
| server | `GIVE_WEAPON_TO_PED` | `GiveWeaponToPed` | public JS global |
| server | `HAS_ENTITY_BEEN_MARKED_AS_NO_LONGER_NEEDED` | `HasEntityBeenMarkedAsNoLongerNeeded` | public JS global |
| server | `HAS_VEHICLE_BEEN_DAMAGED_BY_BULLETS` | `HasVehicleBeenDamagedByBullets` | public JS global |
| server | `HAS_VEHICLE_BEEN_OWNED_BY_PLAYER` | `HasVehicleBeenOwnedByPlayer` | public JS global |
| server | `IS_BOAT_ANCHORED_AND_FROZEN` | `IsBoatAnchoredAndFrozen` | public JS global |
| server | `IS_BOAT_WRECKED` | `IsBoatWrecked` | public JS global |
| server | `IS_ENTITY_VISIBLE` | `IsEntityVisible` | public JS global |
| server | `IS_FLASH_LIGHT_ON` | `IsFlashLightOn` | public JS global |
| server | `IS_HELI_TAIL_BOOM_BREAKABLE` | `IsHeliTailBoomBreakable` | public JS global |
| server | `IS_HELI_TAIL_BOOM_BROKEN` | `IsHeliTailBoomBroken` | public JS global |
| server | `IS_PED_A_PLAYER` | `IsPedAPlayer` | public JS global |
| server | `IS_PED_HANDCUFFED` | `IsPedHandcuffed` | public JS global |
| server | `IS_PED_IN_ANY_VEHICLE` | `IsPedInAnyVehicle` | public JS global |
| server | `IS_PED_IN_VEHICLE` | `IsPedInVehicle` | public JS global |
| server | `IS_PED_ON_MOUNT` | `IsPedOnMount` | public JS global |
| server | `IS_PED_RAGDOLL` | `IsPedRagdoll` | public JS global |
| server | `IS_PED_STRAFING` | `IsPedStrafing` | public JS global |
| server | `IS_PED_USING_ACTION_MODE` | `IsPedUsingActionMode` | public JS global |
| server | `IS_PLAYER_ACE_ALLOWED` | `IsPlayerAceAllowed` | public JS global |
| server | `IS_PLAYER_COMMERCE_INFO_LOADED` | `IsPlayerCommerceInfoLoaded` | public JS global |
| server | `IS_PLAYER_COMMERCE_INFO_LOADED_EXT` | `IsPlayerCommerceInfoLoadedExt` | public JS global |
| server | `IS_PLAYER_EVADING_WANTED_LEVEL` | `IsPlayerEvadingWantedLevel` | public JS global |
| server | `IS_PLAYER_IN_FREE_CAM_MODE` | `IsPlayerInFreeCamMode` | public JS global |
| server | `IS_PLAYER_USING_SUPER_JUMP` | `IsPlayerUsingSuperJump` | public JS global |
| server | `IS_TRAIN_CABOOSE` | `IsTrainCaboose` | public JS global |
| server | `IS_VEHICLE_EXTRA_TURNED_ON` | `IsVehicleExtraTurnedOn` | public JS global |
| server | `IS_VEHICLE_SIREN_ON` | `IsVehicleSirenOn` | public JS global |
| server | `IS_VEHICLE_TYRE_BURST` | `IsVehicleTyreBurst` | public JS global |
| server | `IS_VEHICLE_WINDOW_INTACT` | `IsVehicleWindowIntact` | public JS global |
| server | `LOAD_PLAYER_COMMERCE_DATA` | `LoadPlayerCommerceData` | public JS global |
| server | `LOAD_PLAYER_COMMERCE_DATA_EXT` | `LoadPlayerCommerceDataExt` | public JS global |
| server | `MUMBLE_CREATE_CHANNEL` | `MumbleCreateChannel` | public JS global |
| server | `MUMBLE_IS_PLAYER_MUTED` | `MumbleIsPlayerMuted` | public JS global |
| server | `MUMBLE_SET_PLAYER_MUTED` | `MumbleSetPlayerMuted` | public JS global |
| server | `NETWORK_GET_ENTITY_FROM_NETWORK_ID` | `NetworkGetEntityFromNetworkId` | public JS global |
| server | `NETWORK_GET_FIRST_ENTITY_OWNER` | `NetworkGetFirstEntityOwner` | public JS global |
| server | `NETWORK_GET_NETWORK_ID_FROM_ENTITY` | `NetworkGetNetworkIdFromEntity` | public JS global |
| server | `NETWORK_GET_VOICE_PROXIMITY_OVERRIDE_FOR_PLAYER` | `NetworkGetVoiceProximityOverrideForPlayer` | public JS global |
| server | `PERFORM_HTTP_REQUEST_INTERNAL` | `PerformHttpRequestInternal` | public JS global |
| server | `PERFORM_HTTP_REQUEST_INTERNAL_EX` | `PerformHttpRequestInternalEx` | public JS global |
| server | `PRINT_STRUCTURED_TRACE` | `PrintStructuredTrace` | public JS global |
| server | `REGISTER_CONSOLE_LISTENER` | `RegisterConsoleListener` | public JS global |
| server | `REGISTER_RESOURCE_ASSET` | `RegisterResourceAsset` | public JS global |
| server | `REGISTER_RESOURCE_BUILD_TASK_FACTORY` | `RegisterResourceBuildTaskFactory` | public JS global |
| server | `REMOVE_ALL_PED_WEAPONS` | `RemoveAllPedWeapons` | public JS global |
| server | `REMOVE_BLIP` | `RemoveBlip` | public JS global |
| server | `REMOVE_WEAPON_COMPONENT_FROM_PED` | `RemoveWeaponComponentFromPed` | public JS global |
| server | `REMOVE_WEAPON_FROM_PED` | `RemoveWeaponFromPed` | public JS global |
| server | `REQUEST_PLAYER_COMMERCE_SESSION` | `RequestPlayerCommerceSession` | public JS global |
| server | `SAVE_RESOURCE_FILE` | `SaveResourceFile` | public JS global |
| server | `SCAN_RESOURCE_ROOT` | `ScanResourceRoot` | public JS global |
| server | `SCHEDULE_RESOURCE_TICK` | `ScheduleResourceTick` | public JS global |
| server | `SET_BLIP_SPRITE` | `SetBlipSprite` | public JS global |
| server | `SET_CONVAR` | `SetConvar` | public JS global |
| server | `SET_CONVAR_REPLICATED` | `SetConvarReplicated` | public JS global |
| server | `SET_CONVAR_SERVER_INFO` | `SetConvarServerInfo` | public JS global |
| server | `SET_CURRENT_PED_WEAPON` | `SetCurrentPedWeapon` | public JS global |
| server | `SET_ENTITY_COORDS` | `SetEntityCoords` | public JS global |
| server | `SET_ENTITY_DISTANCE_CULLING_RADIUS` | `SetEntityDistanceCullingRadius` | public JS global |
| server | `SET_ENTITY_HEADING` | `SetEntityHeading` | public JS global |
| server | `SET_ENTITY_IGNORE_REQUEST_CONTROL_FILTER` | `SetEntityIgnoreRequestControlFilter` | public JS global |
| server | `SET_ENTITY_ORPHAN_MODE` | `SetEntityOrphanMode` | public JS global |
| server | `SET_ENTITY_REMOTE_SYNCED_SCENES_ALLOWED` | `SetEntityRemoteSyncedScenesAllowed` | public JS global |
| server | `SET_ENTITY_ROTATION` | `SetEntityRotation` | public JS global |
| server | `SET_ENTITY_ROUTING_BUCKET` | `SetEntityRoutingBucket` | public JS global |
| server | `SET_ENTITY_VELOCITY` | `SetEntityVelocity` | public JS global |
| server | `SET_GAME_TYPE` | `SetGameType` | public JS global |
| server | `SET_HTTP_HANDLER` | `SetHttpHandler` | public JS global |
| server | `SET_MAP_NAME` | `SetMapName` | public JS global |
| server | `SET_PED_AMMO` | `SetPedAmmo` | public JS global |
| server | `SET_PED_ARMOUR` | `SetPedArmour` | public JS global |
| server | `SET_PED_CAN_RAGDOLL` | `SetPedCanRagdoll` | public JS global |
| server | `SET_PED_COMPONENT_VARIATION` | `SetPedComponentVariation` | public JS global |
| server | `SET_PED_CONFIG_FLAG` | `SetPedConfigFlag` | public JS global |
| server | `SET_PED_DEFAULT_COMPONENT_VARIATION` | `SetPedDefaultComponentVariation` | public JS global |
| server | `SET_PED_HAIR_TINT` | `SetPedHairTint` | public JS global |
| server | `SET_PED_HEAD_BLEND_DATA` | `SetPedHeadBlendData` | public JS global |
| server | `SET_PED_HEAD_OVERLAY` | `SetPedHeadOverlay` | public JS global |
| server | `SET_PED_INTO_VEHICLE` | `SetPedIntoVehicle` | public JS global |
| server | `SET_PED_PROP_INDEX` | `SetPedPropIndex` | public JS global |
| server | `SET_PED_RANDOM_COMPONENT_VARIATION` | `SetPedRandomComponentVariation` | public JS global |
| server | `SET_PED_RANDOM_PROPS` | `SetPedRandomProps` | public JS global |
| server | `SET_PED_RESET_FLAG` | `SetPedResetFlag` | public JS global |
| server | `SET_PED_TO_RAGDOLL` | `SetPedToRagdoll` | public JS global |
| server | `SET_PED_TO_RAGDOLL_WITH_FALL` | `SetPedToRagdollWithFall` | public JS global |
| server | `SET_PLAYER_CONTROL` | `SetPlayerControl` | public JS global |
| server | `SET_PLAYER_CULLING_RADIUS` | `SetPlayerCullingRadius` | public JS global |
| server | `SET_PLAYER_INVINCIBLE` | `SetPlayerInvincible` | public JS global |
| server | `SET_PLAYER_MODEL` | `SetPlayerModel` | public JS global |
| server | `SET_PLAYER_ROUTING_BUCKET` | `SetPlayerRoutingBucket` | public JS global |
| server | `SET_PLAYER_WANTED_LEVEL` | `SetPlayerWantedLevel` | public JS global |
| server | `SET_ROUTING_BUCKET_ENTITY_LOCKDOWN_MODE` | `SetRoutingBucketEntityLockdownMode` | public JS global |
| server | `SET_ROUTING_BUCKET_POPULATION_ENABLED` | `SetRoutingBucketPopulationEnabled` | public JS global |
| server | `SET_VEHICLE_ALARM` | `SetVehicleAlarm` | public JS global |
| server | `SET_VEHICLE_BODY_HEALTH` | `SetVehicleBodyHealth` | public JS global |
| server | `SET_VEHICLE_COLOURS` | `SetVehicleColours` | public JS global |
| server | `SET_VEHICLE_COLOUR_COMBINATION` | `SetVehicleColourCombination` | public JS global |
| server | `SET_VEHICLE_CUSTOM_PRIMARY_COLOUR` | `SetVehicleCustomPrimaryColour` | public JS global |
| server | `SET_VEHICLE_CUSTOM_SECONDARY_COLOUR` | `SetVehicleCustomSecondaryColour` | public JS global |
| server | `SET_VEHICLE_DIRT_LEVEL` | `SetVehicleDirtLevel` | public JS global |
| server | `SET_VEHICLE_DOORS_LOCKED` | `SetVehicleDoorsLocked` | public JS global |
| server | `SET_VEHICLE_DOOR_BROKEN` | `SetVehicleDoorBroken` | public JS global |
| server | `SET_VEHICLE_NUMBER_PLATE_TEXT` | `SetVehicleNumberPlateText` | public JS global |
| server | `START_RESOURCE` | `StartResource` | public JS global |
| server | `STOP_RESOURCE` | `StopResource` | public JS global |
| server | `TASK_COMBAT_PED` | `TaskCombatPed` | public JS global |
| server | `TASK_DRIVE_BY` | `TaskDriveBy` | public JS global |
| server | `TASK_ENTER_VEHICLE` | `TaskEnterVehicle` | public JS global |
| server | `TASK_EVERYONE_LEAVE_VEHICLE` | `TaskEveryoneLeaveVehicle` | public JS global |
| server | `TASK_GO_STRAIGHT_TO_COORD` | `TaskGoStraightToCoord` | public JS global |
| server | `TASK_GO_TO_COORD_ANY_MEANS` | `TaskGoToCoordAnyMeans` | public JS global |
| server | `TASK_GO_TO_ENTITY` | `TaskGoToEntity` | public JS global |
| server | `TASK_HANDS_UP` | `TaskHandsUp` | public JS global |
| server | `TASK_LEAVE_ANY_VEHICLE` | `TaskLeaveAnyVehicle` | public JS global |
| server | `TASK_LEAVE_VEHICLE` | `TaskLeaveVehicle` | public JS global |
| server | `TASK_PLAY_ANIM` | `TaskPlayAnim` | public JS global |
| server | `TASK_PLAY_ANIM_ADVANCED` | `TaskPlayAnimAdvanced` | public JS global |
| server | `TASK_REACT_AND_FLEE_PED` | `TaskReactAndFleePed` | public JS global |
| server | `TASK_SHOOT_AT_COORD` | `TaskShootAtCoord` | public JS global |
| server | `TASK_SHOOT_AT_ENTITY` | `TaskShootAtEntity` | public JS global |
| server | `TASK_WARP_PED_INTO_VEHICLE` | `TaskWarpPedIntoVehicle` | public JS global |
| server | `TEMP_BAN_PLAYER` | `TempBanPlayer` | public JS global |
| server | `TRIGGER_CLIENT_EVENT_INTERNAL` | `TriggerClientEventInternal` | public JS global |
| server | `TRIGGER_LATENT_CLIENT_EVENT_INTERNAL` | `TriggerLatentClientEventInternal` | public JS global |
| server | `VERIFY_PASSWORD_HASH` | `VerifyPasswordHash` | public JS global |
| server | `_ADD_BLIP_FOR_AREA` | `AddBlipForArea` | public JS global |
| server | `_SET_PED_EYE_COLOR` | `SetPedEyeColor` | public JS global |
| server | `_SET_PED_FACE_FEATURE` | `SetPedFaceFeature` | public JS global |
| server | `_SET_PED_HEAD_OVERLAY_COLOR` | `SetPedHeadOverlayColor` | public JS global |
| shared | `ADD_CONVAR_CHANGE_LISTENER` | `AddConvarChangeListener` | public JS global |
| shared | `ADD_STATE_BAG_CHANGE_HANDLER` | `AddStateBagChangeHandler` | public JS global |
| shared | `CANCEL_EVENT` | `CancelEvent` | public JS global |
| shared | `DELETE_FUNCTION_REFERENCE` | `DeleteFunctionReference` | public JS global |
| shared | `DELETE_RESOURCE_KVP` | `DeleteResourceKvp` | public JS global |
| shared | `DELETE_RESOURCE_KVP_NO_SYNC` | `DeleteResourceKvpNoSync` | public JS global |
| shared | `DOES_TRAIN_STOP_AT_STATIONS` | `DoesTrainStopAtStations` | public JS global |
| shared | `DUPLICATE_FUNCTION_REFERENCE` | `DuplicateFunctionReference` | public JS global |
| shared | `END_FIND_KVP` | `EndFindKvp` | public JS global |
| shared | `ENSURE_ENTITY_STATE_BAG` | `EnsureEntityStateBag` | public JS global |
| shared | `EXECUTE_COMMAND` | `ExecuteCommand` | public JS global |
| shared | `FIND_KVP` | `FindKvp` | public JS global |
| shared | `FORMAT_STACK_TRACE` | `FormatStackTrace` | public JS global |
| shared | `GET_CONVAR` | `GetConvar` | public JS global |
| shared | `GET_CONVAR_BOOL` | `GetConvarBool` | public JS global |
| shared | `GET_CONVAR_FLOAT` | `GetConvarFloat` | public JS global |
| shared | `GET_CONVAR_INT` | `GetConvarInt` | public JS global |
| shared | `GET_CURRENT_RESOURCE_NAME` | `GetCurrentResourceName` | public JS global |
| shared | `GET_ENTITIES_IN_RADIUS` | `GetEntitiesInRadius` | public JS global |
| shared | `GET_ENTITY_FROM_STATE_BAG_NAME` | `GetEntityFromStateBagName` | public JS global |
| shared | `GET_GAME_BUILD_NUMBER` | `GetGameBuildNumber` | public JS global |
| shared | `GET_GAME_NAME` | `GetGameName` | public JS global |
| shared | `GET_GAME_POOL` | `GetGamePool` | public JS global |
| shared | `GET_INSTANCE_ID` | `GetInstanceId` | public JS global |
| shared | `GET_INVOKING_RESOURCE` | `GetInvokingResource` | public JS global |
| shared | `GET_NUM_RESOURCES` | `GetNumResources` | public JS global |
| shared | `GET_NUM_RESOURCE_METADATA` | `GetNumResourceMetadata` | public JS global |
| shared | `GET_PLAYER_FROM_STATE_BAG_NAME` | `GetPlayerFromStateBagName` | public JS global |
| shared | `GET_PLAYER_MELEE_WEAPON_DAMAGE_MODIFIER` | `GetPlayerMeleeWeaponDamageModifier` | public JS global |
| shared | `GET_PLAYER_WEAPON_DAMAGE_MODIFIER` | `GetPlayerWeaponDamageModifier` | public JS global |
| shared | `GET_PLAYER_WEAPON_DEFENSE_MODIFIER` | `GetPlayerWeaponDefenseModifier` | public JS global |
| shared | `GET_PLAYER_WEAPON_DEFENSE_MODIFIER_2` | `GetPlayerWeaponDefenseModifier2` | public JS global |
| shared | `GET_REGISTERED_COMMANDS` | `GetRegisteredCommands` | public JS global |
| shared | `GET_RESOURCE_BY_FIND_INDEX` | `GetResourceByFindIndex` | public JS global |
| shared | `GET_RESOURCE_COMMANDS` | `GetResourceCommands` | public JS global |
| shared | `GET_RESOURCE_KVP_FLOAT` | `GetResourceKvpFloat` | public JS global |
| shared | `GET_RESOURCE_KVP_INT` | `GetResourceKvpInt` | public JS global |
| shared | `GET_RESOURCE_KVP_STRING` | `GetResourceKvpString` | public JS global |
| shared | `GET_RESOURCE_METADATA` | `GetResourceMetadata` | public JS global |
| shared | `GET_RESOURCE_STATE` | `GetResourceState` | public JS global |
| shared | `GET_STATE_BAG_KEYS` | `GetStateBagKeys` | public JS global |
| shared | `GET_STATE_BAG_VALUE` | `GetStateBagValue` | public JS global |
| shared | `GET_TRAIN_CRUISE_SPEED` | `GetTrainCruiseSpeed` | public JS global |
| shared | `GET_TRAIN_DIRECTION` | `GetTrainDirection` | public JS global |
| shared | `GET_TRAIN_STATE` | `GetTrainState` | public JS global |
| shared | `GET_TRAIN_TRACK_INDEX` | `GetTrainTrackIndex` | public JS global |
| shared | `GET_VEHICLE_HANDBRAKE` | `GetVehicleHandbrake` | public JS global |
| shared | `GET_VEHICLE_STEERING_ANGLE` | `GetVehicleSteeringAngle` | public JS global |
| shared | `GET_VEHICLE_TYPE` | `GetVehicleType` | public JS global |
| shared | `IS_ACE_ALLOWED` | `IsAceAllowed` | public JS global |
| shared | `IS_DUPLICITY_VERSION` | `IsDuplicityVersion` | public JS global |
| shared | `IS_ENTITY_POSITION_FROZEN` | `IsEntityPositionFrozen` | public JS global |
| shared | `IS_PRINCIPAL_ACE_ALLOWED` | `IsPrincipalAceAllowed` | public JS global |
| shared | `IS_VEHICLE_ENGINE_STARTING` | `IsVehicleEngineStarting` | public JS global |
| shared | `LOAD_RESOURCE_FILE` | `LoadResourceFile` | public JS global |
| shared | `NETWORK_GET_ENTITY_OWNER` | `NetworkGetEntityOwner` | public JS global |
| shared | `PROFILER_ENTER_SCOPE` | `ProfilerEnterScope` | public JS global |
| shared | `PROFILER_EXIT_SCOPE` | `ProfilerExitScope` | public JS global |
| shared | `PROFILER_IS_RECORDING` | `ProfilerIsRecording` | public JS global |
| shared | `REGISTER_COMMAND` | `RegisterCommand` | public JS global |
| shared | `REGISTER_RESOURCE_AS_EVENT_HANDLER` | `RegisterResourceAsEventHandler` | public JS global |
| shared | `REMOVE_CONVAR_CHANGE_LISTENER` | `RemoveConvarChangeListener` | public JS global |
| shared | `REMOVE_STATE_BAG_CHANGE_HANDLER` | `RemoveStateBagChangeHandler` | public JS global |
| shared | `SET_RESOURCE_KVP` | `SetResourceKvp` | public JS global |
| shared | `SET_RESOURCE_KVP_FLOAT` | `SetResourceKvpFloat` | public JS global |
| shared | `SET_RESOURCE_KVP_FLOAT_NO_SYNC` | `SetResourceKvpFloatNoSync` | public JS global |
| shared | `SET_RESOURCE_KVP_INT` | `SetResourceKvpInt` | public JS global |
| shared | `SET_RESOURCE_KVP_INT_NO_SYNC` | `SetResourceKvpIntNoSync` | public JS global |
| shared | `SET_RESOURCE_KVP_NO_SYNC` | `SetResourceKvpNoSync` | public JS global |
| shared | `SET_STATE_BAG_VALUE` | `SetStateBagValue` | public JS global |
| shared | `START_FIND_KVP` | `StartFindKvp` | public JS global |
| shared | `STATE_BAG_HAS_KEY` | `StateBagHasKey` | public JS global |
| shared | `TRIGGER_EVENT_INTERNAL` | `TriggerEventInternal` | public JS global |
| shared | `WAS_EVENT_CANCELED` | `WasEventCanceled` | public JS global |

## Missing `server` natives (0)

| Native | Hash | Game | Signature |
|---|---:|---|---|

## Missing `shared` natives (0)

| Native | Hash | Game | Signature |
|---|---:|---|---|
