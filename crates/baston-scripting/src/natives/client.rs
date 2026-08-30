//! Server → client native dispatch (`invoke_native_on_client`).

#[cfg(feature = "js")]
use std::time::Instant;

/// How long a server → client native call may wait for its result.
///
/// This is also the upper bound on how long one such call can stall the
/// resource's runtime: the host command loop runs one dispatch to completion at
/// a time, so while a handler awaits a native result no other event for that
/// resource is serviced (see `invoke_native_on_client`). Keep it low enough
/// to bound a slow/hostile client's impact, but above realistic client RTT so
/// legitimate results aren't dropped. Removing the stall entirely needs the
/// host loop to drive dispatches concurrently on the shared isolate (tracked
/// separately — it's a redesign of the execution model, not a local change).
#[cfg(any(feature = "js", feature = "lua"))]
pub(crate) const NATIVE_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

/// Queue one native call on the net bridge, addressed to `source`'s client.
///
/// Shared by the awaited path ([`invoke_native_on_client`]) and by the
/// fire-and-forget context dispatcher ([`super::rpc`]) so both put the
/// exact same `__baston:invokeNative` payload on the wire. Returns `false` when
/// the bridge is full or closed — backpressure, not a fatal error.
pub(crate) fn queue_native_call(
    net: &crate::net_bridge::NetBridge,
    source: u32,
    id: u64,
    hash: u64,
    args: Vec<serde_json::Value>,
) -> bool {
    use baston_protocol::native::{NativeCall, INVOKE_NATIVE_EVENT};

    let call = NativeCall {
        id,
        hash: format!("0x{hash:016X}"),
        args,
    };
    net.tx
        .try_send(crate::net_bridge::NetOutbound::ClientEvent {
            source,
            event: INVOKE_NATIVE_EVENT.to_owned(),
            args_json: serde_json::json!([call]).to_string(),
        })
        .is_ok()
}

/// Dispatch a GTA native to `source`'s client via the BASTON shim and await
/// the result. Returns a JSON string; errors are `{"__error": "..."}` so the
/// polyfill can throw without deno_core error plumbing.
/// Dispatch a native to a client and await its answer.
///
/// Takes the three services it needs by value rather than borrowing
/// [`NativeState`]: the call awaits a client round trip, and holding a borrow
/// across that await would pin the runtime's state for the whole flight.
#[cfg(feature = "js")]
pub(crate) async fn invoke_native_on_client(
    net: crate::net_bridge::NetBridge,
    observability: std::sync::Arc<crate::observability::Observability>,
    resource: String,
    source: u32,
    hash_hex: String,
    args_json: String,
    expects_return: bool,
) -> String {
    fn err(message: impl std::fmt::Display) -> String {
        serde_json::json!({ "__error": message.to_string() }).to_string()
    }

    let hash = match u64::from_str_radix(hash_hex.trim_start_matches("0x"), 16) {
        Ok(h) => h,
        Err(e) => return err(format!("invalid native hash {hash_hex}: {e}")),
    };
    let args: Vec<serde_json::Value> = match serde_json::from_str(&args_json) {
        Ok(a) => a,
        Err(e) => return err(format!("invalid native args: {e}")),
    };

    static REGISTRY: std::sync::OnceLock<baston_protocol::native::NativeRegistry> =
        std::sync::OnceLock::new();
    if let Err(e) = REGISTRY
        .get_or_init(baston_protocol::native::NativeRegistry::new)
        .validate(hash, args.len())
    {
        return err(e);
    }

    let started = Instant::now();
    let (id, rx) = net.pending_natives.register();
    if !queue_native_call(&net, source, id, hash, args) {
        net.pending_natives.cancel(id);
        observability.record_native_roundtrip(
            &resource,
            hash,
            source,
            started.elapsed().as_micros() as u64,
            false,
            true,
        );
        return err("net bridge full or closed");
    }

    if !expects_return {
        // Fire-and-forget: the shim still replies, but nobody waits.
        net.pending_natives.cancel(id);
        observability.record_native_roundtrip(
            &resource,
            hash,
            source,
            started.elapsed().as_micros() as u64,
            false,
            false,
        );
        return "null".to_owned();
    }

    match tokio::time::timeout(NATIVE_CALL_TIMEOUT, rx).await {
        Ok(Ok(value)) => {
            observability.record_native_roundtrip(
                &resource,
                hash,
                source,
                started.elapsed().as_micros() as u64,
                false,
                false,
            );
            value.to_string()
        }
        Ok(Err(_)) => {
            net.pending_natives.cancel(id);
            observability.record_native_roundtrip(
                &resource,
                hash,
                source,
                started.elapsed().as_micros() as u64,
                false,
                true,
            );
            err("native result channel closed")
        }
        Err(_) => {
            net.pending_natives.cancel(id);
            observability.record_native_roundtrip(
                &resource,
                hash,
                source,
                started.elapsed().as_micros() as u64,
                true,
                true,
            );
            err(format!("native call 0x{hash:016X} timed out"))
        }
    }
}
