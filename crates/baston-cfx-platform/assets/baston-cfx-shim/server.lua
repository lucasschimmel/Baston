-- baston-cfx-shim — server side of the BASTON ⇄ CFX sidecar protocol.
--
-- ## Why a file-drop channel (not stdin/stdout)
--
-- The CitizenFX **server** Lua sandbox does NOT expose `io.read`: the `read`
-- entry is absent from the `io` library and `stdin` is an empty stub
-- (engine-source: citizen-scripting-lua/src/LuaIO.cpp — `iolib[]`, `openio`).
-- `io.write` is redirected to the console. So the old stdin/stdout line protocol
-- could never work inside a real FXServer. Instead we use a **file-drop** channel
-- built on sanctioned natives:
--   • BASTON writes  <resource>/ipc/request.json   (atomically, one at a time)
--   • the shim reads it via an ABSOLUTE-path io.open (bypasses the resource
--     file-list snapshot; read of an absolute path is always permitted)
--   • the shim writes <resource>/ipc/response.json via SaveResourceFile (writing
--     inside the resource's own directory is always permitted —
--     citizen-scripting-core/src/FilesystemPermissions.cpp)
--   • request/response are matched by a monotonic `id`, so a stale or
--     half-written file is simply ignored and retried.
--
-- ## Two operations
--
--   op = "license_status"
--     reply : { id, valid, banned, token?, entitlements = { features = {} }, reason? }
--     Reads sv_licenseKeyToken via GetConvar — a purely LOCAL signal populated by
--     the official CFX component once it validated the operator's licence. No
--     network call here. Ban vs plain-invalid is not locally distinguishable, so
--     `banned` is always false and BASTON treats any `valid=false` as a hard stop.
--
--   op = "decrypt"
--     request : { id, resource, file }
--     reply   : { id, data = <base64 plaintext> } | { id, error = <message> }
--     Reads the resource file through LoadResourceFile: svadhesive has already
--     decrypted it on read via its VFS hook, so we return the (now plaintext)
--     bytes, base64-encoded.
--
-- This shim is materialised on disk by baston-cfx-platform (kept in lockstep
-- with the Rust `Sidecar`). It never contacts CFX.

local RESOURCE = GetCurrentResourceName()
local IPC_DIR = GetResourcePath(RESOURCE) .. '/ipc'
local REQUEST_ABS = IPC_DIR .. '/request.json'
local RESPONSE_REL = 'ipc/response.json'
local READY_REL = 'ipc/ready.json'
local POLL_MS = 20
local PROTOCOL = 2

-- Pure-Lua base64 (no dependency on a build-specific native). Only exercised by
-- the escrow `decrypt` op, once per encrypted file at load — never on a hot path.
local B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'
local function base64encode(data)
  return ((data:gsub('.', function(x)
    local r, b = '', x:byte()
    for i = 8, 1, -1 do r = r .. (b % 2 ^ i - b % 2 ^ (i - 1) > 0 and '1' or '0') end
    return r
  end) .. '0000'):gsub('%d%d%d?%d?%d?%d?', function(x)
    if #x < 6 then return '' end
    local c = 0
    for i = 1, 6 do c = c + (x:sub(i, i) == '1' and 2 ^ (6 - i) or 0) end
    return B64:sub(c + 1, c + 1)
  end) .. ({ '', '==', '=' })[#data % 3 + 1])
end

-- Read a whole file by absolute path. Reads go straight to the local disk device
-- (VFS `FindDevice`), so BASTON's freshly-written request.json is always visible
-- even though it was created after resource mount. Plaintext IPC only — this path
-- deliberately does not route through the escrow decrypt hook.
local function readAbsolute(path)
  local f = io.open(path, 'rb')
  if not f then return nil end
  local data = f:read('a')
  f:close()
  return data
end

local function writeResponse(obj)
  SaveResourceFile(RESOURCE, RESPONSE_REL, json.encode(obj), -1)
end

-- Local read of the official component's verdict. No network here.
local function licenseStatus()
  local token = GetConvar('sv_licenseKeyToken', '')
  local valid = token ~= nil and token ~= ''
  return {
    valid = valid,
    banned = false, -- not locally distinguishable from invalid
    token = valid and token or nil,
    entitlements = { features = {} }, -- no clean local entitlement signal (see docs)
    reason = (not valid)
        and 'sv_licenseKeyToken empty: licence key missing, invalid, or not yet validated by the CFX component'
        or nil,
  }
end

-- Escrow decrypt: read the resource file through the VFS, where svadhesive has
-- already decrypted it. Return the plaintext, base64-encoded.
local function decrypt(req)
  if type(req.resource) ~= 'string' or type(req.file) ~= 'string' then
    return { error = 'decrypt request missing resource/file' }
  end
  local data = LoadResourceFile(req.resource, req.file)
  if data == nil then
    return { error = 'file not found or unreadable: ' .. req.resource .. '/' .. req.file }
  end
  return { data = base64encode(data) }
end

local function handle(req)
  local op = req.op or 'decrypt'
  local result
  if op == 'license_status' then
    result = licenseStatus()
  elseif op == 'decrypt' then
    result = decrypt(req)
  else
    result = { error = 'unknown op: ' .. tostring(op) }
  end
  -- Echo the id as an integer so it round-trips as `1`, not `1.0` (some JSON
  -- encoders would otherwise emit a float the Rust side must still match).
  result.id = math.floor(req.id)
  return result
end

-- Announce readiness exactly once (BASTON waits on this file before querying).
SaveResourceFile(RESOURCE, READY_REL, json.encode({ ready = true, protocol = PROTOCOL }), -1)

local lastId = 0
Citizen.CreateThread(function()
  while true do
    Citizen.Wait(POLL_MS)
    local raw = readAbsolute(REQUEST_ABS)
    if raw then
      local ok, req = pcall(json.decode, raw)
      if ok and type(req) == 'table' and type(req.id) == 'number' and req.id > lastId then
        local okHandle, resp = pcall(handle, req)
        if not okHandle then
          resp = { id = req.id, error = 'shim handler error: ' .. tostring(resp) }
        end
        writeResponse(resp)
        lastId = req.id
      end
    end
  end
end)
