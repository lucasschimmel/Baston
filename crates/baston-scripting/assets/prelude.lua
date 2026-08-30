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
}
