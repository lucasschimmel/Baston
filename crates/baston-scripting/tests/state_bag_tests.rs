use std::sync::Arc;

use baston_scripting::{DeferralRegistry, RoutingLockdownMode, ScriptHost, ScriptSource};

fn host() -> ScriptHost {
    ScriptHost::spawn(
        Arc::new(DeferralRegistry::new()),
        Arc::new(baston_protocol::PlayerDirectory::new()),
    )
    .expect("host spawn")
}

#[tokio::test]
async fn js_handlers_receive_serializable_deliveries_and_errors_are_isolated() {
    let host = host();
    host.load_resource(
        "state-test",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                AddStateBagChangeHandler("health", "entity:42", () => {
                  throw new Error("expected handler failure");
                });
                AddStateBagChangeHandler("health", "entity:42",
                  (bag, key, value, reserved, replicated) => {
                    SetStateBagValue("global:result", "delivery", {
                      bag, key, value, reserved, replicated
                    }, 0, false);
                  }
                );
                SetStateBagValue("entity:42", "health", 75, 0, true);
            "#
            .into(),
        }],
    )
    .await
    .expect("resource load and callback dispatch");

    assert_eq!(
        host.state_bags().get("global:result", "delivery"),
        Some(serde_json::json!({
            "bag": "entity:42",
            "key": "health",
            "value": 75,
            "reserved": 0,
            "replicated": true,
        }))
    );
    let replicated = host.drain_replicated_state_bags(16);
    assert_eq!(replicated.len(), 1);
    assert_eq!(replicated[0].bag, "entity:42");
    assert_eq!(replicated[0].key, "health");
}

#[tokio::test]
async fn routing_bucket_natives_update_shared_control_and_synthetic_entities() {
    let host = host();
    host.load_resource(
        "routing-test",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                const entity = CreateObject(123, 0, 0, 0, true, true, false);
                SetEntityRoutingBucket(entity, 17);
                SetPlayerRoutingBucket(23, 17);
                SetRoutingBucketEntityLockdownMode(17, "strict");
                SetRoutingBucketPopulationEnabled(17, false);
                SetStateBagValue("global:routing-test", "entity", entity, 0, false);
                SetStateBagValue(
                  "global:routing-test",
                  "entityBucket",
                  GetEntityRoutingBucket(entity),
                  0,
                  false
                );
                SetStateBagValue(
                  "global:routing-test",
                  "playerBucket",
                  GetPlayerRoutingBucket(23),
                  0,
                  false
                );
            "#
            .into(),
        }],
    )
    .await
    .expect("routing resource load");

    let bags = host.state_bags();
    let entity = bags
        .get("global:routing-test", "entity")
        .and_then(|value| value.as_u64())
        .expect("synthetic entity id") as u32;
    assert_eq!(
        bags.get("global:routing-test", "entityBucket"),
        Some(serde_json::json!(17))
    );
    assert_eq!(
        bags.get("global:routing-test", "playerBucket"),
        Some(serde_json::json!(17))
    );

    let routing = host.routing_control();
    assert_eq!(routing.entity_bucket(entity), 17);
    assert_eq!(routing.player_bucket(23), 17);
    assert_eq!(routing.lockdown_mode(17), RoutingLockdownMode::Strict);
    assert!(!routing.population_enabled(17));
}
