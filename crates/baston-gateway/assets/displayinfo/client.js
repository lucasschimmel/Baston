// BASTON `displayinfo` overlay.
//
// This file ships inside the server binary; it is not a resource an operator
// installs, and it cannot be edited, replaced or stopped from the resources
// directory. Its only job is to draw what the server sends.
//
// Deliberately dumb: it calls no game native to *measure* anything. Every
// number on screen is the server's own reading, which is the point. An overlay
// that asked the client for its ping would agree with the client even when the
// server disagrees, and that disagreement is exactly what an operator is
// looking for.

const TOGGLE_EVENT = 'baston:displayInfo:toggle';
const SNAPSHOT_EVENT = 'baston:displayInfo';
const DENIED_EVENT = 'baston:displayInfo:denied';

// Wire version this renderer understands. A server newer than the cached
// packfile is refused rather than drawn with labels that no longer match.
const SUPPORTED_VERSION = 1;

// A snapshot older than this is drawn dimmed with an explicit age, so a frozen
// overlay is never mistaken for a healthy one.
const STALE_MS = 2000;

// Verbosity levels, in the spirit of `r_DisplayInfo`.
const LEVEL_OFF = 0;
const LEVEL_BASIC = 1; // server + net + position
const LEVEL_ONESYNC = 2; // + entity state
const LEVEL_MESH = 3; // + zone topology
const MAX_LEVEL = LEVEL_MESH;

// Layout: right-aligned against the safe area, like the reference overlay.
const RIGHT = 0.985;
const TOP = 0.02;
const LINE_HEIGHT = 0.0195;
const SCALE = 0.29;

const WHITE = [220, 220, 220];
const GREEN = [130, 240, 130];
const AMBER = [245, 205, 100];
const RED = [245, 110, 110];
const DIM = [150, 150, 150];

let level = LEVEL_OFF;
let snapshot = null;
let receivedAt = 0;
let notice = null;
let noticeAt = 0;

// A refusal is worth reading once, not forever.
const NOTICE_MS = 8000;

function setNotice(text) {
  notice = text;
  noticeAt = GetGameTimer();
}

function setLevel(next) {
  const wanted = Math.max(LEVEL_OFF, Math.min(MAX_LEVEL, next | 0));
  if (wanted === level) return;
  const wasOff = level === LEVEL_OFF;
  level = wanted;
  if (level === LEVEL_OFF) {
    snapshot = null;
    emitNet(TOGGLE_EVENT, false);
  } else if (wasOff) {
    notice = null;
    emitNet(TOGGLE_EVENT, true);
  }
}

onNet(SNAPSHOT_EVENT, (data) => {
  if (!data || data.v !== SUPPORTED_VERSION) {
    setNotice(
      `displayinfo: server sent v${data && data.v}, this client understands v${SUPPORTED_VERSION}`,
    );
    snapshot = null;
    return;
  }
  snapshot = data;
  receivedAt = GetGameTimer();
});

onNet(DENIED_EVENT, (reason) => {
  // Refused: say why and stop asking, rather than leaving an empty overlay on
  // screen with no explanation.
  setNotice(`displayinfo: ${reason}`);
  level = LEVEL_OFF;
  snapshot = null;
});

RegisterCommand(
  'displayinfo',
  (_source, args) => {
    const requested = args.length > 0 ? parseInt(args[0], 10) : null;
    if (requested !== null && !Number.isNaN(requested)) {
      setLevel(requested);
    } else {
      // No argument cycles, so the overlay is reachable without remembering
      // the levels.
      setLevel(level >= MAX_LEVEL ? LEVEL_OFF : level + 1);
    }
    console.log(`displayinfo level ${level}`);
  },
  false,
);

// --- drawing ---------------------------------------------------------------

function drawLine(y, text, colour) {
  SetTextFont(4);
  SetTextScale(SCALE, SCALE);
  SetTextColour(colour[0], colour[1], colour[2], 255);
  // An outline keeps the text legible over both the sky and a dark interior;
  // the reference overlay relies on the same trick.
  SetTextDropShadow();
  SetTextOutline();
  SetTextJustification(2);
  // The wrap window's left edge must be far enough left that a long line is
  // never clipped; right-justification measures from its right edge.
  SetTextWrap(0.0, RIGHT);
  BeginTextCommandDisplayText('STRING');
  AddTextComponentSubstringPlayerName(text);
  EndTextCommandDisplayText(RIGHT, y);
}

function fixed(value, digits) {
  return typeof value === 'number' && Number.isFinite(value)
    ? value.toFixed(digits)
    : '?';
}

function duration(totalSeconds) {
  const s = Math.max(0, totalSeconds | 0);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return `${h}h${String(m).padStart(2, '0')}m${String(s % 60).padStart(2, '0')}s`;
}

function clockUtc(unixSeconds) {
  const d = new Date(unixSeconds * 1000);
  const pad = (n) => String(n).padStart(2, '0');
  return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())} ${pad(
    d.getUTCHours(),
  )}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}Z`;
}

/// Colour a value by how close it is to trouble: green below `warn`, amber
/// between, red above `bad`.
function threshold(value, warn, bad) {
  if (value >= bad) return RED;
  if (value >= warn) return AMBER;
  return GREEN;
}

function serverLines(out, s) {
  out.push([`${s.name}   build ${s.build}   up ${duration(s.uptime_secs)}`, GREEN]);
  out.push([clockUtc(s.unix_time), DIM]);
  const tick =
    s.tick_hz > 0
      ? `tick ${s.tick_hz}Hz  ${fixed(s.tick_ms, 2)}ms  ${fixed(s.tick_utilization * 100, 0)}% util`
      : 'OneSync off (no server sync tick)';
  out.push([
    `${s.players}/${s.max_players} players   ${tick}`,
    s.tick_hz > 0 ? threshold(s.tick_utilization, 0.7, 0.9) : DIM,
  ]);
}

function netLines(out, n) {
  out.push([
    `Net: ping ${fixed(n.rtt_ms, 1)}ms +/-${fixed(n.rtt_variance_ms, 1)}   loss ${fixed(
      n.loss_pct,
      2,
    )}%   mtu ${n.mtu}`,
    threshold(n.loss_pct, 1, 5),
  ]);
  out.push([
    `     in ${fixed(n.bw_in_kbps, 1)} kbit/s   out ${fixed(
      n.bw_out_kbps,
      1,
    )} kbit/s   sent ${n.packets_sent}   lost ${n.packets_lost}`,
    WHITE,
  ]);
}

function playerLines(out, p) {
  const [x, y, z] = p.position;
  out.push([
    `Player ${p.source} "${p.name}"   net ${p.net_id === undefined ? '-' : p.net_id}`,
    WHITE,
  ]);
  out.push([
    `Pos ${fixed(x, 2)}  ${fixed(y, 2)}  ${fixed(z, 2)}   hdg ${fixed(p.heading, 1)}   sector ${p.sector.join(
      ', ',
    )}`,
    WHITE,
  ]);
  const speed = Math.hypot(p.velocity[0], p.velocity[1], p.velocity[2]);
  out.push([
    `Vel ${fixed(speed, 2)} m/s   hp ${fixed(p.health, 0)}   armour ${fixed(p.armour, 0)}`,
    WHITE,
  ]);
}

function onesyncLines(out, o) {
  out.push([
    `OneSync: ${o.entities} entities   scope ${o.in_scope}   owned ${o.owned}   server-owned ${o.server_owned}`,
    WHITE,
  ]);
  // A client that stops acking keeps its last frame index while the server
  // advances; the gap is the only visible symptom.
  const lag = o.frame_index - o.client_frame_index;
  out.push([
    `         frame ${o.frame_index}   client ${o.client_frame_index}   lag ${lag}`,
    threshold(lag, 30, 120),
  ]);
  const ids = o.object_ids;
  const pressure = ids.max > 0 ? (ids.used + ids.leased) / ids.max : 0;
  out.push([
    `         object ids ${ids.used} used / ${ids.leased} leased / ${ids.free} free of ${ids.max}`,
    threshold(pressure, 0.75, 0.9),
  ]);
  out.push([
    `         bucket ${o.routing_bucket}   lockdown ${o.bucket_lockdown}   population ${
      o.bucket_population ? 'on' : 'off'
    }`,
    o.bucket_lockdown === 'inactive' ? WHITE : AMBER,
  ]);
}

function zoneSummary(z) {
  return `${z.players}/${z.max_players}p  ${z.entities}e  hb ${z.heartbeat_age_ms}ms  ${z.status}`;
}

function meshLines(out, m) {
  out.push([`Mesh: ${m.zones_online} zones online`, GREEN]);
  if (m.current) {
    const b = m.current.bounds;
    out.push([`Zone: ${m.current.id}   ${zoneSummary(m.current)}`, GREEN]);
    out.push([
      `      bounds ${fixed(b[0], 0)},${fixed(b[1], 0)} .. ${fixed(b[2], 0)},${fixed(b[3], 0)}`,
      DIM,
    ]);
    if (m.distance_to_edge !== undefined) {
      // Inside the handoff margin the server is already preparing the
      // neighbour: that transition is the whole reason this line exists.
      const arming = m.distance_to_edge <= m.handoff_margin;
      out.push([
        `      edge ${fixed(m.distance_to_edge, 1)}m   margin ${fixed(m.handoff_margin, 0)}m${
          arming ? '   HANDOFF ARMED' : ''
        }`,
        arming ? AMBER : WHITE,
      ]);
    }
  } else {
    out.push(['Zone: unrouted (no zone owns this player)', RED]);
  }
  for (const n of m.neighbours) {
    const distance = n.distance < 0 ? '?' : `${fixed(n.distance, 1)}m`;
    const bearing = n.direction ? ` ${n.direction}` : '';
    out.push([
      `      ${n.armed ? '>' : ' '} ${n.id}${bearing}  ${distance}   ${zoneSummary(n)}`,
      n.armed ? AMBER : DIM,
    ]);
  }
}

function build() {
  const out = [];
  if (notice) out.push([notice, RED]);
  if (!snapshot) {
    if (!notice) out.push(['displayinfo: waiting for the server...', DIM]);
    return out;
  }

  const age = GetGameTimer() - receivedAt;
  if (age > STALE_MS) {
    out.push([`STALE: no snapshot for ${fixed(age / 1000, 1)}s`, RED]);
  }

  serverLines(out, snapshot.server);
  netLines(out, snapshot.net);
  playerLines(out, snapshot.player);
  if (level >= LEVEL_ONESYNC) {
    if (snapshot.onesync) onesyncLines(out, snapshot.onesync);
    else out.push(['OneSync: disabled on this server', DIM]);
  }
  if (level >= LEVEL_MESH) {
    if (snapshot.mesh) meshLines(out, snapshot.mesh);
    else out.push(['Mesh: single-process server (no zone federation)', DIM]);
  }
  return out;
}

setTick(() => {
  if (notice && GetGameTimer() - noticeAt > NOTICE_MS) notice = null;
  if (level === LEVEL_OFF && !notice) return;
  const lines = build();
  let y = TOP;
  for (const [text, colour] of lines) {
    drawLine(y, text, colour);
    y += LINE_HEIGHT;
  }
});
