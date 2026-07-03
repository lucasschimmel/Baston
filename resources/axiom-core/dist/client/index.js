// axiom-core client (runs inside the FiveM client's V8, served by BASTON).

// --- BASTON native-dispatch shim -------------------------------------------
// The server sends '__baston:invokeNative' [{ id, hash, args }]; execute the
// native locally and reply with '__baston:nativeResult' [id, result].
onNet('__baston:invokeNative', (call) => {
  let result = null;
  try {
    result = Citizen.invokeNative(call.hash, ...call.args);
  } catch (e) {
    console.log('[axiom-core] native ' + call.hash + ' failed: ' + e);
  }
  emitNet('__baston:nativeResult', call.id, result === undefined ? null : result);
});

// --- spawn flow --------------------------------------------------------------
const Delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

onNet('axiom:core:spawnCharacter', async (opts) => {
  const model = GetHashKey(opts.model);
  RequestModel(model);
  let attempts = 0;
  while (!HasModelLoaded(model) && attempts++ < 100) {
    await Delay(100);
  }
  if (!HasModelLoaded(model)) {
    console.log('[axiom-core] model load timed out: ' + opts.model);
    return;
  }
  SetPlayerModel(PlayerId(), model);
  SetEntityCoords(PlayerPedId(), opts.x, opts.y, opts.z, false, false, false, false);
  SetEntityHeading(PlayerPedId(), opts.heading);
  FreezeEntityPosition(PlayerPedId(), false);
  TriggerEvent('axiom:core:onCharacterSpawned');
  // Let the server log the exit-criterion line.
  emitNet('axiom:core:onCharacterSpawned');
});
