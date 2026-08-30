-- baston-cfx-shim — server side of the BASTON ⇄ CFX sidecar protocol.
--
-- Runs inside a genuine, unmodified FXServer (the CFX component's native host)
-- launched by baston-escrow-plugin. It exposes two purely-local capabilities to
-- BASTON over a file-drop IPC channel (see server.lua and the Rust `Sidecar`):
--   • licence verdict  — reads sv_licenseKeyToken via GetConvar
--   • escrow decrypt   — reads a resource file already decrypted by svadhesive
--
-- The resource makes NO network call. Only the genuine FXServer it runs inside
-- ever talks to CFX, doing exactly what it normally does with the operator's own
-- licence. See docs/operations/licensing.md for the compliance boundary.
fx_version 'cerulean'
game 'gta5'

author 'Ship Labs'
description 'BASTON CFX sidecar shim (licence oracle + escrow decrypt)'
version '0.2.0'

server_only 'yes'
server_script 'server.lua'
