-- baston-decrypt-shim — server side of the escrow sidecar protocol.
--
-- Protocol (one JSON object per line):
--   request : { "resource": <name>, "file": <relpath>, "data": <base64> }
--   reply   : { "data": <base64-plaintext> }   on success
--             { "error": <message> }            on failure
--
-- The `data` in the request is ignored: FXServer/svadhesive has already
-- decrypted the file on disk via its VFS hook, so the shim simply reads the
-- resolved path and returns the (now plaintext) bytes. `data` is kept in the
-- request for symmetry and future in-band transfer.
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
  end
end
