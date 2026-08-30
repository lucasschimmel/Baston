param(
    [ValidateSet("Valid", "Fake")]
    [string]$Case = "Valid",
    [string]$LicenseKey = "",
    [string]$SourceConfig = "baston.zone-a-onesync.local.toml",
    [string]$Log = "info",
    [switch]$Build
)

$ErrorActionPreference = "Stop"
$Root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $Root

$TargetDir = Join-Path $env:USERPROFILE ".cache\baston-target"
$ZoneExe = Join-Path $TargetDir "release\baston-zone.exe"

if ($Build -or -not (Test-Path $ZoneExe)) {
    cargo build --release -p baston-zone --features baston-zone/escrow --target-dir $TargetDir
}

if (-not (Test-Path $ZoneExe)) {
    throw "baston-zone.exe not found: $ZoneExe"
}

$FxServerPath = "D:/Dev/Fivem/Servers/WTF/Artifacts/windows/31623/FXServer.exe"
if (-not (Test-Path $FxServerPath)) {
    throw "FXServer.exe not found: $FxServerPath"
}

function Get-LicenseKeyFromConfig([string]$Path) {
    if (-not (Test-Path $Path)) {
        throw "Source config not found: $Path"
    }
    $match = Select-String -LiteralPath $Path -Pattern '^\s*sv_license_key\s*=\s*"([^"]+)"\s*$' | Select-Object -First 1
    if (-not $match) {
        throw "No sv_license_key found in $Path. Pass -LicenseKey instead."
    }
    return $match.Matches[0].Groups[1].Value
}

if ($Case -eq "Valid") {
    if ([string]::IsNullOrWhiteSpace($LicenseKey)) {
        $LicenseKey = Get-LicenseKeyFromConfig $SourceConfig
    }
    $ZoneId = "sidecar-valid"
    $GrpcPort = 50151
    $MetricsPort = 9191
    $SidecarPort = 30230
    $Bounds = "-8000,-8000,-7000,-7000"
} else {
    $LicenseKey = "cfxk_FAKE_SIDEcar_TEST_0000000000000000000000"
    $ZoneId = "sidecar-fake"
    $GrpcPort = 50152
    $MetricsPort = 9192
    $SidecarPort = 30231
    $Bounds = "-7000,-8000,-6000,-7000"
}

$TempConfig = Join-Path $env:TEMP "baston.$($ZoneId).local.toml"

@"
[server]
name = "BASTON Sidecar License Test $Case"
max_players = 64

[resources]
path = "examples/resources"

[nats]
url = "nats://127.0.0.1:4222"
zone_id = "$ZoneId"

[state_sync]
onesync = "on"
sync_interval_ms = 16
push_interval_ms = 50
aoi_radius = 450.0
max_speed_mps = 200.0
ownership_interval_secs = 5

[meshing]
enabled = true
gateway_grpc = "127.0.0.1:50050"
zone_grpc_addr = "0.0.0.0:$GrpcPort"
zone_public_grpc_addr = "127.0.0.1:$GrpcPort"
zone_bounds = "$Bounds"
heartbeat_interval_secs = 5
zone_timeout_secs = 15
boundary_margin = 300.0
boundary_scan_interval_ms = 500
handoff_cooldown_secs = 5

[metrics]
enabled = true
port = $MetricsPort

[dev]
hot_reload = false
auth_bypass = false

[license]
mode = "verified"
sv_license_key = "$LicenseKey"
fxserver_path = "$FxServerPath"
sidecar_port = $SidecarPort

[escrow]
enabled = false
"@ | Set-Content -LiteralPath $TempConfig -Encoding UTF8

try {
    Write-Host "Starting $Case sidecar licence test: zone=$ZoneId grpc=$GrpcPort metrics=$MetricsPort sidecar=$SidecarPort"
    Write-Host "Temp config: $TempConfig"
    $env:BASTON_CONFIG = $TempConfig
    $env:RUST_LOG = $Log
    & $ZoneExe
} finally {
    Remove-Item -LiteralPath $TempConfig -Force -ErrorAction SilentlyContinue
}
