-- BASTON's Lua prelude: the CFX surface a resource expects, built on the two
-- host functions the Rust side installs (`__baston.shared_native` and
-- `__baston.server_native`).
--
-- Mirrors assets/bootstrap.js: the callback registries live here, on the script
-- side, and the host only ever passes JSON across the boundary. Keeping the two
-- preludes symmetrical is what makes one native implementation serve both
-- engines (ADR-002).

local host = __baston
local json_encode, json_decode = host.json_encode, host.json_decode

-- Registries. `__baston.dispatch` reaches these from Rust.
local handlers = {}       -- event name -> { fn, ... }
local net_events = {}     -- event name -> true (registered for client → server)
local commands = {}       -- command name -> fn
local threads = {}        -- coroutines awaiting resumption
local timers = {}         -- { at = ms, fn = f }

-- ---------------------------------------------------------------- natives ---

--- Call a CFX native by name, with the host picking the dispatcher that owns
--- it (shared natives first, then the server-only set).
---
--- `kind` is how the host reads the result back ("string", "int", "float",
--- "bool", "vector3", ...). `InvokeNative` takes no kind — matching CFX, where
--- it is `InvokeNative(hash, ...)` — so it asks for "any" and lets the native
--- table decide.
local function invoke(name, kind, args)
    local raw = host.native(name, kind or "any", args)
    if raw == nil or raw == "" then
        return nil
    end
    return json_decode(raw)
end

Citizen = Citizen or {}

function Citizen.InvokeNative(name, ...)
    return invoke(name, "any", json_encode({ ... }))
end

--- FiveM exposes every native as a global. Rather than generating thousands of
--- stubs, resolve on first use and memoise: an unknown global becomes a native
--- call named after it, in the SCREAMING_SNAKE_CASE the host dispatches on.
local function to_native_name(camel)
    local out = camel:gsub("(%l)(%u)", "%1_%2"):gsub("(%u)(%u%l)", "%1_%2")
    return out:upper()
end

setmetatable(_G, {
    __index = function(_, key)
        if type(key) ~= "string" or key:sub(1, 1):match("%l") then
            return nil
        end
        local native = to_native_name(key)
        local fn = function(...)
            return invoke(native, "any", json_encode({ ... }))
        end
        rawset(_G, key, fn)
        return fn
    end,
})

-- ----------------------------------------------------------------- events ---

function AddEventHandler(event, fn)
    local list = handlers[event]
    if not list then
        list = {}
        handlers[event] = list
        host.add_event_handler(event)
    end
    list[#list + 1] = fn
    return { key = event, index = #list }
end

function RegisterNetEvent(event, fn)
    net_events[event] = true
    host.add_event_handler(event)
    if fn then
        return AddEventHandler(event, fn)
    end
end

function RemoveEventHandler(token)
    if token and handlers[token.key] then
        handlers[token.key][token.index] = nil
    end
end

function TriggerEvent(event, ...)
    host.trigger_event(event, json_encode({ ... }))
end

function TriggerClientEvent(event, target, ...)
    host.trigger_client_event(event, tonumber(target) or 0, json_encode({ ... }))
end

function RegisterCommand(name, fn, restricted)
    commands[name] = fn
    host.register_command(name, restricted and true or false)
end

-- ---------------------------------------------------------------- exports ---

local local_exports = {}

--- `exports('name', fn)` registers; `exports.resource.fn(...)` calls.
---
--- Same two shapes as the JS proxy. Cross-resource calls are not supported
--- yet, and say so rather than returning nil: a silent nil in a Lua resource
--- surfaces hundreds of lines later as "attempt to index a nil value".
exports = setmetatable({}, {
    __call = function(_, name, fn)
        local_exports[name] = fn
        host.add_export(name)
    end,
    __index = function(_, resource)
        local proxy
        proxy = setmetatable({}, {
            __index = function(_, fn_name)
                return function(...)
                    -- Both spellings are idiomatic in the FiveM ecosystem:
                    -- `exports.res:fn(a)` passes the proxy as `self`,
                    -- `exports.res.fn(a)` does not. Drop the receiver rather
                    -- than shifting every argument by one.
                    local args = table.pack(...)
                    local first, count = 1, args.n
                    if count > 0 and args[1] == proxy then
                        first = 2
                    end
                    if resource == host.resource_name() and local_exports[fn_name] then
                        return local_exports[fn_name](table.unpack(args, first, count))
                    end
                    error(("export %s.%s unavailable (no cross-resource exports yet)")
                        :format(tostring(resource), tostring(fn_name)), 2)
                end
            end,
        })
        return proxy
    end,
})

-- -------------------------------------------------------------- state bags ---

local state_bag_handlers = {}

function AddStateBagChangeHandler(key_filter, bag_filter, handler)
    local id = #state_bag_handlers + 1
    state_bag_handlers[id] = handler
    return host.add_state_bag_handler(key_filter or "", bag_filter or "", id)
end

function RemoveStateBagChangeHandler(cookie)
    return host.remove_state_bag_handler(cookie)
end

-- ------------------------------------------- server → client native calls ---

--- Run a GTA native on `source`'s client.
---
--- Without `expects_return` this is fire-and-forget and returns immediately.
--- With it, the call must run inside a `CreateThread` coroutine: the reply
--- comes back over the network, so the script yields until it lands instead of
--- blocking the runtime that would have to deliver it.
function InvokeNativeOnClient(source, hash, args, expects_return)
    -- Checked before the call goes out: a caller who cannot wait for the reply
    -- should not have spent a network round trip discovering that.
    if expects_return and not coroutine.isyieldable() then
        error("InvokeNativeOnClient with a return value must run inside "
            .. "Citizen.CreateThread — the reply arrives on a later tick", 2)
    end
    local id, err = host.invoke_client_native(
        tonumber(source) or 0,
        tostring(hash),
        json_encode(args or {}),
        expects_return and true or false
    )
    if err then
        error("InvokeNativeOnClient: " .. err, 2)
    end
    if not expects_return then
        return nil
    end
    while true do
        local raw = host.poll_client_native(id)
        if raw then
            local result = json_decode(raw)
            if type(result) == "table" and result.__error then
                error("InvokeNativeOnClient: " .. tostring(result.__error), 2)
            end
            return result
        end
        coroutine.yield(0)
    end
end

Citizen.InvokeNativeOnClient = InvokeNativeOnClient

-- --------------------------------------------------- zone transfer (mesh) ---

local zone_transfer_callbacks = {}

--- Register state BASTON must carry with a player across a zone handoff.
function RegisterZoneTransferState(cb)
    if type(cb) ~= "function" then
        return
    end
    zone_transfer_callbacks[#zone_transfer_callbacks + 1] = cb
    host.register_zone_transfer_state()
end

-- ---------------------------------------------------------------- threads ---

--- Cooperative threads, as CFX defines them: `Wait(ms)` yields, and the host
--- resumes the coroutine on a later tick. Nothing here is preemptive — a Lua
--- resource that never yields blocks its own runtime and only its own.
function Citizen.CreateThread(fn)
    local co = coroutine.create(fn)
    threads[#threads + 1] = { co = co, wake_at = 0 }
    return co
end

function Citizen.Wait(ms)
    coroutine.yield(ms or 0)
end

function Citizen.SetTimeout(ms, fn)
    timers[#timers + 1] = { at = host.game_timer() + (ms or 0), fn = fn }
end

CreateThread = Citizen.CreateThread
Wait = Citizen.Wait
SetTimeout = Citizen.SetTimeout

--- Resume every thread whose wait has elapsed, and fire due timers.
---
--- Returns the shortest delay until the next thread wants to run, so the host
--- can idle instead of spinning.
local function tick()
    local now = host.game_timer()
    local next_wake = 50

    for index = #timers, 1, -1 do
        local timer = timers[index]
        if now >= timer.at then
            table.remove(timers, index)
            local ok, err = pcall(timer.fn)
            if not ok then
                host.report_error(tostring(err))
            end
        end
    end

    local alive = {}
    for _, entry in ipairs(threads) do
        if now >= entry.wake_at then
            local ok, result = coroutine.resume(entry.co)
            if not ok then
                host.report_error(tostring(result))
            elseif coroutine.status(entry.co) ~= "dead" then
                entry.wake_at = now + (tonumber(result) or 0)
                alive[#alive + 1] = entry
            end
        else
            alive[#alive + 1] = entry
        end
        if entry.wake_at and entry.wake_at > now then
            next_wake = math.min(next_wake, entry.wake_at - now)
        end
    end
    threads = alive
    return next_wake
end

-- --------------------------------------------------------------- dispatch ---

--- Everything Rust calls into. Kept on one table so the host binds a single
--- Lua value and never reaches into globals a resource could have shadowed.
__baston_dispatch = {
    tick = tick,

    --- Run every handler registered for `event`.
    ---
    --- `source` is set for client → server events and exposed as the CFX
    --- global of the same name for the duration of the dispatch.
    event = function(event, args_json, source)
        local list = handlers[event]
        if not list then
            return 0
        end
        local args = json_decode(args_json) or {}
        local previous = _G.source
        rawset(_G, "source", source)
        local errors = 0
        for _, fn in pairs(list) do
            local ok, err = pcall(fn, table.unpack(args))
            if not ok then
                errors = errors + 1
                host.report_error(tostring(err))
            end
        end
        rawset(_G, "source", previous)
        return errors
    end,

    --- Whether this resource registered for a client → server event. The host
    --- asks before dispatching, so a resource cannot receive net events it
    --- never opted into.
    accepts_net_event = function(event)
        return net_events[event] == true
    end,

    command = function(name, source, args_json, raw)
        local fn = commands[name]
        if not fn then
            return false
        end
        local args = json_decode(args_json) or {}
        local ok, err = pcall(fn, source, args, raw)
        if not ok then
            host.report_error(tostring(err))
        end
        return true
    end,

    --- `playerConnecting(name, setKickReason, deferrals)`.
    ---
    --- Handlers run synchronously; a handler that defers keeps the connection
    --- parked until it calls `deferrals.done()`, exactly as on the JS side.
    --- A handler that throws after deferring would strand the player, so the
    --- connection is released with a server-error reason.
    player_connecting = function(source, player_name)
        local list = handlers["playerConnecting"]
        if not list then
            return 0
        end
        local set_kick_reason = function(reason)
            host.set_kick_reason(source, tostring(reason))
        end
        local deferrals = {
            defer = function() host.deferral_defer(source) end,
            update = function(msg) host.deferral_update(source, tostring(msg)) end,
            done = function(reason)
                host.deferral_done(source, reason == nil and "" or tostring(reason))
            end,
            presentCard = function(card)
                host.deferral_present_card(
                    source,
                    type(card) == "string" and card or json_encode(card)
                )
            end,
        }
        local errors = 0
        for _, fn in pairs(list) do
            local ok, err = pcall(fn, player_name, set_kick_reason, deferrals)
            if not ok then
                errors = errors + 1
                host.report_error(tostring(err))
                host.deferral_done(source, "server error in playerConnecting handler")
            end
        end
        return errors
    end,

    --- Deliver the state-bag changes queued for this resource.
    state_bag_changes = function()
        local deliveries = json_decode(host.poll_state_bag_changes()) or {}
        local errors = 0
        for _, delivery in ipairs(deliveries) do
            local handler = state_bag_handlers[delivery.callback_id]
            if handler then
                local change = delivery.change
                local ok, err = pcall(
                    handler, change.bag, change.key, change.value, 0, change.replicated
                )
                if not ok then
                    errors = errors + 1
                    host.report_error(tostring(err))
                end
            end
        end
        return errors
    end,

    --- Merge every zone-transfer callback into one object for the handoff.
    collect_zone_transfer_state = function(source)
        if #zone_transfer_callbacks == 0 then
            return nil
        end
        local merged = {}
        for _, cb in ipairs(zone_transfer_callbacks) do
            local ok, result = pcall(cb, source)
            if not ok then
                host.report_error(tostring(result))
            elseif type(result) == "table" then
                for k, v in pairs(result) do
                    merged[k] = v
                end
            end
        end
        return json_encode(merged)
    end,
}
