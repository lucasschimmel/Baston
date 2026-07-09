# BASTON local live test runbook

This runbook uses the local TOML configs in the repository root:

- `baston.mono.local.toml`
- `baston.gateway-mesh.local.toml`
- `baston.gateway-onesync.local.toml`
- `baston.zone-a.local.toml`
- `baston.zone-b.local.toml`

They are ignored by Git via `*.local.toml`. Fill only the missing licence values
when needed.

## 1. Build and infrastructure

```powershell
cd D:\Dev\Fivem\Utils\Baston
docker compose up -d nats prometheus grafana
cargo build --release -p baston-gateway -p baston-zone -p baston-loadtest
```

Check local quality:

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## 2. Single-process real client test

Terminal 1:

```powershell
.\tools\local\start-mono.ps1
```

FiveM F8:

```text
connect localhost:30120
```

Expected:

- server logs `player authenticated: license:...`
- `UDP connection established`
- `session host elected`
- `[axiom-core] onCharacterSpawned`

Metrics:

```powershell
(Invoke-WebRequest http://localhost:9090/metrics).Content |
  Select-String "state_updates_accepted|world_state_entities|state_updates_rejected"
```

Expected: `world_state_entities 1`, accepted updates increasing, rejected at 0.

## 3. Two-zone mesh test

Terminal infra:

```powershell
docker compose up -d nats prometheus grafana
```

Terminal gateway:

```powershell
.\tools\local\start-gateway-mesh.ps1
```

Terminal zone A:

```powershell
.\tools\local\start-zone-a.ps1
```

Terminal zone B:

```powershell
.\tools\local\start-zone-b.ps1
```

Check zone registration:

```powershell
$Token = "<admin-token-from-your-local-toml>"
Invoke-RestMethod -Headers @{ Authorization = "Bearer $Token" } `
  http://localhost:8080/api/v1/zones
```

Expected: `zone-a` and `zone-b` registered and heartbeating.

FiveM F8:

```text
connect localhost:30120
```

Handoff test: move across x = 0. Expected logs include handoff preparation and
commit, with no kick and no long freeze.

## 4. Two real clients

Start the gateway with debug UDP logs:

```powershell
.\tools\local\start-gateway-mesh.ps1 -Log "info,udp=debug"
```

Start both zones as above, then connect two PCs or two FiveM installs:

```text
connect <server-ip>:30120
```

Checklist:

- `session host elected` appears once
- both players spawn
- A sees B, B sees A
- names appear above peds
- movement is fluid
- disconnecting A despawns A for B
- `world_state_entities` reaches 2

## 5. OneSync test

Terminal:

```powershell
.\tools\local\start-onesync.ps1
```

FiveM F8:

```text
connect localhost:30120
```

Checklist:

- no `HS_MISMATCH` loop
- spawn works
- with two clients, peds and vehicles replicate
- no critical `unhandled game message` logs

## 6. Keymaster licence test

Shape-only gate:

Edit `baston.mono.local.toml` or a zone config:

```toml
[license]
mode = "gate"
sv_license_key = "cfxk_FILL_ME"
```

Real verified mode is already configured in the local zone files with:

```toml
fxserver_path = "D:/Dev/Fivem/Servers/WTF/Artifacts/windows/31623/FXServer.exe"
```

Zone A uses `sidecar_port = 30130`; zone B uses `sidecar_port = 30131`.

To change it later:

Edit `baston.zone-a.local.toml` and `baston.zone-b.local.toml`:

```toml
[license]
mode = "verified"
sv_license_key = "cfxk_FILL_ME"
fxserver_path = "C:/FXServer/FXServer.exe"
sidecar_port = 30130
```

Use a different `sidecar_port` per zone, then build:

```powershell
cargo build --release -p baston-zone --features escrow
```

Run the mesh. Expected: the zone refuses to start if the official FXServer
sidecar cannot validate the licence.

## 7. Useful endpoints

```powershell
$Token = "<admin-token-from-your-local-toml>"
Invoke-RestMethod -Headers @{ Authorization = "Bearer $Token" } http://localhost:8080/api/v1/status
Invoke-RestMethod -Headers @{ Authorization = "Bearer $Token" } http://localhost:8080/api/v1/resmon
```

Prometheus: http://localhost:9091

Grafana: http://localhost:3001
