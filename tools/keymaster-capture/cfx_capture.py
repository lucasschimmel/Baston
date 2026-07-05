"""mitmproxy addon — capture only the CFX platform handshake traffic.

FXServer builds its HTTPS requests with libcurl and CURLOPT_SSL_VERIFYPEER=0
(HttpClient.cpp), so mitmproxy's interception cert is accepted without any
extra trust setup. We filter to the platform hosts involved in licensing /
listing / policy and dump full request + response (headers + body) as JSONL,
so the licensing handshake can be reverse-engineered field by field.

Run via run-capture.ps1 — do not launch directly.
"""

import json
import os
from mitmproxy import http

# Hosts that make up the license / registration / policy flow. Substring match
# so subdomains and path-only variants are covered.
CFX_HOSTS = (
    "portal-api.cfx.re",          # ① license key → tokens (the missing link)
    "cfx.re/api/register",        # ② nucleus registration
    "servers-frontend.fivem.net", # ③ server-list ingress heartbeat
    "policy-live.fivem.net",      # ④ client entitlement policy
    "gss.cfx-services.net",       # pool-size limits
    "keymaster.fivem.net",        # legacy keymaster (in case)
    "lambda.fivem.net",           # ticket pubkey (already known — sanity)
    "nucleus.cfx.re",
    "users.cfx.re",
)

OUT_PATH = os.environ.get("CFX_CAPTURE_OUT", "cfx-capture.jsonl")


def _is_cfx(flow: http.HTTPFlow) -> bool:
    url = flow.request.pretty_url
    return any(h in url for h in CFX_HOSTS)


def _body_repr(message) -> dict:
    raw = message.raw_content or b""
    try:
        text = raw.decode("utf-8")
        # Keep structured bodies as parsed JSON when possible.
        try:
            return {"json": json.loads(text)}
        except (ValueError, json.JSONDecodeError):
            return {"text": text}
    except UnicodeDecodeError:
        return {"base64_len": len(raw)}


def response(flow: http.HTTPFlow) -> None:
    if not _is_cfx(flow):
        return
    record = {
        "url": flow.request.pretty_url,
        "method": flow.request.method,
        "req_headers": dict(flow.request.headers),
        "req_body": _body_repr(flow.request),
        "status": flow.response.status_code if flow.response else None,
        "resp_headers": dict(flow.response.headers) if flow.response else {},
        "resp_body": _body_repr(flow.response) if flow.response else {},
    }
    line = json.dumps(record, ensure_ascii=False, indent=None)
    with open(OUT_PATH, "a", encoding="utf-8") as fh:
        fh.write(line + "\n")
    # Console breadcrumb so the operator sees traffic landing live.
    print(f"[cfx-capture] {flow.request.method} {flow.request.pretty_url} "
          f"-> {record['status']}")
