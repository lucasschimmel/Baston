# Phase C — Guide de test

Tout ce qu'il faut valider pour la Phase C (state sync multi-joueurs), du plus
automatisé au plus manuel. Les niveaux 1-2 sont déjà passés en session dev ;
les niveaux 3-5 nécessitent de VRAIS clients FiveM et restent à valider.

---

## 0. Prérequis

```powershell
cd D:\Dev\Fivem\Servers\WTF\baston

# NATS (obligatoire pour le state sync) + monitoring
docker compose up -d nats prometheus grafana
docker compose ps nats        # → healthy

# Build release (le dev build fausse CPU/latence)
cargo build --release -p baston-gateway -p baston-loadtest
# Binaires dans C:\Users\osiri\.cache\baston-target\release\
```

Deux configs :
- `baston.toml` — auth CFX réelle, pour les tests avec vrai client. **Pas de
  TLS** : le port jeu reste en HTTP clair (un listener TLS-only casse les
  requêtes plaintext du client → `Received HTTP/0.9`). Le download packfile
  passe en HTTP via un fileServer littéral (validé Phase B, canary 31725).
- `baston-bench.toml` — `auth_bypass = true`, 128 slots, pour le loadtest.

Endpoints utiles :
- Métriques brutes : http://localhost:9090/metrics
- Prometheus : http://localhost:9091 — Grafana : http://localhost:3000 (anonyme admin)

---

## 1. Suite automatisée (2 min)

```powershell
cargo test
cargo clippy --all-targets
```

Ce que ça couvre (dont des E2E contre le NATS docker — ils s'auto-skippent si
NATS est down, vérifier qu'aucun `SKIPPED` n'apparaît) :

| Test | Jalon | Vérifie |
|---|---|---|
| `baston-zone --lib entity_manager` | C1 | dirty flags par champ, coalescing, flush idempotent, 10 threads concurrents, débit >1000 upd/s |
| `baston-zone --lib state_ingest` | C2/C5 | spawn player entity, anti-teleport, ownership refusé, montée véhicule → transfert |
| `state_sync_tests` | C2 | 100 entités publiées sur NATS < 2ms ; tick vide = 0 byte |
| `baston-gateway --lib state_aggregator` | C3/C5 | AoI in/out, Create→Delta→rien-si-inchangé, Delete une seule fois, rates 50/100/500ms, pas d'écho de ses propres entités |
| `state_aggregator_tests` | C3 | E2E zone→NATS→aggregator→snapshot par client |
| `udp_c4_tests` | C4 | relay msgRoute (netId réécrit, self-drop) ; pipeline complet 2 clients ENet (create/update/delete, pas d'écho) |
| `baston-zone --lib ownership` | C5 | owner déconnecté → réassigné au plus proche, pas de churn, players jamais réassignés |

---

## 2. Benchmark 100 joueurs (exit criterion officiel)

```powershell
# Terminal 1 — serveur en mode bench
$env:BASTON_CONFIG = "baston-bench.toml"
C:\Users\osiri\.cache\baston-target\release\baston-gateway.exe

# Terminal 2
C:\Users\osiri\.cache\baston-target\release\baston-loadtest.exe --clients 100 --duration 60s
```

Le binaire imprime PASS/FAIL contre les cibles. Référence du 2026-07-03 :

```
latency p50: 39ms (<50)   p99: 69ms (<100)
CPU gateway+zone: 9% d'un cœur (<40/30)
bandwidth: 0.60 Mbps (<10)
jitter StateSyncEmitter: 0.44ms (<2)
0 dropped connections, 0 entity desyncs
```

Points d'attention :
- Si le jitter explose (~15ms) → `timeBeginPeriod(1)` a échoué au boot
  (chercher le WARN dans les logs serveur).
- Pendant le run, ouvrir Grafana et grapher `world_state_entities`,
  `entities_dirty_per_tick`, `nats_publish_duration_ms`,
  `state_sync_tick_jitter_ms`, `snapshot_bytes_sent` — utile pour situer une
  dégradation (zone vs gateway vs NATS).
- Lancer 2-3 fois : le premier run inclut le warmup JetStream.

---

## 3. Un vrai client FiveM (sanity, ~10 min)

Reprend le flow Phase B et vérifie que la Phase C n'a rien cassé + que le
reporting d'état fonctionne.

1. `baston.toml` (auth réelle, HTTP clair sans `[tls]`), lancer le serveur, `connect localhost:30120`.
2. Checklist console serveur :
   - `player authenticated: license:...` → `UDP connection established` →
     `session host elected` → `[axiom-core] onCharacterSpawned` (= Phase B OK).
   - **Nouveau Phase C** : ~1s après le spawn, `player entity spawned
     (source=1)` — c'est le shim client qui reporte à 10Hz via
     `__baston:stateUpdate`.
3. Marcher en jeu, puis vérifier http://localhost:9090/metrics :
   - `state_updates_accepted` qui monte en continu (~10/s) ;
   - `world_state_entities` = 1 ;
   - `state_updates_rejected` reste à 0.
4. Se déconnecter → `player dropped` + `world_state_entities` retombe à 0
   (le marqueur DELETED a traversé le pipeline).

---

## 4. DEUX vrais clients — LE test C4 (jamais validé live)

C'est le point le plus important et le seul vraiment inconnu : la visibilité
mutuelle passe par le relay `msgRoute` + broadcast `onPlayerJoining`, testés
uniquement avec des clients ENet simulés. Il faut 2 PC (ou 2 installs) avec
2 comptes CFX distincts.

1. Connecter le client A, attendre son spawn complet (LSIA).
2. Connecter le client B.
3. **Côté A** : le ped de B doit apparaître et bouger en temps réel quand B
   marche. **Côté B** : idem pour A. Mouvements fluides (l'interp est faite
   par le moteur GTA, pas par BASTON).
4. Console serveur : vérifier `session host elected` UNE seule fois (pour le
   premier), et aucune boucle `HS_MISMATCH` côté client B (piège connu Phase B).
5. Nom au-dessus de la tête : doit afficher le vrai nom (broadcast
   `onPlayerJoining` netId/name).
6. Déconnecter A → côté B le ped de A doit **despawner** (broadcast
   `onPlayerDropped`). Reconnecter A → il réapparaît.
7. Métriques : `world_state_entities` = 2 quand les deux sont là ;
   `state_updates_accepted` monte à ~20/s.

**Si B ne voit pas A** — diagnostic dans l'ordre :
- `RUST_LOG=debug` et chercher `unhandled game message` : si le client envoie
  des msgTypes inconnus au moment où l'autre joueur devrait apparaître, c'est
  qu'un message de session manque (me donner les hashes, je les résoudrai
  contre le C++).
- Vérifier que des `msgRoute` transitent : ajouter temporairement un compteur
  ou grep debug — s'il n'y a AUCUN msgRoute entrant, le client n'a pas créé la
  session P2P (probablement lié au host / onPlayerJoining).
- Wireshark sur 30120/UDP en parallèle d'un vrai FXServer non-OneSync pour
  comparer la séquence.

---

## 5. Véhicule (exit C5 live)

1. Deux clients connectés et se voyant (test 4 passé).
2. Client A spawne un véhicule localement (client script / commande) et monte
   dedans. Rouler.
3. **Client B doit voir le véhicule rouler** avec A dedans (sync P2P GTA).
4. Côté serveur, si le véhicule est enregistré dans le pipeline (update avec
   `entity_id` + type Vehicle) : log `client entity registered` puis
   `vehicle ownership transferred to occupant` quand A monte.
5. A se déconnecte moteur tournant → dans les 5s, log
   `network owner reassigned` + event `onEntityOwnerChanged` visible si un
   handler de script l'écoute (B, le plus proche, devient owner).

---

## 6. Anti-cheat & robustesse (optionnel, rapide)

- **Teleport** : forcer un `SetEntityCoords` à +5km via le shim → le serveur
  doit logger `state update rejected: implausible displacement` et
  `state_updates_rejected{reason="teleport"}` s'incrémente. La position
  serveur ne bouge pas.
- **NATS down** : `docker compose stop nats` pendant que le serveur tourne →
  le serveur ne crash PAS (logs ERROR publish), et le loadtest montre juste
  une absence de snapshots. `docker compose start nats` → le subscriber
  se reconnecte (retry 1s).
- **Boot sans NATS** : lancer le serveur NATS coupé → il démarre avec
  `NATS unreachable — Phase C state sync DISABLED` (les clients peuvent
  toujours se connecter, sync P2P msgRoute inchangée).

---

## Récap des états

| Niveau | Quoi | État |
|---|---|---|
| 1 | `cargo test` + clippy | ✅ passé (session 2026-07-03) |
| 2 | Benchmark 100 clients | ✅ PASS (p50 39ms / p99 69ms / 0.6 Mbps) |
| 3 | 1 vrai client + reporting 10Hz | ⬜ à valider (Phase B validée, le reporting est nouveau) |
| 4 | 2 vrais clients se voient (msgRoute) | ⬜ **critique — jamais testé live** |
| 5 | Véhicule visible + ownership | ⬜ à valider |
| 6 | Anti-cheat / NATS down | ⬜ optionnel |
