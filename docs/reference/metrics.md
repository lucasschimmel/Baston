---
title: "Metrics reference"
description: "Every Prometheus metric BASTON emits, what it measures, and which ones tell you a server is in trouble."
---

BASTON exposes Prometheus metrics on `metrics.port` (default `9090`) when the
`metrics` module is on — which it is by default.

```bash
curl -s localhost:9090/metrics | head
```

Instrumentation lives in the core and is always compiled in; only the exporter
is a module. Turning `metrics` off costs you the endpoint, not the numbers'
existence.

Ready-made Grafana dashboards are in `deploy/monitoring/dashboards/`, and alert
rules in `deploy/monitoring/alerts.yml`.

Two things to know before you build your own:

- **No `HELP` or `TYPE` metadata.** BASTON does not call `describe_*`, so the
  scrape carries bare samples. Anything that relies on metric metadata will
  find none.
- **Naming is not uniform.** Some metrics carry a `baston_` prefix, most do
  not. Treat the names below as the authority rather than guessing by pattern.

## The five that matter

If you watch nothing else, watch these.

| Metric | Healthy | What it means when it is not |
| --- | --- | --- |
| `onesync_tick_hz` | at `tick_default_hz` (60) | The server is shedding tick rate to keep up. Look at `onesync_tick_utilization`. |
| `onesync_tick_utilization` | < 0.85 | Above `tick_high_utilization`, the adaptive tick starts backing off. Sustained > 1.0 means you are over capacity. |
| `state_sync_tick_jitter_ms` | p99 < 2 ms | The sync loop is not being scheduled on time. On Windows, check the timer resolution; elsewhere, check CPU contention. |
| `baston_script_dispatch_duration_seconds` | p95 in single-digit ms | One resource is doing too much work in a handler. The label tells you which. |
| `baston_players_online` | — | Your capacity denominator for everything else. |

## Scripting

| Metric | Type | Labels | Measures |
| --- | --- | --- | --- |
| `baston_script_dispatch_total` | counter | `resource`, `kind`, `event`, `status` | Every handler dispatch. `kind` is `LoadScript`, `Event`, `NetEvent`, `PlayerConnecting`, `Command`, `ZoneTransferState`, `NativeRoundtrip`. |
| `baston_script_dispatch_duration_seconds` | histogram | `resource`, `kind`, `event` | Wall time of one dispatch. |
| `baston_script_watchdog_terminations_total` | counter | `resource` | A runaway script was force-terminated. **Any non-zero value is a bug in a resource.** |
| `baston_scripts_loaded_total` | counter | `status` (`plain`, `decrypted`, `error`) | Script files loaded. |
| `baston_native_roundtrip_duration_seconds` | histogram | `resource` | Server → client native call latency. |
| `baston_native_roundtrip_timeouts_total` | counter | `resource` | Client never answered a native call. |
| `script_native_unimplemented_total` | counter | `native` | A resource called a native BASTON does not implement. Tells you exactly what to implement next. |
| `script_native_rpc_dispatch_total` | counter | `native` | Context natives routed to a client for execution. |
| `script_native_rpc_no_owner_total` | counter | `native` | No client owned the target entity. Broken out separately because it is *expected* on a healthy non-OneSync server. |
| `script_native_rpc_skipped_total` | counter | `native`, `reason` | Routed native dropped — bad arity, no target, bridge full. |
| `baston_decrypt_duration_seconds` | histogram | — | Escrow decryption time per file. |

`script_native_unimplemented_total` is the most useful metric in this table when
porting a server: it turns "something doesn't work" into a list of native names.

### Profiler

| Metric | Type | Labels | Measures |
| --- | --- | --- | --- |
| `baston_profiler_active` | gauge | — | 1 while a capture is running. |
| `baston_profiler_recordings_total` | counter | `status` | Captures started, by outcome. |

## Entity state and sync

| Metric | Type | Labels | Measures |
| --- | --- | --- | --- |
| `onesync_tick_hz` | gauge | — | Current sync rate. |
| `onesync_tick_utilization` | gauge | — | Fraction of the tick budget consumed. |
| `onesync_tick_work_seconds` | histogram | — | Work done per tick. |
| `onesync_tick_overruns_total` | counter | — | Ticks that exceeded their budget. |
| `onesync_tick_rate_transitions_total` | counter | `reason` | Adaptive-tick rate changes. `reason` names what forced it: `DeadlineMiss`, `QueuePressure`, `WorkOverload`, or headroom recovery. |
| `state_sync_tick_jitter_ms` | histogram | — | Difference between intended and actual tick spacing. |
| `entities_dirty_per_tick` | gauge | — | Entities changed since the last emit. |
| `world_state_entities` | gauge | — | Entities in the authoritative world. |
| `entities_per_client` | histogram | — | How many entities each client is tracking. |
| `snapshot_bytes_sent` | counter | — | Bytes of entity snapshot pushed to clients. |
| `state_updates_accepted` | counter | — | Client position updates accepted. |
| `state_updates_rejected` | counter | `reason` (`teleport`, `not_owner`) | Rejected client updates. `teleport` is a `max_speed_mps` violation; `not_owner` is a client trying to move an entity it does not own. |
| `state_batches_lost_total` | counter | — | NATS batches that never arrived. |
| `world_spawn_failures_total` | counter | — | Script entity creation that failed. |
| `world_commands_dropped_total` | counter | — | World commands dropped under backpressure. |

A climbing `state_updates_rejected{reason="teleport"}` for one player is either a
cheater or a `max_speed_mps` that is too low for your gamemode. Check which
before you act — a server with fast vehicles or teleport scripts produces this
legitimately.

## Networking

| Metric | Type | Labels | Measures |
| --- | --- | --- | --- |
| `baston_players_online` | gauge | — | Connected players. |
| `udp_plane_queue_depth` | gauge | `plane` (`control`, `sync`) | Outbound queue depth per traffic plane. Sustained growth means you are sending faster than the link drains. |
| `udp_plane_dropped_total` | counter | `plane`, `reliable` | Commands dropped on a full plane queue. A non-zero `reliable="true"` is serious: something a client needed was thrown away. |
| `udp_ingress_rejected_total` | counter | `reason` (`wrong_channel`, `routing_bucket`) | Inbound game messages refused. BASTON enforces channel separation more strictly than FXServer. |
| `nats_bytes_published` | counter | — | Bytes published to NATS. |
| `nats_publish_duration_ms` | histogram | — | NATS publish latency. |

## Resource downloads

What clients pull when they join.

| Metric | Type | Labels | Measures |
| --- | --- | --- | --- |
| `resource_download_requests_total` | counter | `method` (`GET`, `HEAD`) | Requests to `/files`. |
| `resource_download_active` | gauge | — | Downloads in flight. Capped by `file_download_concurrency`. |
| `resource_download_bytes_total` | counter | — | Bytes served. |
| `resource_download_timeouts_total` | counter | — | Downloads that exceeded `file_download_timeout_secs`. |
| `resource_download_rejections_total` | counter | `reason` | Refused downloads. `canonical_escape` and `invalid_path` are path-traversal attempts; `not_allowlisted` is a file the manifest never declared; `concurrency` means you hit `file_download_concurrency`; `range` is a malformed Range header. |

## Script HTTP

| Metric | Type | Measures |
| --- | --- | --- |
| `script_http_requests_total` | counter | Outbound `PerformHttpRequest` calls. |
| `script_http_requests_failed_total` | counter | Of those, failures. |
| `script_http_dropped_total` | counter | Dropped because `http_concurrency` was saturated. |
| `script_http_handler_requests_total` | counter | Inbound requests to a `SetHttpHandler`. |
| `script_http_handler_failed_total` | counter | Handler errors. |
| `script_http_handler_timeouts_total` | counter | Handlers that exceeded `http_handler_timeout_secs`. |

## Database

| Metric | Type | Labels | Measures |
| --- | --- | --- | --- |
| `baston_db_queries_total` | counter | `resource`, `status` | Queries, by resource and outcome. |
| `baston_db_query_duration_seconds` | histogram | — | Query latency, including time queued behind the pool. |

If latency is high but your database is idle, `pool_size` is too small: the time
is spent waiting for a connection, not running SQL.

## Persistence

| Metric | Type | Measures |
| --- | --- | --- |
| `kvp_flush_failures_total` | counter | KVP writes that failed to reach disk. **Non-zero means resources are losing data.** |
| `state_bag_changes_dropped_total` | counter | State-bag changes dropped, labelled `queue` (`callback` or `replication`). Handlers or clients missed an update. |
| `script_resource_commands_dropped_total` | counter | Resource start/stop commands dropped under backpressure. |

## Admin API

| Metric | Type | Labels | Measures |
| --- | --- | --- | --- |
| `baston_api_audit_total` | counter | `action`, `outcome` | Every audited control action. Note `outcome` buckets a *denied* attempt as `error`, so a rise here can mean someone is probing with a key that lacks permissions — cross-check the audit log. |

## Multi-zone federation

Only meaningful with `[meshing] enabled = true`.

### Handoffs

| Metric | Type | Labels | Measures |
| --- | --- | --- | --- |
| `entity_handoffs_total` | counter | — | Handoffs attempted. |
| `handoffs_committed_total` | counter | — | Handoffs that completed. |
| `handoff_total_duration_ms` | histogram | — | End-to-end handoff time. |
| `handoff_hold_buffered_updates` | histogram | — | Updates buffered during the 50 ms hold. |
| `handoff_routing_lock_held_us` | histogram | — | How long the routing table was locked. |
| `entity_handoff_zone_lookups` | histogram | — | Zone resolutions per handoff. |
| `handoff_prepare_failures_total` | counter | `zone` | Target zone refused to prepare. |
| `handoff_prepare_timeouts_total` | counter | `zone` | Target zone did not answer. |
| `handoff_prepare_errors_total` | counter | — | Transport errors during prepare. |
| `handoff_activate_failures_total`, `handoff_activate_errors_total` | counter | — | The activate phase. |
| `handoff_confirm_errors_total` | counter | — | The confirm phase. |
| `handoff_rollbacks_total`, `handoff_rollback_failures_total` | counter | — | Aborted handoffs, and rollbacks that themselves failed. |

`handoffs_committed_total` should track `entity_handoffs_total`. A widening gap
is a boundary problem, not a load problem — check `boundary_margin` and
`handoff_cooldown_secs`.

### Zone health

| Metric | Type | Labels | Measures |
| --- | --- | --- | --- |
| `zone_heartbeat_failures_total` | counter | — | Missed zone heartbeats. |
| `zone_failures_total` | counter | `zone` | Zones declared dead after `zone_timeout_secs`. |
| `zone_recovery_players_rerouted_total` | counter | — | Players moved off a dead zone. |
| `zone_recovery_activation_failures_total` | counter | — | Reroutes that failed. |
| `mesh_forward_dropped_total` | counter | — | Client updates dropped in forwarding. |
| `mesh_forward_failures_total` | counter | `zone` | Forwarding failures per zone. |
| `mesh_forward_control_deferred_total` | counter | — | Control messages deferred during a handoff hold. |
| `mesh_release_failures_total` | counter | — | Failures releasing a player from a zone. |

## Cardinality

Several metrics are labelled by things that grow with your server. On a busy
one these become the bulk of your Prometheus storage:

| Metric family | Labelled by | Grows with |
| --- | --- | --- |
| `baston_script_dispatch_*` | `resource` × `kind` × `event` | number of distinct event names |
| `baston_native_roundtrip_*` | `resource` × native `hash` | number of distinct natives called |
| `script_native_rpc_*`, `script_native_unimplemented_total` | `native` | same |
| `handoff_*`, `mesh_forward_*`, `zone_failures_total` | `zone` | number of zones (bounded) |

The event-name and native-hash dimensions are the ones to watch: a resource
that generates event names dynamically — including a player id in the name, say
— will produce unbounded cardinality. That is a resource bug, and this is where
you see it.

## Voice emits nothing

The `baston-voice` crate has no instrumentation at all. If you run voice, its
health is not visible in Prometheus today — watch the logs instead.

## Alerting

`deploy/monitoring/alerts.yml` ships rules for the conditions worth paging on.
If you write your own, the shape that has proven useful:

- **Symptom, not cause.** Alert on `onesync_tick_hz < 40 for 5m`, not on CPU.
- **Any watchdog termination is a page.** It means a resource wedged a runtime.
- **`kvp_flush_failures_total` increasing is a page.** Players are losing data
  and nobody will notice for hours.
