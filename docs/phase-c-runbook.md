# Phase C — Runbook pas-à-pas (commandes exactes)

Deux pipelines indépendants. Tout est en PowerShell, depuis
`D:\Dev\Fivem\Servers\WTF\baston`. Chaque étape = commande → résultat attendu
→ quoi faire si ça échoue. Binaires release : `C:\Users\osiri\.cache\baston-target\release\`.

---

# PIPELINE A — sans te connecter en jeu (~15 min)

## A1. Infra

```powershell
cd D:\Dev\Fivem\Servers\WTF\baston
docker compose up -d nats prometheus grafana
docker compose ps
```

✅ Attendu : `baston-nats-1 … (healthy)`, prometheus et grafana `Up`.
❌ `error during connect … dockerDesktopLinuxEngine` → Docker Desktop pas lancé :
`Start-Process "C:\Program Files\Docker\Docker\Docker Desktop.exe"`, attendre 30s, relancer.

## A2. Build

```powershell
cargo build --release -p baston-gateway -p baston-loadtest
```

✅ Attendu : `Finished 'release' profile` (long au premier build — v8).

## A3. Suite de tests automatisée

```powershell
cargo test 2>&1 | Select-String "test result|SKIPPED|FAILED"
```

✅ Attendu : que des `test result: ok`, **aucun** `FAILED`, **aucun** `SKIPPED: NATS`
(un SKIPPED = NATS down = retour A1).

```powershell
cargo clippy --all-targets 2>&1 | Select-String "^warning|^error"
```

✅ Attendu : aucune sortie.

## A4. Lancer le serveur en mode bench (terminal 1, le laisser ouvert)

```powershell
$env:BASTON_CONFIG = "baston-bench.toml"
$env:RUST_LOG = "info"
C:\Users\osiri\.cache\baston-target\release\baston-gateway.exe
```

✅ Attendu dans les logs de boot, dans cet ordre :
```
Prometheus exporter listening addr=0.0.0.0:9090
dev.auth_bypass is enabled — CFX tickets are NOT validated   ← normal en bench
StateSyncEmitter started zone_id=zone-a interval_ms=16
UDP game transport (ENet) listening port=30120
HTTP gateway listening
StateAggregator: subscribed to baston.zone.*.state
```
❌ `NATS unreachable — Phase C state sync DISABLED` → retour A1.
❌ des WARN `tick jitter above 5ms` en boucle → `timeBeginPeriod(1)` a échoué ; me le signaler.

## A5. Vérifier l'endpoint métriques (terminal 2)

```powershell
(Invoke-WebRequest http://localhost:9090/metrics).Content | Select-String "state_sync_tick_jitter_ms_count"
```

✅ Attendu : une ligne avec un compteur qui existe (> 0 après quelques secondes).

## A6. Benchmark — l'exit criterion Phase C

```powershell
C:\Users\osiri\.cache\baston-target\release\baston-loadtest.exe --clients 100 --duration 60s
```

✅ Attendu (≈ 70s d'exécution) :
```
initConnect: 100 tokens issued
=== baston-loadtest report ===
clients connected : 100 (dropped: 0)
latency p50       : ~40ms   latency p99 : ~70ms
CPU gateway+zone  : <20%
bandwidth (client-observed) : <1 Mbps
StateSyncEmitter jitter avg : <1ms
dropped connections: 0, entity desyncs: 0
exit criterion    : PASS
```
❌ `initConnect refused … auth_bypass` → tu as lancé le serveur avec `baston.toml`
au lieu de `baston-bench.toml` (vérifier `$env:BASTON_CONFIG` dans le terminal 1).
❌ `FAIL` sur une métrique → relancer une fois (warmup) ; si stable, me donner le rapport.

## A7. (optionnel) Grafana pendant le bench

Ouvrir http://localhost:3000 → Explore → datasource Prometheus → grapher
`world_state_entities` (doit monter à 100 pendant le run puis retomber à 0),
`state_sync_tick_jitter_ms` (bucket), `snapshot_bytes_sent`.

## A8. (optionnel) Robustesse NATS

```powershell
docker compose stop nats     # pendant que le serveur tourne
```
✅ Attendu : le serveur NE crash PAS, logs `ERROR … publish failed` en continu.
```powershell
docker compose start nats
```
✅ Attendu : `StateAggregator: subscribed …` réapparaît (retry 1s), plus d'ERROR.

## A9. Arrêt propre

```powershell
# Terminal 1 : Ctrl+C
docker compose stop prometheus grafana   # garder nats si tu enchaînes le pipeline B
```

---

# PIPELINE B — avec le jeu (1 puis 2 clients réels)

Prérequis : NATS up (A1), build fait (A2). **PAS de TLS** — le port jeu doit
rester en HTTP clair (le client FiveM envoie des requêtes en plaintext ; un
listener TLS-only les casse avec `Received HTTP/0.9 when not allowed`). Ne pas
mettre de section `[tls]` dans `baston.toml`. Le download packfile passe en
HTTP clair via un fileServer littéral (validé Phase B sur canary 31725).

## B1. Lancer le serveur en mode réel (terminal 1)

```powershell
cd D:\Dev\Fivem\Servers\WTF\baston
Remove-Item Env:BASTON_CONFIG -ErrorAction SilentlyContinue   # ← important après le pipeline A
$env:RUST_LOG = "info"
C:\Users\osiri\.cache\baston-target\release\baston-gateway.exe
```

✅ Attendu : mêmes lignes qu'en A4 **sans** le warn auth_bypass, et
`HTTP gateway listening` (HTTP clair, c'est le mode correct — pas de TLS).
❌ Si tu vois `HTTPS gateway listening` : tu as une section `[tls]` dans
`baston.toml`, à retirer (elle casse la connexion avec `Received HTTP/0.9`).

## B2. Client 1 — connexion + spawn (Phase B toujours OK ?)

En jeu (F8) : `connect localhost:30120`

✅ Attendu console serveur, dans l'ordre :
```
player authenticated: license:…
UDP connection established: source=1 … latency=XXms
clock sync: offset=…
session host elected source=1
[axiom-core] onCharacterSpawned
```
❌ Bloqué au download → problème TLS/cert ; F8 côté client donne l'erreur exacte.

## B3. Client 1 — le reporting Phase C (NOUVEAU)

Ne rien faire d'autre que marcher en jeu ~10s, puis terminal 2 :

```powershell
(Invoke-WebRequest http://localhost:9090/metrics).Content | Select-String "state_updates_accepted|world_state_entities|state_updates_rejected"
```

✅ Attendu :
- console serveur (déjà passée) : `player entity spawned source=1 entity_id=…`
- `state_updates_accepted` ≈ 10/s écoulées (relancer la commande : il monte) ;
- `world_state_entities 1` ;
- `state_updates_rejected` absent ou à 0.

❌ Pas de `player entity spawned` → le shim n'émet pas ; vérifier en F8 client
qu'il n'y a pas d'erreur JS `__baston:stateUpdate`, et me remonter le log.

## B4. Client 1 — déconnexion propre

Quitter le serveur côté client.

✅ Attendu : `player dropped`, puis :
```powershell
(Invoke-WebRequest http://localhost:9090/metrics).Content | Select-String "world_state_entities"
```
→ `world_state_entities 0` (le DELETED a traversé tout le pipeline).

## B5. DEUX clients — visibilité mutuelle (LE test critique, jamais validé)

Il faut 2 PC / 2 comptes CFX. Passer le serveur en debug d'abord (terminal 1,
Ctrl+C puis) :

```powershell
$env:RUST_LOG = "info,udp=debug"
C:\Users\osiri\.cache\baston-target\release\baston-gateway.exe
```

1. PC A : `connect <ip-du-serveur>:30120` → attendre le spawn complet (B2).
2. PC B : `connect <ip-du-serveur>:30120`.

✅ Checklist, dans l'ordre :
- [ ] console : `session host elected` apparaît UNE seule fois (pour A) ;
- [ ] B charge et spawne sans boucle infinie ni crash ;
- [ ] **A voit le ped de B apparaître ; B voit le ped de A** ;
- [ ] quand B marche/court, A le voit bouger fluide (et inversement) ;
- [ ] le nom du joueur s'affiche au-dessus de chaque ped ;
- [ ] `world_state_entities` = 2 ; `state_updates_accepted` monte ~2× plus vite.

3. Déconnecter A :
- [ ] côté B, le ped de A **despawn** en quelques secondes ;
- [ ] console : `player dropped` + `world_state_entities` retombe à 1.

4. Reconnecter A :
- [ ] B revoit A réapparaître.

❌ **Si B ne voit pas A** — collecter dans cet ordre et me donner le résultat :
```powershell
# 1. Y a-t-il des messages jeu inconnus au moment où l'autre devrait apparaître ?
#    (les hashes 0x… des 'unhandled game message' sont la clé)
# → lire le terminal 1 (udp=debug)

# 2. Des msgRoute transitent-ils ? (chercher 'route relay failed' = mauvais signe,
#    aucune mention de route = le client n'a jamais initié la session P2P)
```
Piège connu : si B boucle `HS_HOSTING → HS_MISMATCH` (F8 client), c'est la
négociation de host — me le dire, c'est le même terrain que le fix Phase B #4.

## B6. Véhicule (exit C5)

Toujours 2 clients connectés et se voyant :

1. Client A spawne un véhicule (menu/trainer local ou commande) et **monte dedans**.
2. Rouler 30 secondes, faire des virages.

✅ Checklist :
- [ ] B voit le véhicule rouler avec A au volant, trajectoire fluide ;
- [ ] A descend, B monte → pas de crash, le véhicule répond à B ;
- [ ] si le véhicule est enregistré dans le pipeline serveur : logs
      `client entity registered` puis `vehicle ownership transferred to occupant` ;
- [ ] A se déconnecte pendant que B est à côté du véhicule → dans les ~5s :
      `network owner reassigned … new=Some(2)`.

## B7. (optionnel) Anti-cheat en conditions réelles

Client A, en F8 ou via un script local, forcer un teleport de +5 km
(`SetEntityCoords`). Dans les ~100ms :

✅ Attendu console serveur :
```
state update rejected: implausible displacement (teleport?) speed_mps=…
```
et `state_updates_rejected{reason="teleport"}` > 0 sur `:9090/metrics`.
Note : le jeu LOCAL de A montre le teleport (autorité client sur son propre
rendu) — c'est l'état serveur/les autres clients qui ne suivent pas.

## B8. Fin de session

```powershell
# Ctrl+C serveur, puis :
docker compose stop nats prometheus grafana
```

---

## Quoi me remonter après tes tests

1. Le rapport complet du loadtest (A6) si ≠ PASS.
2. Pour B5 en échec : les lignes `unhandled game message` (hashes 0x…) et tout
   comportement F8 côté client (HS_MISMATCH, crash, timeout).
3. Pour B6 : si B ne voit pas le véhicule bouger alors que B5 passe — c'est
   une info précieuse (sync véhicule P2P ≠ sync ped).
