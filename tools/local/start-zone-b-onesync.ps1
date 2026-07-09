param(
    [string]$Log = "info"
)

$ErrorActionPreference = "Stop"
$Root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $Root
$env:BASTON_CONFIG = "baston.zone-b-onesync.local.toml"
$env:RUST_LOG = $Log
& "C:\Users\osiri\.cache\baston-target\release\baston-zone.exe"
