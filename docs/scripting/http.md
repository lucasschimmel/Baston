---
title: "HTTP"
description: "Serving HTTP from a resource and calling out to other services — both JavaScript-only, with real limits."
---

Two independent things, both **JavaScript-only**. Neither is available in Lua:
`SetHttpHandler` does nothing there, and `PerformHttpRequest` resolves to an
unimplemented native.

## Serving requests

`SetHttpHandler` registers a handler for `/<your-resource>` and everything under
it, on the **game HTTP port** (default 30120).

```javascript
SetHttpHandler((request, response) => {
  if (request.path === "/status") {
    response.writeHead(200, { "Content-Type": "application/json" });
    response.send(JSON.stringify({ players: GetPlayers().length }));
    return;
  }
  response.writeHead(404);
  response.send("not found");
});
```

```bash
curl http://localhost:30120/my-resource/status
```

### The request object

| Field | |
| --- | --- |
| `address` | the peer address |
| `headers` | request headers |
| `method` | `GET`, `POST`, … |
| `path` | the path **below** your resource prefix |
| `setDataHandler(cb)` | delivers the body |
| `setCancelHandler()` | **a no-op** |

The body is **already buffered whole** before your handler runs;
`setDataHandler` hands it over on the microtask queue. There is no streaming
ingest.

```javascript
SetHttpHandler((req, res) => {
  req.setDataHandler((body) => {
    const payload = JSON.parse(body);
    res.writeHead(200);
    res.send("ok");
  });
});
```

### The response object

`writeHead(code, headers)`, `write(chunk)`, `send(chunk?)`.

`write` **buffers** — it does not stream to the client. A second `send()` is
ignored.

### Limits

| Limit | Default | Setting |
| --- | --- | --- |
| Request body | 1 MiB → `413` | `resources.http_request_max_bytes` |
| Handler deadline | 15 s → `504` | `resources.http_handler_timeout_secs` |

A resource with no handler answers `404` — deliberately indistinguishable from
"no such resource", so the endpoint does not enumerate your resources. A handler
that throws answers `500`. A resource stopped mid-request answers `503`.

Registration is dropped when the resource stops or reloads. Re-registering
replaces the previous handler.

### Before you expose this

This runs on the **public game port**. Anyone who can reach your server can
reach your handler.

- Authenticate anything that is not public. There is no built-in auth.
- Validate everything; the body is attacker-controlled.
- Gateway routes are matched first, so you cannot shadow `/info.json`,
  `/client` or `/files`.
- For server administration, use the [admin API](../reference/api.md) instead —
  it has real per-key permissions and an audit log.

## Calling out

```javascript
PerformHttpRequest(
  "https://api.example.com/thing",
  (status, body, headers, errorData) => {
    if (status !== 200) {
      console.error(`request failed: ${status} ${errorData ?? ""}`);
      return;
    }
    console.log(JSON.parse(body));
  },
  "POST",
  JSON.stringify({ hello: "world" }),
  { "Content-Type": "application/json" }
);
```

Signature: `PerformHttpRequest(url, callback, method?, data?, headers?)`,
returning a token. Non-string `data` is JSON-stringified for you; array header
values are joined with `", "`.

The callback receives `(statusCode, responseText, responseHeaders, errorData)`.

### Failure shapes

| Symptom | Meaning |
| --- | --- |
| returns token `0`, callback fires immediately with status `0` | the request could not be queued — the 1024-deep queue is full |
| `status === 0` with `errorData` set | the request failed: DNS, TLS, connection, timeout |

Always check `status`. A network failure is `0`, not an exception.

### Limits

| Limit | Default | Setting |
| --- | --- | --- |
| Timeout | 30 s, total | `resources.http_request_timeout_secs` |
| Concurrency | 32 in flight, server-wide | `resources.http_concurrency` |
| Response size | 5 MiB | `resources.http_response_max_bytes` |
| Queue depth | 1024 pending | — |

Only `http` and `https` are accepted. The User-Agent is `BASTON/<version>`.

The concurrency limit is **shared by every resource**. One resource making slow
calls delays the others; that is what the limit is for.

### There is no SSRF protection

BASTON does not filter private addresses, loopback or link-local. A resource
that takes a URL from a player and passes it to `PerformHttpRequest` lets that
player probe your internal network:

```javascript
// Do not do this.
on("fetch:url", (url) => PerformHttpRequest(url, cb));
```

Only call URLs your resource controls, or validate against an allowlist you
wrote.

## Watching it

```bash
curl -s localhost:9090/metrics | grep script_http_
```

| Metric | Watch for |
| --- | --- |
| `script_http_dropped_total` | you are saturating `http_concurrency` |
| `script_http_requests_failed_total` | an upstream is down |
| `script_http_handler_timeouts_total` | your handler is too slow |

## Doing this from Lua

You cannot, directly. Two options:

1. Put the HTTP work in a small JavaScript resource and talk to it over events.
2. Do the work outside BASTON and drive the server through the
   [admin API](../reference/api.md).

## Next

- [Events](events.md)
- [Monitoring and control API](../reference/api.md)
