// axiom-core client (runs inside the FiveM client's V8, served by BASTON).

// --- BASTON native-dispatch shim -------------------------------------------
// The server sends '__baston:invokeNative' [{ id, hash, args }]; execute the
// native locally and reply with '__baston:nativeResult' [id, result].
onNet("__baston:invokeNative", (call) => {
  let result = null;
  try {
    result = Citizen.invokeNative(call.hash, ...call.args);
  } catch (e) {
    console.log("[axiom-core] native " + call.hash + " failed: " + e);
  }
  emitNet(
    "__baston:nativeResult",
    call.id,
    result === undefined ? null : result,
  );
});

// --- spawn flow --------------------------------------------------------------
const Delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

onNet("axiom:core:spawnCharacter", async (opts) => {
  const model = GetHashKey(opts.model);
  RequestModel(model);
  let attempts = 0;
  while (!HasModelLoaded(model) && attempts++ < 100) {
    await Delay(100);
  }
  if (!HasModelLoaded(model)) {
    console.log("[axiom-core] model load timed out: " + opts.model);
    return;
  }
  SetPlayerModel(PlayerId(), model);
  // A freemode ped spawns with NO clothing components → invisible body.
  // Apply the default variation so the character is actually rendered.
  SetPedDefaultComponentVariation(PlayerPedId());
  SetModelAsNoLongerNeeded(model);
  SetEntityCoords(
    PlayerPedId(),
    opts.x,
    opts.y,
    opts.z,
    false,
    false,
    false,
    false,
  );
  SetEntityHeading(PlayerPedId(), opts.heading);
  FreezeEntityPosition(PlayerPedId(), false);

  // Dismiss the loading screen. Without spawnmanager/sessionmanager, nothing
  // else calls this, so the client hangs on "Awaiting scripts…" even though
  // the ped spawned. Fade in so the player actually sees the world.
  ShutdownLoadingScreen();
  ShutdownLoadingScreenNui();
  DoScreenFadeIn(500);
  SetPlayerControl(PlayerId(), true, 0);

  TriggerEvent("axiom:core:onCharacterSpawned");
  // Let the server log the exit-criterion line.
  emitNet("axiom:core:onCharacterSpawned");
  startStateReporting(model);
});

// --- Phase D: /meshtest — guided zone-boundary crossing ----------------------
// Spawns a car, warps the player in, NPC-drives east toward the zone border
// (x = 0), pauses just before it, then crosses into zone-b. Watch the two
// zone consoles for the handoff lines while it drives.
const MESH_STOP = { x: -220.0, y: -1720.0 }; // west side, ~220m before x=0
const MESH_CROSS = { x: 450.0, y: -1720.0 }; // east side, well into zone-b
const DRIVE_MODE = 786603; // normal road driving, avoids traffic

function meshNotify(msg) {
  console.log("[meshtest] " + msg);
  BeginTextCommandThefeedPost("STRING");
  AddTextComponentSubstringPlayerName(msg);
  EndTextCommandThefeedPostTicker(true, true);
}

async function meshLoadModel(hash) {
  RequestModel(hash);
  let attempts = 0;
  while (!HasModelLoaded(hash) && attempts++ < 100) {
    await Delay(100);
  }
  return HasModelLoaded(hash);
}

async function meshWaitUntil(check, timeoutMs) {
  const deadline = GetGameTimer() + timeoutMs;
  while (GetGameTimer() < deadline) {
    if (check()) return true;
    await Delay(250);
  }
  return false;
}

let meshTestRunning = false;
RegisterCommand(
  "meshtest",
  async () => {
    if (meshTestRunning) {
      meshNotify("test déjà en cours");
      return;
    }
    meshTestRunning = true;
    try {
      const ped = PlayerPedId();
      const [px, py, pz] = GetEntityCoords(ped, false);
      meshNotify("~b~Étape 1~s~ : spawn du véhicule…");

      const model = GetHashKey("t20");
      if (!(await meshLoadModel(model))) {
        meshNotify("~r~échec du chargement du modèle t20");
        return;
      }
      const veh = CreateVehicle(
        model,
        px + 3.0,
        py,
        pz,
        GetEntityHeading(ped),
        false,
        false,
      );
      SetModelAsNoLongerNeeded(model);
      SetPedIntoVehicle(ped, veh, -1);
      await Delay(500);

      meshNotify(
        "~b~Étape 2~s~ : conduite auto vers la frontière (x=0)… reste assis.",
      );
      TaskVehicleDriveToCoord(
        ped,
        veh,
        MESH_STOP.x,
        MESH_STOP.y,
        30.0,
        25.0,
        0,
        model,
        DRIVE_MODE,
        10.0,
        0.0,
      );
      const reachedStop = await meshWaitUntil(() => {
        const [x, y] = GetEntityCoords(ped, false);
        const dx = x - MESH_STOP.x,
          dy = y - MESH_STOP.y;
        return Math.sqrt(dx * dx + dy * dy) < 18.0;
      }, 180000);
      if (!reachedStop) {
        meshNotify(
          "~r~timeout de conduite — relance /meshtest d'un endroit plus proche du centre",
        );
        return;
      }

      TaskVehicleTempAction(ped, veh, 27, 4000); // handbrake straight
      const [sx] = GetEntityCoords(ped, false);
      meshNotify(
        "~y~ARRÊT~s~ à x=" +
          sx.toFixed(0) +
          " — tu es côté OUEST (zone-a). Vérifie /admin/players ou les consoles. Traversée dans 8s…",
      );
      await Delay(8000);

      meshNotify("~b~Étape 3~s~ : franchissement de la frontière →  zone-b…");
      TaskVehicleDriveToCoord(
        ped,
        veh,
        MESH_CROSS.x,
        MESH_CROSS.y,
        30.0,
        22.0,
        0,
        model,
        DRIVE_MODE,
        10.0,
        0.0,
      );
      const crossed = await meshWaitUntil(
        () => GetEntityCoords(ped, false)[0] >= 0.0,
        120000,
      );
      if (crossed) {
        meshNotify(
          "~g~FRONTIÈRE FRANCHIE~s~ (x ≥ 0) — regarde les consoles : zone-a doit logger " +
            '"handoff complete", zone-b "arrived via zone handoff".',
        );
      } else {
        meshNotify("~r~pas de franchissement détecté (timeout)");
        return;
      }

      await meshWaitUntil(() => {
        const [x, y] = GetEntityCoords(ped, false);
        const dx = x - MESH_CROSS.x,
          dy = y - MESH_CROSS.y;
        return Math.sqrt(dx * dx + dy * dy) < 18.0;
      }, 120000);
      TaskVehicleTempAction(ped, veh, 27, 4000);
      meshNotify(
        "~g~Test terminé~s~ côté EST (zone-b). Refais /meshtest_back pour le retour, " +
          "ou roule vers l'ouest toi-même.",
      );
    } finally {
      meshTestRunning = false;
    }
  },
  false,
);

// --- /meshborder — teleport to the border + visual wall + side tracker ------
// Teleports you ~25m west of the zone boundary (x = 0), draws a red pillar
// wall along the border, and shows your current side on screen. Walk through
// it back and forth: every side flip is one server-side handoff (mind the 5s
// anti-oscillation cooldown between two handoffs of the same player).
const BORDER_SPOT = { x: -25.0, y: -1720.0, fallbackZ: 30.0 };
let meshBorderTick = null;
let meshBorderSide = null; // 'A' | 'B'

RegisterCommand(
  "meshborder",
  async () => {
    // Toggle off.
    if (meshBorderTick !== null) {
      clearTick(meshBorderTick);
      meshBorderTick = null;
      meshBorderSide = null;
      meshNotify("mur de frontière désactivé");
      return;
    }

    const ped = PlayerPedId();
    // Teleport just west of the border, facing east (GTA heading 270 = east).
    SetEntityCoords(
      ped,
      BORDER_SPOT.x,
      BORDER_SPOT.y,
      BORDER_SPOT.fallbackZ,
      false,
      false,
      false,
      false,
    );
    await Delay(300);
    const [found, groundZ] = GetGroundZFor_3dCoord(
      BORDER_SPOT.x,
      BORDER_SPOT.y,
      BORDER_SPOT.fallbackZ + 50.0,
      false,
    );
    if (found) {
      SetEntityCoords(
        ped,
        BORDER_SPOT.x,
        BORDER_SPOT.y,
        groundZ + 1.0,
        false,
        false,
        false,
        false,
      );
    }
    SetEntityHeading(ped, 270.0);
    meshBorderSide = "A";
    meshNotify(
      "~b~Frontière~s~ : le mur rouge = x=0. Traverse-le dans les deux sens " +
        "(attends ~5s entre deux passages : cooldown anti-oscillation).",
    );

    meshBorderTick = setTick(() => {
      const p = PlayerPedId();
      const [x, y, z] = GetEntityCoords(p, false);

      // Red pillar wall along x=0, ±40m around the player's y.
      for (let i = -8; i <= 8; i++) {
        DrawMarker(
          1, // vertical cylinder
          0.0,
          y + i * 5.0,
          z - 1.0,
          0.0,
          0.0,
          0.0,
          0.0,
          0.0,
          0.0,
          1.0,
          1.0,
          12.0,
          255,
          40,
          40,
          110,
          false,
          false,
          2,
          false,
          null,
          null,
          false,
        );
      }

      // Current side, top of screen.
      const side = x >= 0.0 ? "B" : "A";
      SetTextScale(0.6, 0.6);
      SetTextColour(
        side === "A" ? 80 : 255,
        side === "A" ? 180 : 160,
        side === "A" ? 255 : 60,
        255,
      );
      BeginTextCommandDisplayText("STRING");
      AddTextComponentSubstringPlayerName(
        "ZONE " + side + (side === "A" ? " (ouest)" : " (est)") + "  x=" + x.toFixed(1),
      );
      EndTextCommandDisplayText(0.42, 0.03);

      // Side flip = one boundary crossing = one handoff server-side.
      if (side !== meshBorderSide) {
        meshBorderSide = side;
        meshNotify(
          "~y~Passage en ZONE " + side + "~s~ — check les consoles zone-a/zone-b " +
            "(handoff " + (side === "B" ? "a→b" : "b→a") + ")",
        );
      }
    });
  },
  false,
);

// Return leg: drive back west across the border (tests the reverse handoff
// + the 5s anti-oscillation cooldown).
RegisterCommand(
  "meshtest_back",
  async () => {
    const ped = PlayerPedId();
    const veh = GetVehiclePedIsIn(ped, false);
    if (veh === 0) {
      meshNotify("~r~monte dans un véhicule d'abord");
      return;
    }
    meshNotify("~b~Retour~s~ : cap à l'ouest, re-franchissement de x=0…");
    TaskVehicleDriveToCoord(
      ped,
      veh,
      MESH_STOP.x,
      MESH_STOP.y,
      30.0,
      22.0,
      0,
      GetEntityModel(veh),
      DRIVE_MODE,
      10.0,
      0.0,
    );
    const crossed = await meshWaitUntil(
      () => GetEntityCoords(ped, false)[0] < 0.0,
      120000,
    );
    meshNotify(
      crossed
        ? "~g~Retour en zone-a~s~ — les consoles doivent montrer le handoff inverse."
        : "~r~pas de franchissement détecté (timeout)",
    );
  },
  false,
);

RegisterCommand(
  "ping",
  () => {
    console.log("[axiom-core] ping: " + GetGameTimer() + "ms");
  },
  false,
);

// --- Phase C: authoritative state reporting ---------------------------------
// Feed the server pipeline (anti-cheat, AoI, zone state) with this client's
// ped state at 10Hz. Entity rendering between real clients still rides the
// GTA P2P sync (msgRoute relay) — this stream is the server-side authority.
let stateReportStarted = false;
function startStateReporting(model) {
  if (stateReportStarted) return;
  stateReportStarted = true;
  setInterval(() => {
    const ped = PlayerPedId();
    const [x, y, z] = GetEntityCoords(ped, false);
    const [vx, vy, vz] = GetEntityVelocity(ped);
    emitNet("__baston:stateUpdate", {
      model: model >>> 0,
      coords: [x, y, z],
      heading: GetEntityHeading(ped),
      velocity: [vx, vy, vz],
      health: GetEntityHealth(ped),
      armour: GetPedArmour(ped),
    });
  }, 100);
}
