// BASTON bootstrap — global polyfills injected into every resource isolate
// before any resource script runs. Mirrors the FXServer JS scripting surface
// needed for Phase A.
"use strict";
(function (globalThis) {
  const ops = Deno.core.ops;

  function stringify(value) {
    if (typeof value === "string") return value;
    if (value instanceof Error) return value.stack || String(value);
    try {
      return JSON.stringify(value);
    } catch {
      return String(value);
    }
  }

  function makeLog(prefix) {
    return (...args) =>
      ops.op_console_log(prefix + args.map(stringify).join(" "));
  }

  globalThis.console = {
    log: makeLog(""),
    info: makeLog(""),
    warn: makeLog("[warn] "),
    error: makeLog("[error] "),
    debug: makeLog("[debug] "),
  };

  // --- events ---

  let nextCallbackId = 1;
  // event name -> Map<callbackId, fn>
  const eventHandlers = new Map();
  const commandHandlers = new Map();

  function AddEventHandler(name, cb) {
    if (typeof cb !== "function") {
      throw new TypeError("AddEventHandler: callback must be a function");
    }
    const id = nextCallbackId++;
    if (!eventHandlers.has(name)) eventHandlers.set(name, new Map());
    eventHandlers.get(name).set(id, cb);
    ops.op_add_event_handler(name, id);
    return { name, id };
  }

  function RemoveEventHandler(handle) {
    if (!handle) return;
    const handlers = eventHandlers.get(handle.name);
    if (handlers) handlers.delete(handle.id);
  }

  function TriggerEvent(name, ...args) {
    // Queued Rust-side; the script host re-broadcasts to every runtime
    // (including this one) so cross-resource events work uniformly.
    ops.op_trigger_event(name, JSON.stringify(args));
  }

  function TriggerClientEvent(name, source, ...args) {
    ops.op_trigger_client_event(String(name), source >>> 0, JSON.stringify(args));
  }

  function RegisterCommand(name, cb, restricted = false) {
    if (typeof cb !== "function") {
      throw new TypeError("RegisterCommand: callback must be a function");
    }
    const command = String(name);
    const id = nextCallbackId++;
    commandHandlers.set(command, { id, cb, restricted: !!restricted });
    ops.op_register_command(command, !!restricted, id);
  }

  // Server → client native dispatch through the BASTON shim (see
  // baston-protocol native.rs). Returns a Promise.
  async function InvokeNativeOnClient(source, hashHex, args, expectsReturn) {
    const raw = await ops.op_invoke_native_on_client(
      source >>> 0,
      String(hashHex),
      JSON.stringify(args ?? []),
      expectsReturn !== false
    );
    const result = JSON.parse(raw);
    if (result && typeof result === "object" && result.__error) {
      throw new Error(result.__error);
    }
    return result;
  }

  function InvokeCfxSharedNative(name, args) {
    return JSON.parse(ops.op_cfx_shared_native(name, JSON.stringify(args ?? [])));
  }

  function InvokeCfxServerNative(name, resultKind, args) {
    return JSON.parse(
      ops.op_cfx_server_native(name, resultKind, JSON.stringify(args ?? []))
    );
  }

  // --- exports ---

  const localExports = new Map();

  function registerExport(name, fn) {
    const id = nextCallbackId++;
    localExports.set(name, fn);
    ops.op_add_export(name, id);
  }

  const exportsProxy = new Proxy(function () {}, {
    apply(_target, _thisArg, args) {
      registerExport(args[0], args[1]);
    },
    get(_target, resource) {
      return new Proxy(
        {},
        {
          get(_t, fnName) {
            return (...args) => {
              if (
                resource === ops.op_get_current_resource_name() &&
                localExports.has(fnName)
              ) {
                return localExports.get(fnName)(...args);
              }
              ops.op_get_export(String(resource), String(fnName));
              throw new Error(
                `export ${String(resource)}.${String(fnName)} unavailable (Phase A: no cross-resource exports)`
              );
            };
          },
        }
      );
    },
  });

  // --- internal dispatch API (called from Rust via execute_script) ---

  // Track async handler completion so Rust can wait for THIS dispatch only
  // (not the whole event loop). A rejected handler promise is reported the
  // same way as a sync throw, so the returned promise never rejects for
  // handler errors — Rust treats a rejection as a dispatch-level failure.
  function settleHandlerResult(name, result, pending) {
    if (result && typeof result.then === "function") {
      pending.push(
        result.then(undefined, (e) => {
          ops.op_report_handler_error();
          console.error(`[baston] error in '${name}' handler: ${stringify(e)}`);
        })
      );
    }
  }

  function dispatch(name, argsJson) {
    const handlers = eventHandlers.get(name);
    if (!handlers || handlers.size === 0) return;
    const args = JSON.parse(argsJson);
    const pending = [];
    for (const fn of [...handlers.values()]) {
      try {
        settleHandlerResult(name, fn(...args), pending);
      } catch (e) {
        ops.op_report_handler_error();
        console.error(`[baston] error in '${name}' handler: ${stringify(e)}`);
      }
    }
    if (pending.length) return Promise.all(pending);
  }

  function dispatchWithSource(name, source, argsJson) {
    const prev = globalThis.source;
    globalThis.source = source;
    try {
      return dispatch(name, argsJson);
    } finally {
      globalThis.source = prev;
    }
  }

  function dispatchPlayerConnecting(source, playerName) {
    const handlers = eventHandlers.get("playerConnecting");
    if (!handlers || handlers.size === 0) return;
    const setKickReason = (reason) =>
      ops.op_set_kick_reason(source, String(reason));
    const deferrals = {
      defer: () => ops.op_deferral_defer(source),
      update: (msg) => ops.op_deferral_update(source, String(msg)),
      done: (reason) =>
        ops.op_deferral_done(source, reason == null ? "" : String(reason)),
      presentCard: (card) =>
        ops.op_deferral_present_card(
          source,
          typeof card === "string" ? card : JSON.stringify(card)
        ),
    };
    const pending = [];
    for (const fn of [...handlers.values()]) {
      try {
        const result = fn(playerName, setKickReason, deferrals);
        if (result && typeof result.then === "function") {
          pending.push(
            result.then(undefined, (e) => {
              ops.op_report_handler_error();
              console.error(
                `[baston] error in playerConnecting handler: ${stringify(e)}`
              );
              ops.op_deferral_done(
                source,
                "server error in playerConnecting handler"
              );
            })
          );
        }
      } catch (e) {
        ops.op_report_handler_error();
        console.error(
          `[baston] error in playerConnecting handler: ${stringify(e)}`
        );
        ops.op_deferral_done(source, "server error in playerConnecting handler");
      }
    }
    if (pending.length) return Promise.all(pending);
  }

  function dispatchCommand(name, source, argsJson, raw) {
    const entry = commandHandlers.get(name);
    if (!entry) return;
    const args = JSON.parse(argsJson);
    const prev = globalThis.source;
    globalThis.source = source;
    const pending = [];
    try {
      settleHandlerResult(`command:${name}`, entry.cb(source, args, raw), pending);
    } catch (e) {
      ops.op_report_handler_error();
      console.error(`[baston] error in command '${name}': ${stringify(e)}`);
    } finally {
      globalThis.source = prev;
    }
    if (pending.length) return Promise.all(pending);
  }

  // --- Zone transfer state (Phase D handoffs) ---
  // Resources register callbacks returning the state BASTON must carry to the
  // next zone. Collection merges all callbacks of this resource into one
  // object and reports it to Rust.
  const zoneTransferCallbacks = [];
  function RegisterZoneTransferState(cb) {
    if (typeof cb !== "function") return;
    zoneTransferCallbacks.push(cb);
    ops.op_register_zone_transfer_state();
  }
  function collectZoneTransferState(source) {
    if (zoneTransferCallbacks.length === 0) return;
    const merged = {};
    for (const cb of zoneTransferCallbacks) {
      try {
        Object.assign(merged, cb(source) || {});
      } catch (e) {
        ops.op_report_handler_error();
        console.error(`[baston] error in zone transfer state callback: ${stringify(e)}`);
      }
    }
    ops.op_report_zone_transfer_state(JSON.stringify(merged));
  }

  globalThis.__baston = {
    dispatch,
    dispatchWithSource,
    dispatchPlayerConnecting,
    dispatchCommand,
    collectZoneTransferState,
  };
  globalThis.RegisterZoneTransferState = RegisterZoneTransferState;

  // --- FiveM-style globals ---

  globalThis.AddEventHandler = AddEventHandler;
  globalThis.on = AddEventHandler;
  globalThis.onNet = AddEventHandler; // Phase A: net events behave like local events
  globalThis.RemoveEventHandler = RemoveEventHandler;
  globalThis.TriggerEvent = TriggerEvent;
  globalThis.emit = TriggerEvent;
  globalThis.TriggerClientEvent = TriggerClientEvent;
  globalThis.emitNet = TriggerClientEvent;
  globalThis.RegisterCommand = RegisterCommand;
  globalThis.InvokeNativeOnClient = InvokeNativeOnClient;
  globalThis.AddConvarChangeListener = (conVarFilter, handler) =>
    InvokeCfxSharedNative("ADD_CONVAR_CHANGE_LISTENER", [conVarFilter, handler]);
  globalThis.AddStateBagChangeHandler = (keyFilter, bagFilter, handler) =>
    InvokeCfxSharedNative("ADD_STATE_BAG_CHANGE_HANDLER", [keyFilter, bagFilter, handler]);
  globalThis.CancelEvent = () => InvokeCfxSharedNative("CANCEL_EVENT", []);
  globalThis.DeleteFunctionReference = (referenceIdentity) =>
    InvokeCfxSharedNative("DELETE_FUNCTION_REFERENCE", [referenceIdentity]);
  globalThis.DeleteResourceKvp = (key) =>
    InvokeCfxSharedNative("DELETE_RESOURCE_KVP", [key]);
  globalThis.DeleteResourceKvpNoSync = (key) =>
    InvokeCfxSharedNative("DELETE_RESOURCE_KVP_NO_SYNC", [key]);
  globalThis.DuplicateFunctionReference = (referenceIdentity) =>
    InvokeCfxSharedNative("DUPLICATE_FUNCTION_REFERENCE", [referenceIdentity]);
  globalThis.EnsureEntityStateBag = (entity) =>
    InvokeCfxSharedNative("ENSURE_ENTITY_STATE_BAG", [entity]);
  globalThis.ExecuteCommand = (commandString) =>
    InvokeCfxSharedNative("EXECUTE_COMMAND", [commandString]);
  globalThis.FormatStackTrace = (traceData) =>
    InvokeCfxSharedNative("FORMAT_STACK_TRACE", [traceData]);
  globalThis.GetEntitiesInRadius = (x, y, z, radius, entityType) =>
    InvokeCfxSharedNative("GET_ENTITIES_IN_RADIUS", [x, y, z, radius, entityType]);
  globalThis.GetEntityFromStateBagName = (bagName) =>
    InvokeCfxSharedNative("GET_ENTITY_FROM_STATE_BAG_NAME", [bagName]);
  globalThis.GetGameBuildNumber = () =>
    InvokeCfxSharedNative("GET_GAME_BUILD_NUMBER", []);
  globalThis.GetGameName = () => InvokeCfxSharedNative("GET_GAME_NAME", []);
  globalThis.GetGamePool = (poolName) =>
    InvokeCfxSharedNative("GET_GAME_POOL", [poolName]);
  globalThis.GetInstanceId = () => InvokeCfxSharedNative("GET_INSTANCE_ID", []);
  globalThis.GetInvokingResource = () =>
    InvokeCfxSharedNative("GET_INVOKING_RESOURCE", []);
  globalThis.GetPlayerFromStateBagName = (bagName) =>
    InvokeCfxSharedNative("GET_PLAYER_FROM_STATE_BAG_NAME", [bagName]);
  globalThis.GetRegisteredCommands = () =>
    InvokeCfxSharedNative("GET_REGISTERED_COMMANDS", []);
  globalThis.GetResourceCommands = (resource) =>
    InvokeCfxSharedNative("GET_RESOURCE_COMMANDS", [resource]);
  globalThis.GetResourceKvpFloat = (key) =>
    InvokeCfxSharedNative("GET_RESOURCE_KVP_FLOAT", [key]);
  globalThis.GetResourceKvpInt = (key) =>
    InvokeCfxSharedNative("GET_RESOURCE_KVP_INT", [key]);
  globalThis.GetResourceKvpString = (key) =>
    InvokeCfxSharedNative("GET_RESOURCE_KVP_STRING", [key]);
  globalThis.GetStateBagKeys = (bagName) =>
    InvokeCfxSharedNative("GET_STATE_BAG_KEYS", [bagName]);
  globalThis.GetStateBagValue = (bagName, key) =>
    InvokeCfxSharedNative("GET_STATE_BAG_VALUE", [bagName, key]);
  globalThis.IsAceAllowed = (object) =>
    InvokeCfxSharedNative("IS_ACE_ALLOWED", [object]);
  globalThis.IsDuplicityVersion = () =>
    InvokeCfxSharedNative("IS_DUPLICITY_VERSION", []);
  globalThis.IsPrincipalAceAllowed = (principal, object) =>
    InvokeCfxSharedNative("IS_PRINCIPAL_ACE_ALLOWED", [principal, object]);
  globalThis.ProfilerEnterScope = (scopeName) =>
    InvokeCfxSharedNative("PROFILER_ENTER_SCOPE", [scopeName]);
  globalThis.ProfilerExitScope = () =>
    InvokeCfxSharedNative("PROFILER_EXIT_SCOPE", []);
  globalThis.ProfilerIsRecording = () =>
    InvokeCfxSharedNative("PROFILER_IS_RECORDING", []);
  globalThis.RegisterResourceAsEventHandler = (eventName) =>
    InvokeCfxSharedNative("REGISTER_RESOURCE_AS_EVENT_HANDLER", [eventName]);
  globalThis.RemoveConvarChangeListener = (cookie) =>
    InvokeCfxSharedNative("REMOVE_CONVAR_CHANGE_LISTENER", [cookie]);
  globalThis.RemoveStateBagChangeHandler = (cookie) =>
    InvokeCfxSharedNative("REMOVE_STATE_BAG_CHANGE_HANDLER", [cookie]);
  globalThis.SetResourceKvp = (key, value) =>
    InvokeCfxSharedNative("SET_RESOURCE_KVP", [key, value]);
  globalThis.SetResourceKvpFloat = (key, value) =>
    InvokeCfxSharedNative("SET_RESOURCE_KVP_FLOAT", [key, value]);
  globalThis.SetResourceKvpFloatNoSync = (key, value) =>
    InvokeCfxSharedNative("SET_RESOURCE_KVP_FLOAT_NO_SYNC", [key, value]);
  globalThis.SetResourceKvpInt = (key, value) =>
    InvokeCfxSharedNative("SET_RESOURCE_KVP_INT", [key, value]);
  globalThis.SetResourceKvpIntNoSync = (key, value) =>
    InvokeCfxSharedNative("SET_RESOURCE_KVP_INT_NO_SYNC", [key, value]);
  globalThis.SetResourceKvpNoSync = (key, value) =>
    InvokeCfxSharedNative("SET_RESOURCE_KVP_NO_SYNC", [key, value]);
  globalThis.SetStateBagValue = (bagName, keyName, valueData, valueLength, replicated) =>
    InvokeCfxSharedNative("SET_STATE_BAG_VALUE", [
      bagName,
      keyName,
      valueData,
      valueLength,
      replicated,
    ]);
  globalThis.StateBagHasKey = (bagName, key) =>
    InvokeCfxSharedNative("STATE_BAG_HAS_KEY", [bagName, key]);
  globalThis.TriggerEventInternal = (eventName, eventPayload, payloadLength) =>
    InvokeCfxSharedNative("TRIGGER_EVENT_INTERNAL", [
      eventName,
      eventPayload,
      payloadLength,
    ]);
  globalThis.WasEventCanceled = () =>
    InvokeCfxSharedNative("WAS_EVENT_CANCELED", []);

  const kvpFinds = new Map();
  let nextKvpFind = 1;
  globalThis.StartFindKvp = (prefix) => {
    const handle = nextKvpFind++;
    kvpFinds.set(handle, InvokeCfxSharedNative("FIND_KVP", [prefix]));
    return handle;
  };
  globalThis.FindKvp = (handle) => {
    const keys = kvpFinds.get(handle) || [];
    return keys.length === 0 ? null : keys.shift();
  };
  globalThis.EndFindKvp = (handle) => {
    kvpFinds.delete(handle);
    InvokeCfxSharedNative("END_FIND_KVP", [handle]);
  };

  // Shared CFX entity/train/vehicle accessors that are backed by the future
  // entity state-bag bridge. They intentionally return neutral values today.
  globalThis.DoesTrainStopAtStations = (train) => false;
  globalThis.GetPlayerMeleeWeaponDamageModifier = (playerId) => 1.0;
  globalThis.GetPlayerWeaponDamageModifier = (playerId) => 1.0;
  globalThis.GetPlayerWeaponDefenseModifier = (playerId) => 1.0;
  globalThis.GetPlayerWeaponDefenseModifier2 = (playerId) => 1.0;
  globalThis.GetTrainCruiseSpeed = (train) => 0.0;
  globalThis.GetTrainDirection = (train) => false;
  globalThis.GetTrainState = (train) => 0;
  globalThis.GetTrainTrackIndex = (train) => 0;
  globalThis.GetVehicleHandbrake = (vehicle) => false;
  globalThis.GetVehicleSteeringAngle = (vehicle) => 0.0;
  globalThis.GetVehicleType = (vehicle) => "";
  globalThis.IsEntityPositionFrozen = (entity) => false;
  globalThis.IsVehicleEngineStarting = (vehicle) => false;
  globalThis.NetworkGetEntityOwner = (entity) => 0;
  globalThis.GetConvar = (name, defaultValue) =>
    ops.op_get_convar(String(name), String(defaultValue ?? ""));
  globalThis.GetConvarInt = (name, defaultValue) =>
    ops.op_get_convar_int(String(name), Number(defaultValue ?? 0) | 0);
  globalThis.GetConvarFloat = (name, defaultValue) =>
    ops.op_get_convar_float(String(name), Number(defaultValue ?? 0));
  globalThis.GetConvarBool = (name, defaultValue) =>
    ops.op_get_convar_bool(String(name), !!defaultValue);
  globalThis.SetConvar = (name, value) =>
    ops.op_set_convar(String(name), String(value ?? ""));
  globalThis.SetConvarReplicated = globalThis.SetConvar;
  globalThis.SetConvarServerInfo = globalThis.SetConvar;
  // GetPlayerPed(source): round trip to the player's client (Phase B).
  globalThis.GetPlayerPed = (source) =>
    InvokeNativeOnClient(source, "0x43A66C31C68491C0", [-1], true);
  globalThis.RegisterNetEvent = function () {}; // net events auto-registered
  globalThis.exports = exportsProxy;
  globalThis.GetCurrentResourceName = () => ops.op_get_current_resource_name();
  globalThis.GetGameTimer = () => ops.op_get_game_timer();
  globalThis.GetNumResources = () => ops.op_get_num_resources();
  globalThis.GetResourceByFindIndex = (index) => {
    const name = ops.op_get_resource_by_find_index(index >>> 0);
    return name === "" ? null : name;
  };
  globalThis.GetResourceState = (name) => ops.op_get_resource_state(String(name));
  globalThis.GetResourcePath = (name) => {
    const path = ops.op_get_resource_path(String(name));
    return path === "" ? null : path;
  };
  globalThis.GetNumResourceMetadata = (resource, key) =>
    ops.op_get_num_resource_metadata(String(resource), String(key));
  globalThis.GetResourceMetadata = (resource, key, index) => {
    const value = ops.op_get_resource_metadata(
      String(resource),
      String(key),
      index >>> 0
    );
    return value === "" ? null : value;
  };
  globalThis.LoadResourceFile = (resource, fileName) => {
    const value = ops.op_load_resource_file(String(resource), String(fileName));
    return value === "" ? null : value;
  };
  globalThis.SaveResourceFile = (resource, fileName, data, dataLength = -1) =>
    ops.op_save_resource_file(
      String(resource),
      String(fileName),
      String(data ?? ""),
      Number(dataLength ?? -1) | 0
    );
  globalThis.GetNumPlayerIndices = () => ops.op_get_num_player_indices();
  globalThis.GetPlayerFromIndex = (i) => ops.op_get_player_from_index(i >>> 0);
  globalThis.GetPlayerName = (source) => ops.op_get_player_name(source >>> 0);
  globalThis.DoesPlayerExist = (source) =>
    ops.op_does_player_exist(source >>> 0);
  globalThis.GetNumPlayerIdentifiers = (source) =>
    ops.op_get_num_player_identifiers(source >>> 0);
  globalThis.GetPlayerIdentifier = (source, index) => {
    const id = ops.op_get_player_identifier(source >>> 0, index >>> 0);
    return id === "" ? null : id;
  };
  globalThis.GetPlayerIdentifierByType = (source, type) =>
    ops.op_get_player_identifier_by_type(source >>> 0, String(type));
  globalThis.GetPlayerEndpoint = (source) => {
    const endpoint = ops.op_get_player_endpoint(source >>> 0);
    return endpoint === "" ? null : endpoint;
  };
  globalThis.GetPlayerGuid = (source) => {
    const guid = ops.op_get_player_guid(source >>> 0);
    return guid === "" ? null : guid;
  };
  globalThis.GetPlayerPing = (source) => ops.op_get_player_ping(source >>> 0);
  globalThis.GetNumPlayerTokens = (source) =>
    ops.op_get_num_player_tokens(source >>> 0);
  globalThis.GetPlayerToken = (source, index) => {
    const token = ops.op_get_player_token(source >>> 0, index >>> 0);
    return token === "" ? null : token;
  };
  globalThis.GetPlayers = () => {
    const n = ops.op_get_num_player_indices();
    const out = [];
    for (let i = 0; i < n; i++) out.push(ops.op_get_player_from_index(i));
    return out;
  };
  globalThis.FreezeEntityPosition = (...args) =>
    InvokeCfxServerNative("FREEZE_ENTITY_POSITION", "void", args);
  globalThis.GetEntityCoords = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_COORDS", "Vector3", args);
  globalThis.SetEntityCoords = (...args) =>
    InvokeCfxServerNative("SET_ENTITY_COORDS", "void", args);
  globalThis.SetEntityHeading = (...args) =>
    InvokeCfxServerNative("SET_ENTITY_HEADING", "void", args);
  globalThis.SetPlayerModel = (...args) =>
    InvokeCfxServerNative("SET_PLAYER_MODEL", "void", args);
  globalThis.SetVehicleDoorsLocked = (...args) =>
    InvokeCfxServerNative("SET_VEHICLE_DOORS_LOCKED", "void", args);

  // --- generated CFX server native shims (from natives_cfx.json) ---
  globalThis.AddBlipForCoord = (...args) =>
    InvokeCfxServerNative("ADD_BLIP_FOR_COORD", "Blip", args);
  globalThis.AddBlipForEntity = (...args) =>
    InvokeCfxServerNative("ADD_BLIP_FOR_ENTITY", "Blip", args);
  globalThis.AddBlipForRadius = (...args) =>
    InvokeCfxServerNative("ADD_BLIP_FOR_RADIUS", "Blip", args);
  globalThis.AddPedDecorationFromHashes = (...args) =>
    InvokeCfxServerNative("ADD_PED_DECORATION_FROM_HASHES", "void", args);
  globalThis.ApplyForceToEntity = (...args) =>
    InvokeCfxServerNative("APPLY_FORCE_TO_ENTITY", "void", args);
  globalThis.CanPlayerStartCommerceSession = (...args) =>
    InvokeCfxServerNative("CAN_PLAYER_START_COMMERCE_SESSION", "BOOL", args);
  globalThis.ClearPedProp = (...args) =>
    InvokeCfxServerNative("CLEAR_PED_PROP", "void", args);
  globalThis.ClearPedSecondaryTask = (...args) =>
    InvokeCfxServerNative("CLEAR_PED_SECONDARY_TASK", "void", args);
  globalThis.ClearPedTasks = (...args) =>
    InvokeCfxServerNative("CLEAR_PED_TASKS", "void", args);
  globalThis.ClearPedTasksImmediately = (...args) =>
    InvokeCfxServerNative("CLEAR_PED_TASKS_IMMEDIATELY", "void", args);
  globalThis.ClearPlayerWantedLevel = (...args) =>
    InvokeCfxServerNative("CLEAR_PLAYER_WANTED_LEVEL", "void", args);
  globalThis.CreateObject = (...args) =>
    InvokeCfxServerNative("CREATE_OBJECT", "Entity", args);
  globalThis.CreateObjectNoOffset = (...args) =>
    InvokeCfxServerNative("CREATE_OBJECT_NO_OFFSET", "Entity", args);
  globalThis.CreatePed = (...args) =>
    InvokeCfxServerNative("CREATE_PED", "Entity", args);
  globalThis.CreatePedInsideVehicle = (...args) =>
    InvokeCfxServerNative("CREATE_PED_INSIDE_VEHICLE", "Entity", args);
  globalThis.CreateVehicle = (...args) =>
    InvokeCfxServerNative("CREATE_VEHICLE", "Entity", args);
  globalThis.CreateVehicleServerSetter = (...args) =>
    InvokeCfxServerNative("CREATE_VEHICLE_SERVER_SETTER", "Vehicle", args);
  globalThis.DeleteEntity = (...args) =>
    InvokeCfxServerNative("DELETE_ENTITY", "void", args);
  globalThis.DeleteTrain = (...args) =>
    InvokeCfxServerNative("DELETE_TRAIN", "void", args);
  globalThis.DoesBoatSinkWhenWrecked = (...args) =>
    InvokeCfxServerNative("DOES_BOAT_SINK_WHEN_WRECKED", "bool", args);
  globalThis.DoesEntityExist = (...args) =>
    InvokeCfxServerNative("DOES_ENTITY_EXIST", "BOOL", args);
  globalThis.DoesPlayerOwnSku = (...args) =>
    InvokeCfxServerNative("DOES_PLAYER_OWN_SKU", "BOOL", args);
  globalThis.DoesPlayerOwnSkuExt = (...args) =>
    InvokeCfxServerNative("DOES_PLAYER_OWN_SKU_EXT", "BOOL", args);
  globalThis.DropPlayer = (...args) =>
    InvokeCfxServerNative("DROP_PLAYER", "void", args);
  globalThis.EnableEnhancedHostSupport = (...args) =>
    InvokeCfxServerNative("ENABLE_ENHANCED_HOST_SUPPORT", "void", args);
  globalThis.FlagServerAsPrivate = (...args) =>
    InvokeCfxServerNative("FLAG_SERVER_AS_PRIVATE", "void", args);
  globalThis.FlushResourceKvp = (...args) =>
    InvokeCfxServerNative("FLUSH_RESOURCE_KVP", "void", args);
  globalThis.GetAirDragMultiplierForPlayersVehicle = (...args) =>
    InvokeCfxServerNative("GET_AIR_DRAG_MULTIPLIER_FOR_PLAYERS_VEHICLE", "float", args);
  globalThis.GetAllObjects = (...args) =>
    InvokeCfxServerNative("GET_ALL_OBJECTS", "object", args);
  globalThis.GetAllPeds = (...args) =>
    InvokeCfxServerNative("GET_ALL_PEDS", "object", args);
  globalThis.GetAllVehicles = (...args) =>
    InvokeCfxServerNative("GET_ALL_VEHICLES", "object", args);
  globalThis.GetConsoleBuffer = (...args) =>
    InvokeCfxServerNative("GET_CONSOLE_BUFFER", "char*", args);
  globalThis.GetCurrentPedWeapon = (...args) =>
    InvokeCfxServerNative("GET_CURRENT_PED_WEAPON", "Hash", args);
  globalThis.GetEntityAttachedTo = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_ATTACHED_TO", "Entity", args);
  globalThis.GetEntityCollisionDisabled = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_COLLISION_DISABLED", "bool", args);
  globalThis.GetEntityHeading = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_HEADING", "float", args);
  globalThis.GetEntityHealth = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_HEALTH", "int", args);
  globalThis.GetEntityMaxHealth = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_MAX_HEALTH", "int", args);
  globalThis.GetEntityModel = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_MODEL", "Hash", args);
  globalThis.GetEntityOrphanMode = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_ORPHAN_MODE", "int", args);
  globalThis.GetEntityPopulationType = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_POPULATION_TYPE", "int", args);
  globalThis.GetEntityRemoteSyncedScenesAllowed = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_REMOTE_SYNCED_SCENES_ALLOWED", "BOOL", args);
  globalThis.GetEntityRotation = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_ROTATION", "Vector3", args);
  globalThis.GetEntityRotationVelocity = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_ROTATION_VELOCITY", "Vector3", args);
  globalThis.GetEntityRoutingBucket = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_ROUTING_BUCKET", "int", args);
  globalThis.GetEntityScript = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_SCRIPT", "char*", args);
  globalThis.GetEntitySpeed = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_SPEED", "float", args);
  globalThis.GetEntityType = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_TYPE", "int", args);
  globalThis.GetEntityVelocity = (...args) =>
    InvokeCfxServerNative("GET_ENTITY_VELOCITY", "Vector3", args);
  globalThis.GetHashKey = (...args) =>
    InvokeCfxServerNative("GET_HASH_KEY", "Hash", args);
  globalThis.GetHeliBodyHealth = (...args) =>
    InvokeCfxServerNative("GET_HELI_BODY_HEALTH", "int", args);
  globalThis.GetHeliDisableExplodeFromBodyDamage = (...args) =>
    InvokeCfxServerNative("GET_HELI_DISABLE_EXPLODE_FROM_BODY_DAMAGE", "BOOL", args);
  globalThis.GetHeliEngineHealth = (...args) =>
    InvokeCfxServerNative("GET_HELI_ENGINE_HEALTH", "int", args);
  globalThis.GetHeliGasTankHealth = (...args) =>
    InvokeCfxServerNative("GET_HELI_GAS_TANK_HEALTH", "int", args);
  globalThis.GetHeliMainRotorDamageScale = (...args) =>
    InvokeCfxServerNative("GET_HELI_MAIN_ROTOR_DAMAGE_SCALE", "float", args);
  globalThis.GetHeliMainRotorHealth = (...args) =>
    InvokeCfxServerNative("GET_HELI_MAIN_ROTOR_HEALTH", "float", args);
  globalThis.GetHeliPitchControl = (...args) =>
    InvokeCfxServerNative("GET_HELI_PITCH_CONTROL", "float", args);
  globalThis.GetHeliRearRotorDamageScale = (...args) =>
    InvokeCfxServerNative("GET_HELI_REAR_ROTOR_DAMAGE_SCALE", "float", args);
  globalThis.GetHeliRearRotorHealth = (...args) =>
    InvokeCfxServerNative("GET_HELI_REAR_ROTOR_HEALTH", "float", args);
  globalThis.GetHeliRollControl = (...args) =>
    InvokeCfxServerNative("GET_HELI_ROLL_CONTROL", "float", args);
  globalThis.GetHeliTailRotorDamageScale = (...args) =>
    InvokeCfxServerNative("GET_HELI_TAIL_ROTOR_DAMAGE_SCALE", "float", args);
  globalThis.GetHeliTailRotorHealth = (...args) =>
    InvokeCfxServerNative("GET_HELI_TAIL_ROTOR_HEALTH", "float", args);
  globalThis.GetHeliThrottleControl = (...args) =>
    InvokeCfxServerNative("GET_HELI_THROTTLE_CONTROL", "float", args);
  globalThis.GetHeliYawControl = (...args) =>
    InvokeCfxServerNative("GET_HELI_YAW_CONTROL", "float", args);
  globalThis.GetHostId = (...args) =>
    InvokeCfxServerNative("GET_HOST_ID", "char*", args);
  globalThis.GetIsHeliEngineRunning = (...args) =>
    InvokeCfxServerNative("GET_IS_HELI_ENGINE_RUNNING", "BOOL", args);
  globalThis.GetIsVehicleEngineRunning = (...args) =>
    InvokeCfxServerNative("GET_IS_VEHICLE_ENGINE_RUNNING", "BOOL", args);
  globalThis.GetIsVehiclePrimaryColourCustom = (...args) =>
    InvokeCfxServerNative("GET_IS_VEHICLE_PRIMARY_COLOUR_CUSTOM", "BOOL", args);
  globalThis.GetIsVehicleSecondaryColourCustom = (...args) =>
    InvokeCfxServerNative("GET_IS_VEHICLE_SECONDARY_COLOUR_CUSTOM", "BOOL", args);
  globalThis.GetLandingGearState = (...args) =>
    InvokeCfxServerNative("GET_LANDING_GEAR_STATE", "int", args);
  globalThis.GetLastPedInVehicleSeat = (...args) =>
    InvokeCfxServerNative("GET_LAST_PED_IN_VEHICLE_SEAT", "Entity", args);
  globalThis.GetMount = (...args) =>
    InvokeCfxServerNative("GET_MOUNT", "Ped", args);
  globalThis.GetNetTypeFromEntity = (...args) =>
    InvokeCfxServerNative("GET_NET_TYPE_FROM_ENTITY", "int", args);
  globalThis.GetPasswordHash = (...args) =>
    InvokeCfxServerNative("GET_PASSWORD_HASH", "char*", args);
  globalThis.GetPedArmour = (...args) =>
    InvokeCfxServerNative("GET_PED_ARMOUR", "int", args);
  globalThis.GetPedCauseOfDeath = (...args) =>
    InvokeCfxServerNative("GET_PED_CAUSE_OF_DEATH", "Hash", args);
  globalThis.GetPedDesiredHeading = (...args) =>
    InvokeCfxServerNative("GET_PED_DESIRED_HEADING", "float", args);
  globalThis.GetPedInVehicleSeat = (...args) =>
    InvokeCfxServerNative("GET_PED_IN_VEHICLE_SEAT", "Entity", args);
  globalThis.GetPedMaxHealth = (...args) =>
    InvokeCfxServerNative("GET_PED_MAX_HEALTH", "int", args);
  globalThis.GetPedRelationshipGroupHash = (...args) =>
    InvokeCfxServerNative("GET_PED_RELATIONSHIP_GROUP_HASH", "Hash", args);
  globalThis.GetPedScriptTaskCommand = (...args) =>
    InvokeCfxServerNative("GET_PED_SCRIPT_TASK_COMMAND", "Hash", args);
  globalThis.GetPedScriptTaskStage = (...args) =>
    InvokeCfxServerNative("GET_PED_SCRIPT_TASK_STAGE", "int", args);
  globalThis.GetPedSourceOfDamage = (...args) =>
    InvokeCfxServerNative("GET_PED_SOURCE_OF_DAMAGE", "Entity", args);
  globalThis.GetPedSourceOfDeath = (...args) =>
    InvokeCfxServerNative("GET_PED_SOURCE_OF_DEATH", "Entity", args);
  globalThis.GetPedSpecificTaskType = (...args) =>
    InvokeCfxServerNative("GET_PED_SPECIFIC_TASK_TYPE", "int", args);
  globalThis.GetPedStealthMovement = (...args) =>
    InvokeCfxServerNative("GET_PED_STEALTH_MOVEMENT", "bool", args);
  globalThis.GetPlayerCameraRotation = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_CAMERA_ROTATION", "Vector3", args);
  globalThis.GetPlayerFakeWantedLevel = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_FAKE_WANTED_LEVEL", "int", args);
  globalThis.GetPlayerFocusPos = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_FOCUS_POS", "Vector3", args);
  globalThis.GetPlayerInvincible = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_INVINCIBLE", "BOOL", args);
  globalThis.GetPlayerLastMsg = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_LAST_MSG", "int", args);
  globalThis.GetPlayerMaxArmour = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_MAX_ARMOUR", "int", args);
  globalThis.GetPlayerMaxHealth = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_MAX_HEALTH", "int", args);
  globalThis.GetPlayerPeerStatistics = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_PEER_STATISTICS", "int", args);
  globalThis.GetPlayerRoutingBucket = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_ROUTING_BUCKET", "int", args);
  globalThis.GetPlayerTeam = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_TEAM", "int", args);
  globalThis.GetPlayerTimeInPursuit = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_TIME_IN_PURSUIT", "int", args);
  globalThis.GetPlayerTimeOnline = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_TIME_ONLINE", "int", args);
  globalThis.GetPlayerWantedCentrePosition = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_WANTED_CENTRE_POSITION", "Vector3", args);
  globalThis.GetPlayerWantedLevel = (...args) =>
    InvokeCfxServerNative("GET_PLAYER_WANTED_LEVEL", "int", args);
  globalThis.GetSeatPedIsUsing = (...args) =>
    InvokeCfxServerNative("GET_SEAT_PED_IS_USING", "int", args);
  globalThis.GetSelectedPedWeapon = (...args) =>
    InvokeCfxServerNative("GET_SELECTED_PED_WEAPON", "Hash", args);
  globalThis.GetThrusterSideRcsThrottle = (...args) =>
    InvokeCfxServerNative("GET_THRUSTER_SIDE_RCS_THROTTLE", "float", args);
  globalThis.GetThrusterThrottle = (...args) =>
    InvokeCfxServerNative("GET_THRUSTER_THROTTLE", "float", args);
  globalThis.GetTrainBackwardCarriage = (...args) =>
    InvokeCfxServerNative("GET_TRAIN_BACKWARD_CARRIAGE", "int", args);
  globalThis.GetTrainCarriageEngine = (...args) =>
    InvokeCfxServerNative("GET_TRAIN_CARRIAGE_ENGINE", "int", args);
  globalThis.GetTrainCarriageIndex = (...args) =>
    InvokeCfxServerNative("GET_TRAIN_CARRIAGE_INDEX", "int", args);
  globalThis.GetTrainForwardCarriage = (...args) =>
    InvokeCfxServerNative("GET_TRAIN_FORWARD_CARRIAGE", "int", args);
  globalThis.GetVehicleBodyHealth = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_BODY_HEALTH", "float", args);
  globalThis.GetVehicleColours = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_COLOURS", "void", args);
  globalThis.GetVehicleCustomPrimaryColour = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_CUSTOM_PRIMARY_COLOUR", "void", args);
  globalThis.GetVehicleCustomSecondaryColour = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_CUSTOM_SECONDARY_COLOUR", "void", args);
  globalThis.GetVehicleDashboardColour = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_DASHBOARD_COLOUR", "void", args);
  globalThis.GetVehicleDirtLevel = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_DIRT_LEVEL", "float", args);
  globalThis.GetVehicleDoorsLockedForPlayer = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_DOORS_LOCKED_FOR_PLAYER", "int", args);
  globalThis.GetVehicleDoorLockStatus = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_DOOR_LOCK_STATUS", "int", args);
  globalThis.GetVehicleDoorStatus = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_DOOR_STATUS", "int", args);
  globalThis.GetVehicleEngineHealth = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_ENGINE_HEALTH", "float", args);
  globalThis.GetVehicleExtraColours = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_EXTRA_COLOURS", "void", args);
  globalThis.GetVehicleFlightNozzlePosition = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_FLIGHT_NOZZLE_POSITION", "float", args);
  globalThis.GetVehicleHeadlightsColour = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_HEADLIGHTS_COLOUR", "int", args);
  globalThis.GetVehicleHomingLockonState = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_HOMING_LOCKON_STATE", "int", args);
  globalThis.GetVehicleHornType = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_HORN_TYPE", "Hash", args);
  globalThis.GetVehicleInteriorColour = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_INTERIOR_COLOUR", "void", args);
  globalThis.GetVehicleLightsState = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_LIGHTS_STATE", "BOOL", args);
  globalThis.GetVehicleLivery = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_LIVERY", "int", args);
  globalThis.GetVehicleLockOnTarget = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_LOCK_ON_TARGET", "Vehicle", args);
  globalThis.GetVehicleNeonColour = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_NEON_COLOUR", "void", args);
  globalThis.GetVehicleNeonEnabled = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_NEON_ENABLED", "BOOL", args);
  globalThis.GetVehicleNumberPlateText = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_NUMBER_PLATE_TEXT", "char*", args);
  globalThis.GetVehicleNumberPlateTextIndex = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_NUMBER_PLATE_TEXT_INDEX", "int", args);
  globalThis.GetVehiclePedIsIn = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_PED_IS_IN", "Vehicle", args);
  globalThis.GetVehiclePetrolTankHealth = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_PETROL_TANK_HEALTH", "float", args);
  globalThis.GetVehicleRadioStationIndex = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_RADIO_STATION_INDEX", "int", args);
  globalThis.GetVehicleRoofLivery = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_ROOF_LIVERY", "int", args);
  globalThis.GetVehicleTotalRepairs = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_TOTAL_REPAIRS", "int", args);
  globalThis.GetVehicleTyreSmokeColor = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_TYRE_SMOKE_COLOR", "void", args);
  globalThis.GetVehicleWheelType = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_WHEEL_TYPE", "int", args);
  globalThis.GetVehicleWindowTint = (...args) =>
    InvokeCfxServerNative("GET_VEHICLE_WINDOW_TINT", "int", args);
  globalThis.GiveWeaponComponentToPed = (...args) =>
    InvokeCfxServerNative("GIVE_WEAPON_COMPONENT_TO_PED", "void", args);
  globalThis.GiveWeaponToPed = (...args) =>
    InvokeCfxServerNative("GIVE_WEAPON_TO_PED", "void", args);
  globalThis.HasEntityBeenMarkedAsNoLongerNeeded = (...args) =>
    InvokeCfxServerNative("HAS_ENTITY_BEEN_MARKED_AS_NO_LONGER_NEEDED", "BOOL", args);
  globalThis.HasVehicleBeenDamagedByBullets = (...args) =>
    InvokeCfxServerNative("HAS_VEHICLE_BEEN_DAMAGED_BY_BULLETS", "BOOL", args);
  globalThis.HasVehicleBeenOwnedByPlayer = (...args) =>
    InvokeCfxServerNative("HAS_VEHICLE_BEEN_OWNED_BY_PLAYER", "BOOL", args);
  globalThis.IsBoatAnchoredAndFrozen = (...args) =>
    InvokeCfxServerNative("IS_BOAT_ANCHORED_AND_FROZEN", "bool", args);
  globalThis.IsBoatWrecked = (...args) =>
    InvokeCfxServerNative("IS_BOAT_WRECKED", "bool", args);
  globalThis.IsEntityVisible = (...args) =>
    InvokeCfxServerNative("IS_ENTITY_VISIBLE", "BOOL", args);
  globalThis.IsFlashLightOn = (...args) =>
    InvokeCfxServerNative("IS_FLASH_LIGHT_ON", "bool", args);
  globalThis.IsHeliTailBoomBreakable = (...args) =>
    InvokeCfxServerNative("IS_HELI_TAIL_BOOM_BREAKABLE", "BOOL", args);
  globalThis.IsHeliTailBoomBroken = (...args) =>
    InvokeCfxServerNative("IS_HELI_TAIL_BOOM_BROKEN", "BOOL", args);
  globalThis.IsPedAPlayer = (...args) =>
    InvokeCfxServerNative("IS_PED_A_PLAYER", "BOOL", args);
  globalThis.IsPedHandcuffed = (...args) =>
    InvokeCfxServerNative("IS_PED_HANDCUFFED", "bool", args);
  globalThis.IsPedInAnyVehicle = (...args) =>
    InvokeCfxServerNative("IS_PED_IN_ANY_VEHICLE", "BOOL", args);
  globalThis.IsPedInVehicle = (...args) =>
    InvokeCfxServerNative("IS_PED_IN_VEHICLE", "BOOL", args);
  globalThis.IsPedOnMount = (...args) =>
    InvokeCfxServerNative("IS_PED_ON_MOUNT", "BOOL", args);
  globalThis.IsPedRagdoll = (...args) =>
    InvokeCfxServerNative("IS_PED_RAGDOLL", "bool", args);
  globalThis.IsPedStrafing = (...args) =>
    InvokeCfxServerNative("IS_PED_STRAFING", "bool", args);
  globalThis.IsPedUsingActionMode = (...args) =>
    InvokeCfxServerNative("IS_PED_USING_ACTION_MODE", "bool", args);
  globalThis.IsPlayerAceAllowed = (...args) =>
    InvokeCfxServerNative("IS_PLAYER_ACE_ALLOWED", "BOOL", args);
  globalThis.IsPlayerCommerceInfoLoaded = (...args) =>
    InvokeCfxServerNative("IS_PLAYER_COMMERCE_INFO_LOADED", "BOOL", args);
  globalThis.IsPlayerCommerceInfoLoadedExt = (...args) =>
    InvokeCfxServerNative("IS_PLAYER_COMMERCE_INFO_LOADED_EXT", "BOOL", args);
  globalThis.IsPlayerEvadingWantedLevel = (...args) =>
    InvokeCfxServerNative("IS_PLAYER_EVADING_WANTED_LEVEL", "BOOL", args);
  globalThis.IsPlayerInFreeCamMode = (...args) =>
    InvokeCfxServerNative("IS_PLAYER_IN_FREE_CAM_MODE", "bool", args);
  globalThis.IsPlayerUsingSuperJump = (...args) =>
    InvokeCfxServerNative("IS_PLAYER_USING_SUPER_JUMP", "BOOL", args);
  globalThis.IsTrainCaboose = (...args) =>
    InvokeCfxServerNative("IS_TRAIN_CABOOSE", "bool", args);
  globalThis.IsVehicleExtraTurnedOn = (...args) =>
    InvokeCfxServerNative("IS_VEHICLE_EXTRA_TURNED_ON", "BOOL", args);
  globalThis.IsVehicleSirenOn = (...args) =>
    InvokeCfxServerNative("IS_VEHICLE_SIREN_ON", "BOOL", args);
  globalThis.IsVehicleTyreBurst = (...args) =>
    InvokeCfxServerNative("IS_VEHICLE_TYRE_BURST", "BOOL", args);
  globalThis.IsVehicleWindowIntact = (...args) =>
    InvokeCfxServerNative("IS_VEHICLE_WINDOW_INTACT", "BOOL", args);
  globalThis.LoadPlayerCommerceData = (...args) =>
    InvokeCfxServerNative("LOAD_PLAYER_COMMERCE_DATA", "void", args);
  globalThis.LoadPlayerCommerceDataExt = (...args) =>
    InvokeCfxServerNative("LOAD_PLAYER_COMMERCE_DATA_EXT", "void", args);
  globalThis.MumbleCreateChannel = (...args) =>
    InvokeCfxServerNative("MUMBLE_CREATE_CHANNEL", "void", args);
  globalThis.MumbleIsPlayerMuted = (...args) =>
    InvokeCfxServerNative("MUMBLE_IS_PLAYER_MUTED", "BOOL", args);
  globalThis.MumbleSetPlayerMuted = (...args) =>
    InvokeCfxServerNative("MUMBLE_SET_PLAYER_MUTED", "void", args);
  globalThis.NetworkGetEntityFromNetworkId = (...args) =>
    InvokeCfxServerNative("NETWORK_GET_ENTITY_FROM_NETWORK_ID", "Entity", args);
  globalThis.NetworkGetFirstEntityOwner = (...args) =>
    InvokeCfxServerNative("NETWORK_GET_FIRST_ENTITY_OWNER", "int", args);
  globalThis.NetworkGetNetworkIdFromEntity = (...args) =>
    InvokeCfxServerNative("NETWORK_GET_NETWORK_ID_FROM_ENTITY", "int", args);
  globalThis.NetworkGetVoiceProximityOverrideForPlayer = (...args) =>
    InvokeCfxServerNative("NETWORK_GET_VOICE_PROXIMITY_OVERRIDE_FOR_PLAYER", "Vector3", args);
  globalThis.PerformHttpRequestInternal = (...args) =>
    InvokeCfxServerNative("PERFORM_HTTP_REQUEST_INTERNAL", "int", args);
  globalThis.PerformHttpRequestInternalEx = (...args) =>
    InvokeCfxServerNative("PERFORM_HTTP_REQUEST_INTERNAL_EX", "int", args);
  globalThis.PrintStructuredTrace = (...args) =>
    InvokeCfxServerNative("PRINT_STRUCTURED_TRACE", "void", args);
  globalThis.RegisterConsoleListener = (...args) =>
    InvokeCfxServerNative("REGISTER_CONSOLE_LISTENER", "void", args);
  globalThis.RegisterResourceAsset = (...args) =>
    InvokeCfxServerNative("REGISTER_RESOURCE_ASSET", "char*", args);
  globalThis.RegisterResourceBuildTaskFactory = (...args) =>
    InvokeCfxServerNative("REGISTER_RESOURCE_BUILD_TASK_FACTORY", "void", args);
  globalThis.RemoveAllPedWeapons = (...args) =>
    InvokeCfxServerNative("REMOVE_ALL_PED_WEAPONS", "void", args);
  globalThis.RemoveBlip = (...args) =>
    InvokeCfxServerNative("REMOVE_BLIP", "void", args);
  globalThis.RemoveWeaponComponentFromPed = (...args) =>
    InvokeCfxServerNative("REMOVE_WEAPON_COMPONENT_FROM_PED", "void", args);
  globalThis.RemoveWeaponFromPed = (...args) =>
    InvokeCfxServerNative("REMOVE_WEAPON_FROM_PED", "void", args);
  globalThis.RequestPlayerCommerceSession = (...args) =>
    InvokeCfxServerNative("REQUEST_PLAYER_COMMERCE_SESSION", "void", args);
  globalThis.ScanResourceRoot = (...args) =>
    InvokeCfxServerNative("SCAN_RESOURCE_ROOT", "void", args);
  globalThis.ScheduleResourceTick = (...args) =>
    InvokeCfxServerNative("SCHEDULE_RESOURCE_TICK", "void", args);
  globalThis.SetBlipSprite = (...args) =>
    InvokeCfxServerNative("SET_BLIP_SPRITE", "void", args);
  globalThis.SetCurrentPedWeapon = (...args) =>
    InvokeCfxServerNative("SET_CURRENT_PED_WEAPON", "void", args);
  globalThis.SetEntityDistanceCullingRadius = (...args) =>
    InvokeCfxServerNative("SET_ENTITY_DISTANCE_CULLING_RADIUS", "void", args);
  globalThis.SetEntityIgnoreRequestControlFilter = (...args) =>
    InvokeCfxServerNative("SET_ENTITY_IGNORE_REQUEST_CONTROL_FILTER", "void", args);
  globalThis.SetEntityOrphanMode = (...args) =>
    InvokeCfxServerNative("SET_ENTITY_ORPHAN_MODE", "void", args);
  globalThis.SetEntityRemoteSyncedScenesAllowed = (...args) =>
    InvokeCfxServerNative("SET_ENTITY_REMOTE_SYNCED_SCENES_ALLOWED", "void", args);
  globalThis.SetEntityRotation = (...args) =>
    InvokeCfxServerNative("SET_ENTITY_ROTATION", "void", args);
  globalThis.SetEntityRoutingBucket = (...args) =>
    InvokeCfxServerNative("SET_ENTITY_ROUTING_BUCKET", "void", args);
  globalThis.SetEntityVelocity = (...args) =>
    InvokeCfxServerNative("SET_ENTITY_VELOCITY", "void", args);
  globalThis.SetGameType = (...args) =>
    InvokeCfxServerNative("SET_GAME_TYPE", "void", args);
  globalThis.SetHttpHandler = (...args) =>
    InvokeCfxServerNative("SET_HTTP_HANDLER", "void", args);
  globalThis.SetMapName = (...args) =>
    InvokeCfxServerNative("SET_MAP_NAME", "void", args);
  globalThis.SetPedAmmo = (...args) =>
    InvokeCfxServerNative("SET_PED_AMMO", "void", args);
  globalThis.SetPedArmour = (...args) =>
    InvokeCfxServerNative("SET_PED_ARMOUR", "void", args);
  globalThis.SetPedCanRagdoll = (...args) =>
    InvokeCfxServerNative("SET_PED_CAN_RAGDOLL", "void", args);
  globalThis.SetPedComponentVariation = (...args) =>
    InvokeCfxServerNative("SET_PED_COMPONENT_VARIATION", "void", args);
  globalThis.SetPedConfigFlag = (...args) =>
    InvokeCfxServerNative("SET_PED_CONFIG_FLAG", "void", args);
  globalThis.SetPedDefaultComponentVariation = (...args) =>
    InvokeCfxServerNative("SET_PED_DEFAULT_COMPONENT_VARIATION", "void", args);
  globalThis.SetPedHairTint = (...args) =>
    InvokeCfxServerNative("SET_PED_HAIR_TINT", "void", args);
  globalThis.SetPedHeadBlendData = (...args) =>
    InvokeCfxServerNative("SET_PED_HEAD_BLEND_DATA", "void", args);
  globalThis.SetPedHeadOverlay = (...args) =>
    InvokeCfxServerNative("SET_PED_HEAD_OVERLAY", "void", args);
  globalThis.SetPedIntoVehicle = (...args) =>
    InvokeCfxServerNative("SET_PED_INTO_VEHICLE", "void", args);
  globalThis.SetPedPropIndex = (...args) =>
    InvokeCfxServerNative("SET_PED_PROP_INDEX", "void", args);
  globalThis.SetPedRandomComponentVariation = (...args) =>
    InvokeCfxServerNative("SET_PED_RANDOM_COMPONENT_VARIATION", "void", args);
  globalThis.SetPedRandomProps = (...args) =>
    InvokeCfxServerNative("SET_PED_RANDOM_PROPS", "void", args);
  globalThis.SetPedResetFlag = (...args) =>
    InvokeCfxServerNative("SET_PED_RESET_FLAG", "void", args);
  globalThis.SetPedToRagdoll = (...args) =>
    InvokeCfxServerNative("SET_PED_TO_RAGDOLL", "void", args);
  globalThis.SetPedToRagdollWithFall = (...args) =>
    InvokeCfxServerNative("SET_PED_TO_RAGDOLL_WITH_FALL", "void", args);
  globalThis.SetPlayerControl = (...args) =>
    InvokeCfxServerNative("SET_PLAYER_CONTROL", "void", args);
  globalThis.SetPlayerCullingRadius = (...args) =>
    InvokeCfxServerNative("SET_PLAYER_CULLING_RADIUS", "void", args);
  globalThis.SetPlayerInvincible = (...args) =>
    InvokeCfxServerNative("SET_PLAYER_INVINCIBLE", "void", args);
  globalThis.SetPlayerRoutingBucket = (...args) =>
    InvokeCfxServerNative("SET_PLAYER_ROUTING_BUCKET", "void", args);
  globalThis.SetPlayerWantedLevel = (...args) =>
    InvokeCfxServerNative("SET_PLAYER_WANTED_LEVEL", "void", args);
  globalThis.SetRoutingBucketEntityLockdownMode = (...args) =>
    InvokeCfxServerNative("SET_ROUTING_BUCKET_ENTITY_LOCKDOWN_MODE", "void", args);
  globalThis.SetRoutingBucketPopulationEnabled = (...args) =>
    InvokeCfxServerNative("SET_ROUTING_BUCKET_POPULATION_ENABLED", "void", args);
  globalThis.SetVehicleAlarm = (...args) =>
    InvokeCfxServerNative("SET_VEHICLE_ALARM", "void", args);
  globalThis.SetVehicleBodyHealth = (...args) =>
    InvokeCfxServerNative("SET_VEHICLE_BODY_HEALTH", "void", args);
  globalThis.SetVehicleColours = (...args) =>
    InvokeCfxServerNative("SET_VEHICLE_COLOURS", "void", args);
  globalThis.SetVehicleColourCombination = (...args) =>
    InvokeCfxServerNative("SET_VEHICLE_COLOUR_COMBINATION", "void", args);
  globalThis.SetVehicleCustomPrimaryColour = (...args) =>
    InvokeCfxServerNative("SET_VEHICLE_CUSTOM_PRIMARY_COLOUR", "void", args);
  globalThis.SetVehicleCustomSecondaryColour = (...args) =>
    InvokeCfxServerNative("SET_VEHICLE_CUSTOM_SECONDARY_COLOUR", "void", args);
  globalThis.SetVehicleDirtLevel = (...args) =>
    InvokeCfxServerNative("SET_VEHICLE_DIRT_LEVEL", "void", args);
  globalThis.SetVehicleDoorBroken = (...args) =>
    InvokeCfxServerNative("SET_VEHICLE_DOOR_BROKEN", "void", args);
  globalThis.SetVehicleNumberPlateText = (...args) =>
    InvokeCfxServerNative("SET_VEHICLE_NUMBER_PLATE_TEXT", "void", args);
  globalThis.StartResource = (...args) =>
    InvokeCfxServerNative("START_RESOURCE", "BOOL", args);
  globalThis.StopResource = (...args) =>
    InvokeCfxServerNative("STOP_RESOURCE", "BOOL", args);
  globalThis.TaskCombatPed = (...args) =>
    InvokeCfxServerNative("TASK_COMBAT_PED", "void", args);
  globalThis.TaskDriveBy = (...args) =>
    InvokeCfxServerNative("TASK_DRIVE_BY", "void", args);
  globalThis.TaskEnterVehicle = (...args) =>
    InvokeCfxServerNative("TASK_ENTER_VEHICLE", "void", args);
  globalThis.TaskEveryoneLeaveVehicle = (...args) =>
    InvokeCfxServerNative("TASK_EVERYONE_LEAVE_VEHICLE", "void", args);
  globalThis.TaskGoStraightToCoord = (...args) =>
    InvokeCfxServerNative("TASK_GO_STRAIGHT_TO_COORD", "void", args);
  globalThis.TaskGoToCoordAnyMeans = (...args) =>
    InvokeCfxServerNative("TASK_GO_TO_COORD_ANY_MEANS", "void", args);
  globalThis.TaskGoToEntity = (...args) =>
    InvokeCfxServerNative("TASK_GO_TO_ENTITY", "void", args);
  globalThis.TaskHandsUp = (...args) =>
    InvokeCfxServerNative("TASK_HANDS_UP", "void", args);
  globalThis.TaskLeaveAnyVehicle = (...args) =>
    InvokeCfxServerNative("TASK_LEAVE_ANY_VEHICLE", "void", args);
  globalThis.TaskLeaveVehicle = (...args) =>
    InvokeCfxServerNative("TASK_LEAVE_VEHICLE", "void", args);
  globalThis.TaskPlayAnim = (...args) =>
    InvokeCfxServerNative("TASK_PLAY_ANIM", "void", args);
  globalThis.TaskPlayAnimAdvanced = (...args) =>
    InvokeCfxServerNative("TASK_PLAY_ANIM_ADVANCED", "void", args);
  globalThis.TaskReactAndFleePed = (...args) =>
    InvokeCfxServerNative("TASK_REACT_AND_FLEE_PED", "void", args);
  globalThis.TaskShootAtCoord = (...args) =>
    InvokeCfxServerNative("TASK_SHOOT_AT_COORD", "void", args);
  globalThis.TaskShootAtEntity = (...args) =>
    InvokeCfxServerNative("TASK_SHOOT_AT_ENTITY", "void", args);
  globalThis.TaskWarpPedIntoVehicle = (...args) =>
    InvokeCfxServerNative("TASK_WARP_PED_INTO_VEHICLE", "void", args);
  globalThis.TempBanPlayer = (...args) =>
    InvokeCfxServerNative("TEMP_BAN_PLAYER", "void", args);
  globalThis.TriggerClientEventInternal = (...args) =>
    InvokeCfxServerNative("TRIGGER_CLIENT_EVENT_INTERNAL", "void", args);
  globalThis.TriggerLatentClientEventInternal = (...args) =>
    InvokeCfxServerNative("TRIGGER_LATENT_CLIENT_EVENT_INTERNAL", "void", args);
  globalThis.VerifyPasswordHash = (...args) =>
    InvokeCfxServerNative("VERIFY_PASSWORD_HASH", "BOOL", args);
  globalThis.AddBlipForArea = (...args) =>
    InvokeCfxServerNative("_ADD_BLIP_FOR_AREA", "Blip", args);
  globalThis.SetPedEyeColor = (...args) =>
    InvokeCfxServerNative("_SET_PED_EYE_COLOR", "void", args);
  globalThis.SetPedFaceFeature = (...args) =>
    InvokeCfxServerNative("_SET_PED_FACE_FEATURE", "void", args);
  globalThis.SetPedHeadOverlayColor = (...args) =>
    InvokeCfxServerNative("_SET_PED_HEAD_OVERLAY_COLOR", "void", args);

  // Phase A timer stubs: fire on the microtask queue, ignoring the delay.
  globalThis.setTimeout = (fn) => {
    Promise.resolve().then(fn);
    return 0;
  };
  globalThis.setInterval = () => 0;
  globalThis.clearTimeout = () => {};
  globalThis.clearInterval = () => {};
})(globalThis);
