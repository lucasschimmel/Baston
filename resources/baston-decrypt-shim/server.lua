-- baston-decrypt-shim — server side of the CFX sidecar protocol.
--
-- This resource runs inside a genuine, unmodified FXServer (the CFX component's
-- native host). It answers BASTON over a line-delimited JSON protocol. Two ops:
--
--   op = "decrypt"        (default when "op" is absent)
--     request : { "op": "decrypt", "resource": <name>, "file": <relpath>, "data": <base64> }
--     reply   : { "data": <base64-plaintext> }   on success
--               { "error": <message> }            on failure
--     The request `data` is ignored: FXServer/svadhesive has already decrypted
--     the file on disk via its VFS hook, so the shim reads the resolved path and
--     returns the (now plaintext) bytes.
--
--   op = "license_status"
--     request : { "op": "license_status" }
--     reply   : { "valid": bool, "banned": bool,
--                 "entitlements": { "features": [...] }, "reason": <string?> }
--     Reads `sv_licenseKeyToken` via GetConvar — a purely LOCAL signal. When the
--     official CFX component has validated the operator's licence, that convar is
--     non-empty. This resource makes NO network call; the genuine FXServer it
--     runs inside is the only thing that ever talked to CFX. A missing token is
--     reported as `valid=false` (key missing / invalid / not yet validated); the
--     ban vs plain-invalid distinction is not locally available, so `banned` is
--     always false and the caller treats any `valid=false` as a hard stop.
--
-- NOTE (known limitation): direct stdin/stdout streaming from an FXServer
-- resource is constrained by the server sandbox. On builds where `io.read`
-- against the process stdin is unavailable, drive this shim through the
-- alternate file-drop transport documented in docs/escrow-support.md. The Rust
-- side (SidecarDecryptor) speaks the JSON line protocol regardless of transport.

local function b64encode(bytes)
  -- CitizenFX exposes Base64 helpers via the crypto natives when available.
  if Base64Encode then
    return Base64Encode(bytes)
  end
  error('no base64 encoder available in this FXServer build')
end

local function respond(obj)
  io.write(json.encode(obj) .. '\n')
  io.flush()
end

-- Signal readiness to the parent (BASTON) exactly once.
io.write('READY\n')
io.flush()

while true do
  local line = io.read('l')
  if not line then
    break
  end

  local ok, req = pcall(json.decode, line)
  if not ok or type(req) ~= 'table' then
    respond({ error = 'invalid json' })
  else
    local op = req.op or 'decrypt'
    if op == 'license_status' then
      -- Local read of the official component's verdict. No network here.
      local token = GetConvar('sv_licenseKeyToken', '')
      local valid = token ~= nil and token ~= ''
      respond({
        valid = valid,
        banned = false, -- not locally distinguishable from invalid
        entitlements = { features = {} }, -- no clean local entitlement signal
        reason = (not valid)
          and 'sv_licenseKeyToken empty: licence key missing, invalid, or not yet validated by the CFX component'
          or nil,
      })
    elseif op == 'decrypt' then
      local resourcePath = GetResourcePath(req.resource)
      if not resourcePath then
        respond({ error = 'unknown resource: ' .. tostring(req.resource) })
      else
        local filePath = resourcePath .. '/' .. req.file
        local f = io.open(filePath, 'rb')
        if not f then
          respond({ error = 'file not found: ' .. filePath })
        else
          local data = f:read('*all')
          f:close()
          -- Bytes read here are already decrypted by svadhesive's IO hook.
          respond({ data = b64encode(data) })
        end
      end
    else
      respond({ error = 'unknown op: ' .. tostring(op) })
    end
  end
end
