---
title: "Getting started"
description: "Boot a server, declare resources, connect a database, and go multi-zone."
---

Guide de bout en bout : builder BASTON, lancer un serveur (mono-process ou mesh
multi-zones), déclarer tes resources/scripts/assets, brancher ta base de données,
puis tester ResMon, le profiler, Prometheus et Grafana.

> ⚠️ **Document d'origine, daté.** Il décrit encore le sidecar FXServer
> (`[escrow]`, `mode = "verified"`, `public_listing`), supprimé depuis —
> voir [ADR-003](../adr/003-remove-the-fxserver-sidecar.md). Pour la
> configuration actuelle, lire
> [`reference/configuration.md`](../reference/configuration.md).

> **BASTON n'est pas FXServer.** C'est une réécriture Rust *from scratch* du
> serveur FiveM (le client GTA V/FiveM ne voit pas la différence). Conséquence
> pratique : **pas de `server.cfg`, pas de `resources.cfg`, pas de `ensure`,
> pas de `fxmanifest.lua`**. Ces fichiers appartiennent au FXServer classique
> qui vit dans `../Base/` et `../Artifacts/` — un monde séparé, utilisé
> uniquement comme *ground truth* (tests escrow, comparaison de comportement).
> Ici on parle du projet Rust dans `baston/`.

---

## 0. Prérequis

| Outil | Pourquoi |
|---|---|
| Rust stable (edition 2021) | builder les binaires |
| Docker Desktop | NATS + Prometheus + Grafana (et le mesh multi-zones) |
| `jq`, `curl` | interroger l'API admin |
| PowerShell | environnement de dev de référence (Windows) |

Note build : le `target-dir` est forcé sur `C:/Users/osiri/.cache/baston-target`
(voir `.cargo/config.toml` — contournement d'un symlink v8 entre lecteurs C:/D:).
Les binaires release sortent donc dans
`C:\Users\osiri\.cache\baston-target\release\`.

```powershell
cd D:\Dev\Fivem\Servers\WTF\baston
cargo build --release -p baston-gateway            # + -p baston-zone pour le mesh
```

Le premier build est long (compilation de V8 via `deno_core`).

---

## 1. Architecture en une image

```
                 ┌─────────────────── mono-process (dev simple) ───────────────────┐
FiveM client ──▶ │ baston-gateway  :30120 (jeu TCP+UDP) · :8080 (API) · :9090 (/metrics) │
                 │  ├─ runtime JS (deno_core) : tes server_scripts                   │
                 │  ├─ ResourceManager : scanne resources/                           │
                 │  └─ state sync intra-process                                      │
                 └──────────────────────────────────────────────────────────────────┘

                 ┌─────────────────── mesh (Phase D, multi-zones) ─────────────────┐
FiveM client ──▶ │ baston-gateway (seul process face au jeu) :30120                 │
                 │      │ gRPC :50050 (registry/handoff)   │ NATS :4222 (state)     │
                 │  ┌───┴────────┬──────────────┐                                   │
                 │ zone-a       zone-b   … zone-N  (chacun : runtime JS + entités)  │
                 └──────────────────────────────────────────────────────────────────┘
```

- **Mono-process** : le gateway fait tout. C'est le mode par défaut
  (`[meshing] enabled = false`). C'est celui à utiliser pour développer.
- **Mesh** : le gateway route, les *zones* portent les runtimes de scripts et
  l'état des entités. Activé par `BASTON_MESHING_ENABLED=true`. Nécessite NATS.

---

## 2. Configuration : `config/baston.toml`

Le binaire charge le fichier pointé par `$env:BASTON_CONFIG`, sinon `config/baston.toml`
du répertoire courant (`baston-gateway.rs:71`). Les variables d'environnement
écrasent ensuite certaines valeurs.

Fichiers fournis :

| Fichier | Usage |
|---|---|
| `config/baston.toml` | dev réaliste (auth CFX réelle, 32 slots, licence `off`) |
| `config/baston-bench.toml` | benchmark (auth **bypassée**, 128 slots, hot-reload off) |
| `config/baston.docker.toml` | monté dans les conteneurs par `deploy/docker/docker-compose.yml` |

### Sections principales (défauts dans `baston-config/src/lib.rs`)

```toml
[server]
name = "BASTON Dev"
port = 30120
bind_address = "0.0.0.0"     # interface TCP + UDP du jeu
max_players = 32
enforce_game_build = "3258"   # le client bascule sur ce build avant connexion

[resources]
path = "resources"            # dossier scanné (override : BASTON_RESOURCES_PATH)

[auth]
pubkey_url = "https://lambda.fivem.net/api/ticket/pubkey"  # vérif ticket CFX offline

[udp]
poll_interval_ms = 5          # cadence de poll ENet

[dev]
hot_reload = true
auth_bypass = false           # true = pas de ticket CFX (dev/LAN sans launcher)

[state_sync]
sync_interval_ms = 16         # zone → NATS (62 fps)
push_interval_ms = 50         # gateway → clients (20 fps)
aoi_radius = 450.0            # rayon d'intérêt (m)
max_speed_mps = 200.0         # anti-cheat déplacement
onesync = "off"               # "on" = clone parsing serveur-autoritaire (OneSync-NG)

[metrics]
enabled = true
port = 9090                   # /metrics Prometheus

[voice]
enabled = false               # serveur vocal Mumble embarqué (baston-voice)
port = 30121                  # TCP(TLS) contrôle + UDP voix (même numéro)

[license]
mode = "off"                  # off | gate | verified   (cf docs/operations/licensing.md)
sv_license_key = ""
# public_listing = true        # broker FXServer officiel → liste CFX
# listing_ip_override = "203.0.113.10"

[escrow]
enabled = false               # assets chiffrés CFX (Windows + --features escrow)

[api]                         # API monitoring/contrôle — cf §7
audit_log = "baston-audit.jsonl"
```

### Overrides par variable d'environnement

| Var | Écrase |
|---|---|
| `BASTON_CONFIG` | chemin du fichier de config lui-même |
| `BASTON_PORT` | `server.port` |
| `BASTON_RESOURCES_PATH` | `resources.path` |
| `BASTON_METRICS_PORT` | `metrics.port` (utile pour lancer 2 zones sur une machine) |
| `BASTON_ADMIN_TOKEN` | `meshing.admin_token` (clé admin full-permission) |
| `BASTON_MESHING_ENABLED` | `meshing.enabled` (`true`/`1`/`yes`) |
| `ZONE_ID`, `ZONE_BOUNDS`, `GATEWAY_GRPC`, `NATS_URL`, `ZONE_GRPC_ADDR`, `ZONE_PUBLIC_GRPC_ADDR` | fédération (contrat Docker Compose) |

> ⚠️ **Pas de section `[tls]`.** Le client FiveM envoie des requêtes en HTTP
> clair sur le port jeu ; un listener TLS-only les casse
> (`Received HTTP/0.9 when not allowed`). Le download packfile reste en HTTP
> clair volontairement.

---

## 3. Lancer un serveur mono-process (le mode dev)

```powershell
cd D:\Dev\Fivem\Servers\WTF\baston
Remove-Item Env:BASTON_CONFIG -ErrorAction SilentlyContinue   # -> utilise baston.toml
$env:RUST_LOG = "info"
C:\Users\osiri\.cache\baston-target\release\baston-gateway.exe
```

Logs de boot attendus :

```
Prometheus exporter listening addr=0.0.0.0:9090
UDP game transport (ENet) listening port=30120
HTTP gateway listening
```

Puis en jeu (console F8) : `connect localhost:30120`.

Variante **sans launcher / sans ticket CFX** (LAN, tests) : lance avec
`config/baston-bench.toml` (`auth_bypass = true`) ou mets `[dev] auth_bypass = true`.

```powershell
$env:BASTON_CONFIG = "baston-bench.toml"
C:\Users\osiri\.cache\baston-target\release\baston-gateway.exe
```

---

## 4. Resources, scripts et « mappings »

### 4.1 Il n'y a pas de `resources.cfg`

En FXServer tu écris `ensure mon-resource` dans un `.cfg`. **BASTON n'a pas ça.**
Le `ResourceManager` scanne `resources/` et charge *automatiquement* tout
sous-dossier qui contient un `manifest.json` valide
(`resource_loader/manifest.rs`). Pour « désactiver » une resource : retire son
dossier ou son `manifest.json` (ou pilote-la à chaud via l'API `resources/{name}/stop`, §7).

### 4.2 Le manifeste : `manifest.json` (remplace `fxmanifest.lua`)

Champs supportés (`baston-protocol/src/lib.rs`, struct `ResourceManifest`) :

```json
{
  "name": "axiom-core",
  "version": "0.2.0",
  "dependencies": ["autre-resource"],
  "server_scripts": ["dist/server/index.js"],
  "client_scripts": ["dist/client/index.js"],
  "files": ["dist/client/index.js"]
}
```

- `server_scripts` : exécutés dans le runtime JS serveur (deno_core / V8).
- `client_scripts` + `files` : empaquetés dans un `resource.rpf` généré, envoyés
  au client. BASTON **génère un `fxmanifest.lua`** à la volée pour le packfile
  client à partir de ce JSON (`packfile.rs:generate_fxmanifest`) — tu n'écris
  jamais ce Lua toi-même.
- `dependencies` : ordre de chargement.

> Le format est volontairement minimal. Pas de `shared_scripts`, pas de
> `data_file`, pas de `exports`/`provide` déclaratifs dans le manifeste : ce que
> tu vois ci-dessus est l'intégralité du schéma aujourd'hui.

Exemple concret présent dans le repo : `examples/resources/axiom-core/` (le gamemode),
avec ses bundles `dist/server/index.js` et `dist/client/index.js`.

### 4.3 « Mappings » = assets streamés (`stream/`)

Comme FXServer : dépose tes `.yft/.ytd/.ydr/.ydd/...` (véhicules, vêtements,
props, éléments de map) dans un dossier `stream/` de la resource, à n'importe
quelle profondeur. **Aucune déclaration** dans le manifeste — le dossier est
auto-scanné (`docs/guides/streaming.md`).

```
resources/
  carpack/
    manifest.json          # { "name": "carpack" } suffit
    stream/
      vehicles/adder2.yft
      vehicles/adder2.ytd
```

Détails : hash SHA1 par fichier, annoncé dans `streamFiles` par *basename*
(uniques dans une resource), servi sur `/files/<resource>/<basename>`,
invalidation par mtime+size (hot reload en remplaçant le fichier). Fichiers
> 4 GiB ignorés. Assets **stream chiffrés escrow** hors périmètre (l'escrow ne
couvre que les scripts).

> Une vraie map complète type `ymap`/`ytyp` via `data_file` FXServer n'est pas
> couverte par le schéma `manifest.json` actuel : ce qui marche aujourd'hui,
> c'est le streaming d'assets par `stream/`.

---

## 5. Ajouter ta base de données

**BASTON n'a pas de couche DB intégrée.** Le dossier `../Supabase/` du repo WTF
est vide, et le cœur Rust ne parle à aucune base. L'accès aux données se fait
**dans le `server_script` de ta resource**, exactement comme un
`oxmysql`/`mysql-async` FiveM mais côté runtime JS BASTON.

Concrètement, pour brancher Supabase/Postgres :

1. Écris ta logique DB dans le bundle `server_scripts` de ta resource (fetch
   HTTP vers l'API REST Supabase, ou un client SQL selon ce que le runtime
   expose).
2. Passe l'URL et la clé **par une source hors-code** — jamais en dur dans le
   JS ni dans le manifeste. (Le runtime n'a pas encore de convar `[[api.keys]]`
   pour ça ; en attendant, injecte via l'environnement du process ou un fichier
   de conf lu par ton script, et garde la clé service-role hors du repo.)
3. Le schéma SQL vit dans `../Supabase/` (migrations) — c'est ta source de
   vérité DB, indépendante du serveur de jeu.

> À vérifier avant d'écrire le script : quelles capacités réseau le runtime
> `baston-scripting` expose réellement (fetch, timers). Ne suppose pas une API
> Node complète — regarde `crates/baston-scripting/src/extensions.rs` et
> `assets/bootstrap.js` pour la surface disponible.

---

## 5bis. Voix (serveur Mumble embarqué)

BASTON embarque son propre serveur vocal compatible Mumble (crate
`baston-voice`) — l'équivalent du umurmur intégré à FXServer. Le client FiveM
stock s'y connecte tel quel (TLS auto-signé, username `"[netId]"`).

```toml
[voice]
enabled = true    # défaut : false
port = 30121      # TCP (contrôle TLS) + UDP (voix) — même numéro, ≠ port jeu
```

Overrides : `BASTON_VOICE_ENABLED`, `BASTON_VOICE_PORT`.

Ce qui marche aujourd'hui :

- Handshake Mumble complet (Version → Authenticate → CryptSetup →
  ChannelState/UserState → ServerSync), crypto OCB2-AES128 sur UDP, fallback
  **UDPTunnel** (voix via TCP quand l'UDP est bloqué), ping chiffré et probe
  server-list.
- Routage par canal + voice targets (whispers/radios pma-voice), strip du bloc
  positionnel quand les contextes diffèrent.
- Natives serveur branchées : `MumbleCreateChannel`, `MumbleSetPlayerMuted`,
  `MumbleIsPlayerMuted`, `NetworkSet/Get/ClearVoiceProximityOverrideForPlayer`.
- La session vocale est détruite quand le joueur quitte le serveur de jeu.

Limite actuelle : pas encore de culling AoI serveur-autoritaire (le trait
`AoiOracle` est prêt, `NoCulling` par défaut — comportement pma-voice stock).

---

## 6. Lancer le mesh multi-zones (Docker Compose)

Le `deploy/docker/docker-compose.yml` est la **source de vérité** du setup multi-zones :
NATS + gateway + `zone-a` + `zone-b` + Prometheus + Grafana.

### Tout d'un coup

```powershell
cd D:\Dev\Fivem\Servers\WTF\baston
$env:BASTON_ADMIN_TOKEN = "un-token-solide-32+caracteres"
docker compose -f deploy/docker/docker-compose.yml up -d
docker compose ps
```

### Setup hybride recommandé en dev (infra en Docker, Rust en natif)

Plus rapide à itérer : NATS + monitoring en conteneurs, gateway et zones en
process natifs.

```powershell
docker compose -f deploy/docker/docker-compose.yml up -d nats prometheus grafana

# terminal 1 — gateway
$env:BASTON_MESHING_ENABLED = "true"
cargo run --release --bin baston-gateway

# terminal 2 — zone ouest
$env:ZONE_ID="zone-a"; $env:ZONE_BOUNDS="-4000,-4000,0,4000"; $env:BASTON_METRICS_PORT="9092"
cargo run --release --bin baston-zone

# terminal 3 — zone est
$env:ZONE_ID="zone-b"; $env:ZONE_BOUNDS="0,-4000,4000,4000"; $env:BASTON_METRICS_PORT="9093"
cargo run --release --bin baston-zone
```

### Ajouter une zone

1. Choisis des bounds qui pavent la map sans recouvrement (`docs/guides/zone-config.md`,
   bounds min-inclusif / max-exclusif, root −4000..+4000).
2. Copie le service `zone-b` dans `deploy/docker/docker-compose.yml`, change `ZONE_ID`,
   `ZONE_BOUNDS`, `ZONE_PUBLIC_GRPC_ADDR`.
3. `docker compose -f deploy/docker/docker-compose.yml up -d <service>` — la zone s'enregistre seule (retry 2s×30) ;
   vérifie `GET /admin/zones` ou `GET /api/v1/zones`.

Layout conseillé en prod : 4 zones, le sud (Los Santos) découpé plus fin — table
dans `docs/guides/zone-config.md`. Drain / crash / recovery : `docs/operations/running.md`.

---

## 7. ResMon & Profiler

Objectif : faire mieux que le `resmon`/`profiler` FXServer — voir la conso CPU
par resource, les handlers/events qui spikent, les natives roundtrip qui
bloquent, les fuites mémoire V8.

### 7.1 L'API admin `/api/v1` (port 8080)

Accès par **clé bearer**. Chaque clé est déclarée dans `[[api.keys]]` avec des
permissions explicites ; la clé legacy `BASTON_ADMIN_TOKEN` reste full-permission
(nom `admin`). Sans aucune clé **ni** admin token, le listener ne démarre pas
(fail-closed).

```toml
[api]
audit_log = "baston-audit.jsonl"

[[api.keys]]
name = "panel"
token = "e7a91b0f3d5c8e2a4f6b1d..."     # openssl rand -hex 32, ≥ 32 caractères
permissions = [
  "monitor.read", "resource.control", "player.kick", "zone.drain",
  "profiler.control", "profiler.read", "console.execute",
]
```

| Permission | Donne accès à |
|---|---|
| `monitor.read` | status, players, zones, resources, **resmon**, profiler status |
| `resource.control` | start/stop/restart resource |
| `player.kick` | kick joueur |
| `zone.drain` | drain zone |
| `profiler.control` | profiler record/stop |
| `profiler.read` | télécharger les captures |
| `console.execute` | commandes console bornées |

### 7.2 Lire le ResMon

```bash
TOKEN=... # ta clé
curl -s -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/resmon | jq
curl -s -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/resmon/resources/axiom-core | jq
curl -s -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/resmon/events | jq
```

`/api/v1/resmon` renvoie `cpu_ms_1m`, `dispatch_count`, `p95_ms`/`p99_ms`,
`watchdogs`, `native_p95_ms`, `native_timeouts`, `memory_mb` par resource.
`scope` vaut `"gateway"` en mono-process, `"mesh"` en meshing (il agrège alors
gateway + zones ; les erreurs RPC par zone sont dans le tableau `zones`).

### 7.3 Capturer un profil (Chrome Trace / Perfetto)

```bash
# démarrer une capture bornée
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H "content-type: application/json" \
  -d '{"frames":500,"seconds":null,"scope":"server","include_native_calls":true}' \
  http://localhost:8080/api/v1/profiler/record

curl -s -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/profiler/status | jq
curl -s -X POST -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/profiler/stop

# récupérer la trace -> importable dans chrome://tracing ou ui.perfetto.dev
curl -s -H "Authorization: Bearer $TOKEN" \
  http://localhost:8080/api/v1/profiler/latest/trace > baston-trace.json
```

Les captures sont **en mémoire, bornées, off par défaut**, et n'incluent jamais
les tokens admin, identifiers, IP ni payloads sensibles. `record`/`stop` sont
audités.

### 7.4 Via les commandes console (esprit FXServer)

```bash
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H "content-type: application/json" \
  -d '{"command":"resmon 1"}' http://localhost:8080/api/v1/commands/execute | jq
```

Built-ins : `resmon [1|0|on|off]`, `profiler record [frames]`, `profiler stop`,
`profiler status`, `profiler view`. Les commandes inconnues sont relayées aux
resources qui ont fait `RegisterCommand`. Détails complets : `docs/reference/api.md`.

---

## 8. Prometheus & Grafana

### 8.1 Les trois ports — ne pas les confondre

| Port | Quoi |
|---|---|
| `9090` | `/metrics` d'**un process BASTON** (le gateway **et chaque zone** exposent le leur). C'est la source des chiffres. |
| `9091` | **UI Prometheus** (hôte). Le conteneur écoute sur 9090 en interne → mapping `9091:9090`. `http://localhost:9091` (`/targets`, `/rules`, `/alerts`). |
| `3001` | **Grafana** (`3001:3000`). Admin anonyme activé pour le dev. |

Prometheus scrape gateway + zones toutes les 5s (`monitoring/prometheus.yml`).

### 8.2 Démarrer la stack

```powershell
docker compose -f deploy/docker/docker-compose.yml up -d prometheus grafana        # + nats si mesh
```

- Prometheus : http://localhost:9091 — vérifie `/targets` (les jobs
  `baston-gateway` / `baston-zones` doivent être `UP`).
- Grafana : http://localhost:3001 — dossier **BASTON**, datasource Prometheus
  auto-provisionnée (`monitoring/grafana-datasource.yml` +
  `grafana-dashboards.yml`).

> Un gateway lancé **en natif** (hors Docker) est scrapé via la cible
> `host.docker.internal:9090` déjà présente dans `prometheus.yml`.

### 8.3 Dashboards fournis (auto-provisionnés)

| Dashboard (uid) | À quoi il sert |
|---|---|
| **BASTON — Server Overview** (`baston-overview`) | santé globale d'un coup d'œil : joueurs online, resources chargées/erreurs, escrow decrypt p95/p99, audit API (rate + refus), UDP dropped, script dispatch p95/p99 par resource, native roundtrip p95/p99, profiler actif. **Board « le serveur est-il sain ? »** |
| **BASTON — Zone Mesh** (`baston-mesh`) | plan de données cross-zone : handoffs/s, routing-lock, latence handoff p99, prepare failures/timeouts, zone failures, state-sync jitter, reroutes, entités AoI/client, NATS publish latency. **Board de diagnostic du meshing.** |

### 8.4 Alertes (`monitoring/alerts.yml`)

Chargées par Prometheus (`rule_files`), visibles sur `http://localhost:9091/rules`
et `/alerts`. **Pas d'Alertmanager** dans le compose dev → les alertes ne
« paginent » nulle part, tu les regardes dans l'UI Prometheus. Règles (toutes
`warning`) : `ZoneDown`, `ZoneEvicted`, `ZoneHeartbeatFailing`,
`ApiAuthFailureSpike`, `HandoffPrepareFailures`, `ResourceLoadErrors`,
`ScriptWatchdogTerminations`, `NativeRoundtripTimeouts`, `ScriptDispatchP99High`.

Chaque alerte a une `description` qui dit quoi vérifier. Cheminement type :
`ScriptWatchdogTerminations`/`ScriptDispatchP99High` →
`GET /api/v1/resmon/resources/{name}` pour trouver le handler lent → `profiler
record` → `/profiler/latest/trace`. Détail complet : `docs/operations/running.md`.

---

## 9. Tester la charge (loadtest / benchmark)

Le binaire `baston-loadtest` simule des clients (nécessite `auth_bypass = true`,
donc `config/baston-bench.toml`).

### Mono-zone (exit criterion Phase C)

```powershell
$env:BASTON_CONFIG = "baston-bench.toml"
C:\Users\osiri\.cache\baston-target\release\baston-gateway.exe   # terminal 1
C:\Users\osiri\.cache\baston-target\release\baston-loadtest.exe --clients 100 --duration 60s
```

Attendu : 100 connectés (0 dropped), p50 ~40ms / p99 ~70ms, CPU < 20%,
`exit criterion: PASS`.

### Mesh (exit criterion Phase D)

```powershell
docker compose -f deploy/docker/docker-compose.yml up -d nats prometheus grafana
# gateway + 2 zones en natif (cf §6)
cargo run --release --bin baston-loadtest -- --zones 2 --clients-per-zone 1000 \
  --handoffs true --duration 300s \
  --zone-metrics http://127.0.0.1:9092/metrics,http://127.0.0.1:9093/metrics
```

Cibles : handoff success > 99.9%, handoff p99 < 100ms, freeze client 0ms,
CPU gateway < 50%, NATS < 100 MB/s, 0 zone failures.

Runbook pas-à-pas (y compris tests avec 2 clients réels, véhicules, anti-cheat) :
`docs/operations/runbooks/phase-c.md`.

---

## 10. Cheat-sheet

```powershell
# Build
cargo build --release -p baston-gateway -p baston-zone -p baston-loadtest

# Dev mono-process (auth CFX réelle)
Remove-Item Env:BASTON_CONFIG -EA SilentlyContinue
C:\Users\osiri\.cache\baston-target\release\baston-gateway.exe

# Dev sans launcher (LAN)
$env:BASTON_CONFIG="baston-bench.toml"; ...\baston-gateway.exe

# Mesh complet
$env:BASTON_ADMIN_TOKEN="token32+"; docker compose -f deploy/docker/docker-compose.yml up -d

# Monitoring seul
docker compose -f deploy/docker/docker-compose.yml up -d nats prometheus grafana
#   Prometheus  http://localhost:9091      Grafana http://localhost:3001

# API admin (TOKEN = BASTON_ADMIN_TOKEN ou une [[api.keys]])
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/status  | jq
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/resmon  | jq
curl -X POST -H "Authorization: Bearer $TOKEN" -H "content-type: application/json" \
     -d '{"command":"resmon 1"}' http://localhost:8080/api/v1/commands/execute

# Qualité (avant commit)
cargo fmt --check ; cargo clippy --workspace -- -D warnings ; cargo test --workspace
```

## Références internes

- `docs/guides/modules.md` — modules activables, bundles (js / lua / lite / full), addons
- `docs/operations/running.md` — topologie, zones, monitoring, alerting détaillé
- `docs/reference/api.md` — API `/api/v1` complète (routes, permissions, audit)
- `docs/guides/streaming.md` — assets `stream/`
- `docs/guides/zone-config.md` — découpage des zones
- `docs/operations/runbooks/phase-c.md` — runbook de test manuel (clients réels)
- `docs/operations/licensing.md`, `docs/operations/escrow.md` — licence CFX & assets chiffrés
