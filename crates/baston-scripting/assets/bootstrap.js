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

  function dispatch(name, argsJson) {
    const handlers = eventHandlers.get(name);
    if (!handlers || handlers.size === 0) return;
    const args = JSON.parse(argsJson);
    for (const fn of [...handlers.values()]) {
      try {
        fn(...args);
      } catch (e) {
        console.error(`[baston] error in '${name}' handler: ${stringify(e)}`);
      }
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
    for (const fn of [...handlers.values()]) {
      try {
        fn(playerName, setKickReason, deferrals);
      } catch (e) {
        console.error(
          `[baston] error in playerConnecting handler: ${stringify(e)}`
        );
        ops.op_deferral_done(source, "server error in playerConnecting handler");
      }
    }
  }

  globalThis.__baston = { dispatch, dispatchPlayerConnecting };

  // --- FiveM-style globals ---

  globalThis.AddEventHandler = AddEventHandler;
  globalThis.on = AddEventHandler;
  globalThis.onNet = AddEventHandler; // Phase A: net events behave like local events
  globalThis.RemoveEventHandler = RemoveEventHandler;
  globalThis.TriggerEvent = TriggerEvent;
  globalThis.emit = TriggerEvent;
  globalThis.RegisterNetEvent = function () {}; // Phase A stub
  globalThis.exports = exportsProxy;
  globalThis.GetCurrentResourceName = () => ops.op_get_current_resource_name();
  globalThis.GetGameTimer = () => ops.op_get_game_timer();
  globalThis.GetNumPlayerIndices = () => ops.op_get_num_player_indices();
  globalThis.GetPlayerFromIndex = (i) => ops.op_get_player_from_index(i >>> 0);
  globalThis.GetPlayerName = (source) => ops.op_get_player_name(source >>> 0);
  globalThis.GetPlayerIdentifierByType = (source, type) =>
    ops.op_get_player_identifier_by_type(source >>> 0, String(type));
  globalThis.GetPlayers = () => {
    const n = ops.op_get_num_player_indices();
    const out = [];
    for (let i = 0; i < n; i++) out.push(ops.op_get_player_from_index(i));
    return out;
  };

  // Phase A timer stubs: fire on the microtask queue, ignoring the delay.
  globalThis.setTimeout = (fn) => {
    Promise.resolve().then(fn);
    return 0;
  };
  globalThis.setInterval = () => 0;
  globalThis.clearTimeout = () => {};
  globalThis.clearInterval = () => {};
})(globalThis);
