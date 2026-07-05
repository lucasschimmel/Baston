# Keymaster capture harness

Goal: capture the **CFX platform handshake** a real FXServer performs — the one
step missing from the local engine-source mirror — so BASTON can reproduce it
and become a *registered* server (server list, OneSync slot entitlements,
client policy features like custom-clothing streaming).

## What it captures

FXServer boots, validates `sv_licenseKey`, then talks to CFX. This harness
routes all of FXServer's HTTPS through a local mitmproxy that logs only the
platform hosts:

| Step | Host | What we learn |
|------|------|---------------|
| ① license → tokens | `portal-api.cfx.re` | **the missing link**: request shape + the `nucleusToken` / `listingToken` / `sv_licenseKeyToken` returned |
| ② nucleus register | `cfx.re/api/register` | reverse-proxy host assignment |
| ③ server-list heartbeat | `servers-frontend.fivem.net/api/serverlist/ingress` | listing payload + cadence |
| ④ client policy | `policy-live.fivem.net` | entitlement array (only fires when a client connects) |
| pools | `gss.cfx-services.net` | pool-size limits |

Interception works with **no CA-trust setup** because FXServer uses
`CURLOPT_SSL_VERIFYPEER=0` (HttpClient.cpp) — it accepts mitmproxy's cert.

## Run it

1. Paste your real Pebble key into `capture.cfg` → `sv_licenseKey "cfxk_..."`.
2. `powershell -ExecutionPolicy Bypass -File .\run-capture.ps1`
3. Wait ~30–60 s until the console prints the `serverlist/ingress` line, then
   Ctrl+C.
4. Read `cfx-capture.jsonl` (one JSON record per request).

## ⚠️ Secret hygiene

Your license key travels in the `portal-api.cfx.re` request and **will appear**
in `cfx-capture.jsonl`. Review and redact the key before sharing that file.
`cfx-capture.jsonl` and `capture.cfg` (once keyed) are git-ignored here.

## If nothing is captured

FXServer's libcurl should honour the `HTTPS_PROXY` env var the script sets. If
the capture file stays empty, set the **Windows system proxy** to
`http://127.0.0.1:8080` (Settings → Network → Proxy) and rerun — libcurl falls
back to the WinHTTP proxy config on some builds.
