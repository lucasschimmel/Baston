param(
    [string]$Log = "info,udp=debug"
)

$ErrorActionPreference = "Stop"
$Root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $Root
$env:BASTON_CONFIG = "baston.gateway-onesync.local.toml"
$env:RUST_LOG = $Log
& "C:\Users\osiri\.cache\baston-target\release\baston-gateway.exe"
