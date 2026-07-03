-- baston-decrypt-shim — sidecar resource for CFX Asset Escrow decryption.
--
-- Runs inside a minimal FXServer subprocess launched by baston-escrow-plugin.
-- FXServer + svadhesive decrypt escrow files transparently on read; this shim
-- exposes that over a line-delimited JSON protocol on stdin/stdout so BASTON
-- (a separate process) can obtain the plaintext.
fx_version 'cerulean'
game 'gta5'

author 'Ship Labs'
description 'BASTON escrow decrypt sidecar shim'
version '0.1.0'

server_script 'server.lua'
