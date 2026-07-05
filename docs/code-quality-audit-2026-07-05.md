# BASTON — Audit qualité de code

> Date : 2026-07-05 · Périmètre : workspace Rust `baston/` (8 crates, ~17 500 lignes de source, ~20 100 avec tests)
> Méthode : outillage objectif (clippy, fmt, greps d'anti-patterns) + revue qualitative approfondie crate par crate.
> Ce rapport photographie l'état à HEAD `3948d82` (working tree modifié au moment de l'audit).

---

## 1. Verdict

**Codebase de qualité nettement supérieure à la moyenne pour son stade.** La discipline d'ingénierie est réelle et visible : gestion d'erreur rigoureuse sur les chemins de parsing non fiables, `unsafe` minimal et *sound*, documentation dense (ratio ~2:1), 231 tests dont des tests de robustesse (entrées tronquées/malformées), défauts de configuration sûrs et **aucun secret en dur**.

La prémisse de départ (« ~133 `unwrap()` = autant de bombes DoS ») **ne s'est pas confirmée** : après vérification ligne par ligne, la quasi-totalité des `unwrap` sont dans des blocs `#[cfg(test)]` ou sur des invariants réellement internes. Les parseurs de données client propagent systématiquement via `Option`/`Result`.

Les défauts réels sont **concentrés et bien identifiés** :

- **Sécurité de surface publique** : deux vulnérabilités exploitables depuis Internet (route admin non authentifiée + amplification UDP).
- **Robustesse d'exécution** : absence de watchdog sur le runtime de scripting, et un bug de comptabilité d'object-id dans OneSync-NG (encore non branché).
- **Process** : une CI définie mais qui échouerait en l'état et n'a pas de remote pour tourner.

**Note globale : B+ / « très bon socle, dette de sécurité de surface publique à solder avant tout listing public ».**

### Répartition des findings

| Sévérité | Nombre | Exploitable aujourd'hui ? |
|----------|:------:|---------------------------|
| 🔴 Critique | 0 | — |
| 🟠 Élevé | 5 | 2 depuis Internet, 1 via client hostile, 2 latents (code non branché / API future) |
| 🟡 Moyen | 13 | mix robustesse / concurrence / cohérence de typage |
| ⚪ Faible | 25 | idiomes, cohésion, dette de test |
| **Total** | **43** | + findings de process (§6) |

---

## 1bis. Remédiation appliquée (session 2026-07-05)

Les cinq paliers du §7 ont été traités. **État du build après remédiation : `clippy -- -D warnings` ✅, `fmt --check` ✅, `cargo test --workspace` = 245 tests ✅** (la CI, auparavant rouge, passe au vert).

### Corrigé et testé

| Palier | Findings traités |
|--------|------------------|
| **0 — Sécurité** | SEC-1 (route drop authentifiée par bearer, fail-closed) · SEC-2 (token-bucket par IP + challenge borné 64 o sur `getinfo`) · SEC-3 (comparaison token constante-temps via `subtle`) · SEC-4 (CORS restreint GET/POST, plus de headers arbitraires) · SEC-5 (doc de confiance proxy sur `x-real-ip`) |
| **1 — Quick wins** | TYPE-1 (`validate()` branché dans `load()`) · CONC-3 (mutex/RwLock empoisonnables → `into_inner`, 3 sites) · STRUCT-5 (`total_cmp`) · ROB-7 (`wrapping_add`) · STRUCT-2 (6 warnings clippy) · PROC-1 (`fmt --all` + CI verte) |
| **2 — Robustesse** | **ROB-1 (watchdog V8 `terminate_execution` + `cancel_terminate_execution`, avec test de non-régression)** · CONC-1 (`canonicalize` hors lock async) · CONC-2 (canaux bornés : net_bridge, mesh_forward, cmd UDP, avec drop+métrique) · STRUCT-4 (noms de script internés — fin de la fuite au hot-reload) |
| **3 — Avant OneSync-NG** | **ROB-3 (fuite d'object-id sur create rejeté — `apply_create` retourne un bool, + test)** · ROB-5 (progression garantie du budget d'interest) · ROB-6 (gardes `length` du bitbuffer) · TYPE-4 (asymétrie `length_hack` documentée) · F1 (cap priorité f32) · sweep déterministe "ne panique jamais" (4000 itérations sur les 3 portes non fiables) |
| **4 — Hygiène** | TYPE-2 (`mode`/`backend` en `enum`, 2 variantes d'erreur supprimées) · PROC-2 (`.cargo/config.toml` désindexé + gitignoré + `.example`) |

### Différé volontairement (avec justification)

| Finding | Raison |
|---------|--------|
| **ROB-2** (sérialisation de l'appel natif) | Le fix complet = rendre les dispatches concurrents sur un `JsRuntime` `!Send` → **refonte du cœur d'exécution du scripting, live-testé**. Trop risqué à l'aveugle. Mitigation appliquée : sérialisation documentée + bornée par `NATIVE_CALL_TIMEOUT`. À traiter comme un chantier de design dédié. |
| **ROB-4** (unification `IdAllocator`) | Le bug concret (ROB-3) est corrigé ; la divergence de comptabilité qu'il visait n'est **pas atteignable** une fois ROB-3 réglé (les chemins `remove_client`/`apply_remove` nettoient les deux structures). Réécriture structurelle = optionnelle. |
| **STRUCT-1** (découpage fichiers > 400 l) | Pur churn cosmétique à fort risque sur du code fonctionnel ; cohésion jugée bonne par la revue (ex. `udp/mod.rs` = machine à états mono-propriétaire, handlers < 50 l). Non fait. |
| **cargo-fuzz** (harnais complet) | Remplacé par un sweep déterministe sur stable (CI stable, pas de toolchain nightly). Harnais `cargo-fuzz` = suivi optionnel. |

---

## 2. Métriques objectives

| Métrique | Valeur | Lecture |
|----------|:------:|---------|
| Lignes de source Rust | ~17 445 | 8 crates |
| Lignes avec tests/benches | ~20 100 | — |
| Tests (`#[test]`/`#[tokio::test]`) | 231 | bonne densité |
| `clippy --workspace` (défaut) | **6 warnings** | quasi-propre |
| `clippy -- -D warnings` | **échec (exit 101)** | ⚠️ CI serait rouge |
| `cargo fmt --check` | **échec (exit 1)** | ⚠️ drift de formatage |
| `unsafe` (blocs) | 6 | tous FFI LZ4, *sound*, commentés `// SAFETY:` |
| `unwrap()` / `expect()` | 133 / 40 | ~majorité en tests ; **0 sur chemin client vérifié** |
| `panic!`/`todo!`/`unreachable!` | 4 | négligeable |
| `TODO`/`FIXME`/`HACK` | 0 | remarquable |
| Doc comments (`///` + `//!`) | 1152 + 61 | vs 544 items `pub` → ratio ~2:1 |
| Fichiers > 400 lignes | 11 | voir §5.D |
| Remotes git | **0** | repo purement local → CI jamais exécutée |

---

## 3. Tableau de bord par crate

| Crate | LOC src | Note | 🟠 | 🟡 | ⚪ | Commentaire |
|-------|:-------:|:----:|:--:|:--:|:--:|-------------|
| `baston-protocol` | 4 409 | **A** | 0 | 1 | 5 | Parsing non fiable exemplaire. Risque unique latent. |
| `baston-core` | 348 | **A** | 0 | 0 | 2 | Fail-closed, exemplaire, très testé. |
| `baston-config` | 862 | **B+** | 0 | 2 | 3 | Messages d'erreur modèles. `load()` ne valide pas. |
| `baston-escrow-plugin` | 766 | **A-** | 0 | 1 | 2 | Le plus défensif. Mutex poison à durcir. |
| `baston-zone` | 4 550 | **B** | 1 | 3 | 4 | Concurrence saine. Comptabilité d'object-id fragile. |
| `baston-gateway` | 4 382 | **B-** | 2 | 3 | 3 | Auth CFX solide, mais 2 trous de surface publique. |
| `baston-scripting` | 1 269 | **B-** | 2 | 3 | 5 | Isolation V8 juste, mais pas de watchdog. |
| `baston-loadtest` | 859 | **B** | 0 | 0 | 3 | Outil interne honnête, `partial_cmp` fragile. |

---

## 4. Findings prioritaires — Sécurité de surface publique

> Ce sont les seuls défauts directement atteignables par un attaquant non authentifié. **À traiter en premier**, avant toute mise en visibilité server-list.

### 🟠 SEC-1 — Route admin non authentifiée sur le port public
**`crates/baston-gateway/src/http/mod.rs:44-47`** + **`http/client.rs:204-220`**

La route `POST /admin/player/{source}/drop` est montée sur le routeur HTTP **public** (port jeu 30120) **sans aucune vérification de token**. `admin_drop_player` ne contrôle ni bearer ni header. Comme `allocate_source` est un simple compteur `+1`, les `source` sont séquentiels et prévisibles.

**Impact** : n'importe qui sur Internet peut déconnecter arbitrairement n'importe quel joueur (DoS ciblé trivial, énumérable). Contraste total avec `admin.rs`, qui protège correctement son API par bearer token sur un port séparé.

**Correction** : retirer la route du routeur public, ou lui imposer le même `check_auth` que `admin.rs`. Si c'est un vestige de Phase A, la supprimer.

### 🟠 SEC-2 — Amplification UDP sur `getinfo` (~16×)
**`crates/baston-gateway/src/udp/oob.rs:38-57`** (+ `:44`)

`handle_oob` répond à `getinfo` sans rate-limiting ni challenge, et **réfléchit le challenge contrôlé par l'attaquant** dans une réponse d'environ 180 octets pour une requête de ~11 octets → **facteur d'amplification ~16×**. Le challenge n'est borné que par la taille du datagramme (`split(...).next()` peut renvoyer des centaines d'octets), ce qui aggrave le ratio.

**Impact** : vecteur de réflexion/amplification classique. IP source spoofée → le serveur inonde une victime tierce.

**Correction** : token-bucket par IP source sur le chemin OOB (`governor` ou map `IP→Instant`) ; tronquer le challecho écho à ≤ 64 octets.

### 🟡 SEC-3 — Comparaison de token admin non constante-temps
**`crates/baston-gateway/src/admin.rs:62`** — `provided == Some(state.token.as_str())`. Fuite de timing théorique sur un secret d'opérateur. Corriger via `subtle::ConstantTimeEq`.

### 🟡 SEC-4 — `CorsLayer::permissive()` sur tout le routeur public
**`crates/baston-gateway/src/http/mod.rs:49`** — combiné à SEC-1, permet à un site tiers de déclencher le drop en cross-origin depuis le navigateur d'un admin. Restreindre aux origines CEF nécessaires.

### ⚪ SEC-5 — `peer_ip` fait confiance à `x-real-ip`
**`crates/baston-gateway/src/http/client.rs:41-47`** — l'identifier `ip:` (persisté, utilisé pour ban/allowlist) est spoofable sans reverse-proxy de confiance qui réécrit l'en-tête. Documenter/imposer le déploiement derrière proxy, ou préférer l'adresse socket.

**Bonne nouvelle** : l'auth de ticket CFX (`auth/ticket.rs`, `auth/mod.rs`) est **solide** — parsing borné, double signature RSA PKCS#1v1.5, expiry/GUID/anti-rejeu avec GC, bien testée. La note mémoire évoquant un `dev-admin-token` par défaut est **inexacte** : le défaut est vide (API désactivée, refus de démarrage si le token est vide sur le port admin) ; `dev-admin-token` n'apparaît que dans les fichiers Docker de dev.

---

## 5. Findings par thème

### A. Robustesse & DoS applicatif

#### 🟠 ROB-1 — Pas de watchdog sur le runtime de scripting V8
**`crates/baston-scripting/src/host.rs:412`** + **`runtime.rs:152-161`**

Le thread runtime traite les commandes **séquentiellement** (`while rx.recv().await` → `run_event_loop().await`). Un script synchrone (`while(true){}`) n'est pas interruptible : aucun `v8::IsolateHandle::terminate_execution` ni deadline n'est armé. Le resource gèle son runtime *pour toujours* ; le host reste bloqué sur le `oneshot` de réponse, le `mpsc(64)` sature, puis tous les `send().await` du host se bloquent en cascade.

**Correction** : récupérer un `IsolateHandle` (il est `Send`) à la création, armer un timer qui appelle `terminate_execution()` au-delà d'un budget CPU par dispatch. C'est le levier de robustesse n°1 avant d'exécuter des scripts non fiables (resources communautaires).

#### 🟠 ROB-2 — `op_invoke_native_on_client` sérialise tout le trafic du resource pendant 1 s
**`crates/baston-scripting/src/extensions.rs:109-178`** (timeout `NATIVE_CALL_TIMEOUT = 1000 ms`, `:51`)

L'op async attend le résultat client dans le chemin de dispatch synchrone. Tant qu'un handler attend, la boucle de commandes du thread ne reprend pas → aucun autre événement du resource n'est traité. **Un client hostile contrôle le timing de sa réponse `__baston:nativeResult`** et peut donc infliger un DoS ciblé (jusqu'à 1 s par appel, cumulatif).

**Correction** : modéliser l'appel natif comme une vraie promesse résolue sur un tour de boucle ultérieur, pour que `run_event_loop` continue à pomper pendant l'attente.

#### 🟠 ROB-3 (latent) — Fuite d'object-id sur create rejeté (OneSync-NG)
**`crates/baston-zone/src/onesync.rs:198-202`**

`id_used[object_id] = true` est exécuté **inconditionnellement** après `apply_create`, alors qu'`apply_create` (`:392-399`) rejette silencieusement le create si un autre owner détient l'entité **ou si `entity_type` est `None`** (bits de type invalides — trivial à forger). L'id est alors marqué « used » à vie sans entité associée : ni `apply_remove` ni `remove_client` ne le libéreront → épuisement progressif des 8192 slots → DoS sur les créations légitimes.

**Latence** : `ServerGameState` n'est **pas encore branché** dans le binaire (utilisé seulement par ses tests, flag `onesync` off par défaut). À corriger **avant** activation de OneSync-NG.

**Correction** : ne marquer `id_used` que si `apply_create` a réellement inséré (le faire retourner un `bool`, ou déplacer le marquage dans `apply_create` juste avant l'`insert`). Voir CONC/STRUCT ci-dessous : la vraie racine est le modèle d'état d'id éclaté.

#### 🟡 ROB-4 — Comptabilité `id_used` / `id_leased` divergente
**`crates/baston-zone/src/onesync.rs:136-154`** — `remove_client` et l'orphelinage manipulent trois structures (`id_used: Vec<bool>`, `id_leased: Vec<bool>`, `leased: Vec<u16>` par client) qui doivent rester cohérentes ; sous churn de connexions elles dérivent. **Remède structurel** (résout aussi ROB-3) : source de vérité unique `enum IdState { Free, Leased(source), Used }` indexée par id, extraite dans un `IdAllocator` testable.

#### 🟡 ROB-5 — Starvation d'entité quand `cost > budget` (interest management)
**`crates/baston-zone/src/interest_ng.rs:167-190`** — si l'entité la plus prioritaire a `cost = 22 + data_len > budget_bytes`, elle est skippée (`continue`) à chaque tick indéfiniment et n'est jamais transmise au client (vue sparse incohérente). Hors d'atteinte avec le budget par défaut (24 KiB vs blob max 4117 o), mais réel si budget mal configuré. **Correction** : garantir la progression — si `spent == 0`, envoyer l'entité de tête quand même (dépassement d'un tick), ou lui réserver un slot.

#### 🟡 ROB-6 (latent) — Gardes `length` du bitbuffer disparaissant en release
**`crates/baston-protocol/src/rage/buffer.rs:138-172`** — `read_signed` fait `read_bits_single(length - 1)` (underflow si `length == 0`) et `read_float`/`read_long` font `1u64 << length` (shift-overflow si `length ≥ 64`), protégés seulement par `debug_assert!`. **Non atteignable aujourd'hui** (ces `pub fn` ne sont appelées nulle part hors tests), mais elles sont destinées au futur node-reader du sync-tree. **Correction préventive** : garder `if length == 0 || length > 32 { return None; }` en tête de fonction avant que ces lectures ne soient câblées.

#### ⚪ ROB-7 — Divers panics-debug sur entrées forgées / dette de fuzzing
- `object_ids.rs:56` : `gap + 1` panic en debug si `gap == u16::MAX` → `gap.wrapping_add(1)`.
- `buffer.rs:118,130` : invariant `dest`/`src` gardé par `debug_assert!` seul → durcir en retour `false`.
- **Manque un harness `cargo-fuzz`** sur les 3 vraies portes d'entrée non fiables : `decode_incoming`, `parse_nack`, `decompress_using_dict`. Quelques minutes de fuzzing = le meilleur filet sur ces fonctions où un panic = DoS.

### B. Concurrence & async

#### 🟡 CONC-1 — I/O bloquante tenue à travers un lock async
**`crates/baston-zone/src/resource_manager.rs:292-304`** — `resource_for_path` exécute deux `std::fs::canonicalize` **synchrones** par entrée dans un `.find()` **en tenant le `tokio::sync::Mutex` `resources`**. Bloque un worker tokio et sérialise tous les accès `resources` (start/stop/status) le temps des `stat` disque. **Correction** : collecter les chemins sous lock bref, relâcher, puis `tokio::fs::canonicalize().await` (ou `spawn_blocking`) hors lock.

#### 🟡 CONC-2 — Canaux non bornés sur les chemins chauds → OOM
**`crates/baston-gateway/src/udp/mod.rs:67`**, **`state_aggregator.rs`**, **`mesh_forward.rs:48`** ; **`baston-scripting/src/net_bridge.rs:26,32`** — `mpsc::unbounded_channel` sur les commandes UDP, le forward NATS et le net-bridge. À 2000 joueurs (l'aggregator émet « des dizaines de milliers de sends/s »), un consommateur lent fait croître la file sans borne → pression mémoire non bornée au lieu d'une backpressure propre. Le batch-drain (plafonné 4096) atténue en aval mais ne borne pas l'amont. **Correction** : canaux bornés `mpsc::channel(N)` avec drop explicite pour l'unreliable (le sync est superseded ~50 ms plus tard), ou instrumenter la profondeur de file.

#### 🟡 CONC-3 — `RwLock` / `Mutex` empoisonnables → panic en cascade
**`baston-scripting/src/host.rs:134,343`** (`.expect("… lock poisoned")` sur `cross_zone`, alimenté par un `Arc<dyn Fn>` externe) et **`baston-escrow-plugin/src/sidecar.rs:128,215`** (`.expect` sur `jobs`/`child`). Un panic d'un thread tenant le lock empoisonne le mutex → tout appel ultérieur panic. **Correction** : `.unwrap_or_else(|e| e.into_inner())` (données cohérentes ici) ou `parking_lot` (pas de poison). *Note : `baston-gateway` fait déjà bien ceci avec `poisoned.into_inner()` — homogénéiser.*

#### ⚪ CONC-4 — `tokio::spawn` détachés sans plafond par client
**`baston-gateway/src/udp/mod.rs:511,860-924`** — `playerJoining` et le dispatch des net events sont spawn-détachés (choix documenté pour éviter un deadlock), mais un client peut spammer `msgServerEvent` → spawn illimité. **Correction** : `JoinSet` borné ou sémaphore par source.

### C. Cohérence de typage & API

#### 🟡 TYPE-1 — `load()` de la config ne valide pas les sous-sections
**`crates/baston-config/src/lib.rs:646-659`** — `load()` **n'appelle jamais** `escrow.validate()` ni `license.validate()`, pourtant les messages d'erreur les plus actionnables du projet vivent là. Un binaire qui oublie l'appel démarre avec une config invalide sans le message soigné. **Meilleur ROI de l'audit** (le travail est fait, il suffit de le brancher) : ajouter un `self.validate()` agrégateur en fin de `load()`, ou renommer `load_unvalidated`.

#### 🟡 TYPE-2 — `String` libre là où un `enum` s'impose
**`crates/baston-config/src/lib.rs:137` (`mode`), `:226` (`backend`)** — validés par `match` sur littéraux à runtime, alors qu'`OneSyncMode` (`:374`) prouve que le pattern `#[serde(rename_all="lowercase")]` est déjà maîtrisé dans le fichier. Passer en `enum LicenseMode`/`EscrowBackend` : serde produit l'erreur « unknown variant » gratuitement et supprime deux variantes d'erreur manuelles.

#### ⚪ TYPE-3 — Casts tronquants silencieux
`baston-zone/src/onesync.rs:79,130,216` (`source as u16`), `baston-loadtest` (`as u64`/`as u32` sur f64 Prometheus). Documenter l'invariant (net ids ≤ 16 bits) ou `u16::try_from` avec log.

#### ⚪ TYPE-4 — `length_hack` inbound non appliqué en big mode
**`baston-protocol/src/rage/packet.rs:76`** — `decode_incoming` crée le buffer **sans** `.with_length_hack()`, alors que le chemin d'ack l'active (`onesync.rs:189`). En OneSync big mode, l'object_id (16 bits) serait lu sur 13 bits → désync de 3 bits, records corrompus (pas de crash — lectures bornées — mais données fausses). Vérifier si intentionnel (BASTON tourne non-OneSync) ; sinon propager le flag, ou `debug_assert!(!length_hack)` à l'entrée.

### D. Structure & idiomes

#### ⚪ STRUCT-1 — 11 fichiers > 400 lignes
| Fichier | Lignes | Note |
|---------|:------:|------|
| `baston-gateway/src/udp/mod.rs` | 975 | cohésion OK (machine à états mono-propriétaire, handlers < 50 l) ; extraire `udp/host.rs` + `udp/onesync.rs` |
| `baston-config/src/lib.rs` | 862 | découper par domaine (error/license/escrow/meshing/defaults) |
| `baston-zone/src/onesync.rs` | 665 | 3 responsabilités mêlées ; extraire `IdAllocator` (cf. ROB-4) |
| `baston-protocol/src/rage/sync_trees.rs` | 662 | — |
| `baston-zone/src/bin/baston-zone.rs` | 530 | — |
| `baston-gateway/src/state_aggregator.rs` | 511 | — |
| + 5 autres entre 403 et 474 | | — |

Aucun n'est bloquant (cohésion généralement bonne), mais le découpage améliorerait la testabilité — en particulier isoler la logique d'object-id d'`onesync.rs`, la plus sujette aux bugs.

#### ⚪ STRUCT-2 — 6 warnings clippy résiduels
`baston-zone/src/mesh.rs:33,35` (type_complexity ×2, alias `type`) · `baston-protocol/src/rage/packet.rs:398` (imports inutilisés) · `baston-gateway/src/admin.rs:57` (result_large_err → boxer la `Response`) · `baston-loadtest/src/main.rs:144` (`is_multiple_of`). **Tous corrigeables en < 15 min** et requis pour un CI vert sous `-D warnings`.

#### ⚪ STRUCT-3 — Silences potentiellement diagnostiquables
`baston-scripting/src/host.rs:200-213` (`collect_zone_transfer_state` avale les erreurs de canal sans log → perte de state silencieuse au handoff) · `baston-config/src/lib.rs:700` (override booléen d'env mal orthographié silencieusement traité `false`). Ajouter un `tracing::warn!`.

#### ⚪ STRUCT-4 — Fuite mémoire non bornée au hot-reload
**`baston-scripting/src/runtime.rs:82-84`** — `Box::leak(name.into_boxed_str())` par script. Le commentaire prétend la fuite « bounded », mais `load_resource` est **idempotent et re-appelable** (hot-reload) → fuite monotone à chaque rechargement. Stocker les noms dans une arène possédée par le `ScriptRuntime` (libérée au drop de l'isolate).

#### ⚪ STRUCT-5 — `partial_cmp().unwrap()` sur f64 → panic sur NaN
**`baston-loadtest/src/report.rs:89,168`** — un seul `NaN` de latence panique tout le rapport en fin de test (minutes de mesure perdues). Outil interne → faible, mais trivial : `partial_cmp(...).unwrap_or(Equal)` ou `sort_unstable_by(f64::total_cmp)`.

---

## 6. Process, CI & outillage

### ⚠️ PROC-1 — La CI est définie mais échouerait, et ne tourne nulle part
`.github/workflows/ci.yml` définit un bon gate : `cargo check` + `test` + **`clippy -- -D warnings`** + **`fmt --all --check`**. Mais, reproduit localement :
- `cargo clippy --workspace -- -D warnings` → **exit 101** (les 6 warnings de STRUCT-2 deviennent des erreurs).
- `cargo fmt --all -- --check` → **exit 1** (drift, ex. `baston-config/src/lib.rs:54,750`).
- **`git remote -v` est vide** → repo purement local, la CI n'a **jamais** été exécutée.

**Le gate qualité est aspirationnel, pas effectif.** Action : (a) `cargo fmt --all` + corriger les 6 warnings clippy pour repasser au vert ; (b) pousser sur un remote (GitHub) pour que la CI s'exécute réellement ; (c) envisager d'ajouter `cargo-deny`/`cargo-audit` (vérif des vulnérabilités de dépendances) au workflow.

### ⚠️ PROC-2 — `.cargo/config.toml` commité avec un chemin absolu machine-spécifique
`.cargo/config.toml` (suivi par git) force `[build] target-dir = "C:/Users/osiri/.cache/baston-target"`. Le commentaire affirme « Linux ignores the issue » — **c'est inexact** : `target-dir` est honoré partout ; sur Linux, `C:/Users/...` est un chemin relatif, cargo créerait `<repo>/C:/Users/osiri/.cache/baston-target`. Le CI runner utiliserait donc un target-dir hors du `target/` que `Swatinem/rust-cache` met en cache → cache-miss systématique (build ~12 min à chaque run). **Action** : sortir ce réglage machine-local du fichier commité (le `.gitignore` local, ou variable d'env `CARGO_TARGET_DIR` sur la machine Windows), et corriger le commentaire.

---

## 7. Plan d'action priorisé

### Palier 0 — Avant tout listing public (sécurité)
1. **SEC-1** : authentifier ou supprimer `POST /admin/player/{source}/drop`. *(critique en exposition, effort faible)*
2. **SEC-2** : rate-limit + troncature du challenge sur le chemin OOB `getinfo`. *(effort moyen)*
3. **SEC-3 / SEC-4** : token admin constant-temps + CORS restreint. *(effort faible)*

### Palier 1 — Quick wins (< 1 jour, fort ROI)
4. **TYPE-1** : brancher `validate()` dans `config::load()`. *(le meilleur rapport valeur/effort)*
5. **PROC-1** : `cargo fmt --all` + corriger les 6 warnings clippy → CI verte ; pousser sur un remote.
6. **CONC-3** : dé-paniquer les mutex empoisonnables (`into_inner()`) dans scripting + escrow.
7. **STRUCT-5** / **ROB-7** : `unwrap_or(Equal)` sur les `partial_cmp` ; `wrapping_add` sur `gap`.

### Palier 2 — Robustesse d'exécution (court terme)
8. **ROB-1** : watchdog `IsolateHandle::terminate_execution` sur le runtime de scripting.
9. **ROB-2** : dé-sérialiser l'attente des natives client hors du chemin de dispatch.
10. **CONC-2** : borner les canaux `mpsc` des chemins chauds avec politique de drop.
11. **CONC-1** : sortir `canonicalize` du lock async.
12. **STRUCT-4** : supprimer le `Box::leak` du nom de script.

### Palier 3 — Avant activation de OneSync-NG
13. **ROB-3 + ROB-4** : `IdAllocator` unifié (`enum IdState`) — corrige la fuite d'id, la divergence de comptabilité, et facilite le découpage d'`onesync.rs`.
14. **ROB-5** : garantir la progression du budget d'interest management.
15. **ROB-6 / TYPE-4** : durcir les gardes `length` du bitbuffer et clarifier le `length_hack` inbound **avant** de câbler le node-reader du sync-tree.
16. **Dette de test** : cible `cargo-fuzz` sur `decode_incoming`/`parse_nack`/`decompress_using_dict` ; tests de rejet de create + réutilisation d'id dans `onesync`.

### Palier 4 — Hygiène (opportuniste)
17. **TYPE-2** : `mode`/`backend` en `enum`. · **STRUCT-1** : découper les fichiers > 400 l en commençant par `onesync.rs`. · **PROC-2** : sortir le `target-dir` machine-local du repo.

---

## 8. Ce qui est déjà très bien (à préserver)

- **Parsing de données non fiables** (`baston-protocol`, `baston-zone`) : `Option`/`Result` partout, bornes systématiques, LZ4 plafonné (pas de bombe de décompression), boucles de décodage terminantes. Un modèle.
- **Auth CFX** (`baston-gateway/src/auth/`) : RSA offline, anti-rejeu + GC, bien testée.
- **`baston-core`** : fail-closed par construction, aucun chemin n'élève un droit de licence.
- **Messages d'erreur de `baston-config`** : chaque variante embarque la correction exacte à écrire dans `baston.toml`. Parmi les meilleurs vus.
- **Défauts sûrs** : `auth_bypass=false`, escrow off, aucun secret en dur, `admin_token` vide = API désactivée.
- **`unsafe` FFI LZ4** : les 6 blocs sont *sound*, avec buffers pré-alloués au pire cas et commentaires `// SAFETY:` justes.
- **Documentation** : ratio doc/code ~2:1, 61 docs de module `//!`.

---

*Annexe — Fichiers les plus cités : `baston-gateway/src/{http/mod.rs, http/client.rs, udp/oob.rs, admin.rs, udp/mod.rs}` · `baston-scripting/src/{host.rs, runtime.rs, extensions.rs}` · `baston-zone/src/{onesync.rs, interest_ng.rs, resource_manager.rs}` · `baston-protocol/src/rage/{buffer.rs, packet.rs, object_ids.rs}` · `baston-config/src/lib.rs`.*
