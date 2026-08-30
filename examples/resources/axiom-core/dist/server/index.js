AddEventHandler('playerConnecting', function (name, setKickReason, deferrals) {
  deferrals.defer();
  console.log('[axiom-core] player connecting: ' + name);
  deferrals.update('Checking whitelist...');
  // Phase B: everyone is allowed.
  deferrals.done();
});

// --- Phase D: zone-transferable state (mesh handoff demo) ---
// Whatever this callback returns is carried by BASTON to the next zone and
// handed back through playerJoining's second argument over there.
var meshTestCounter = 0;
AddEventHandler('playerConnecting', function () {
  meshTestCounter++;
});
RegisterZoneTransferState(function (source) {
  return {
    demoCounter: meshTestCounter,
    lastZoneLeftAt: GetGameTimer(),
    carriedFor: source,
  };
});

// Fired once the UDP game connection is established (gateway process), AND
// on zone-handoff activation (zone process — second arg = restored state).
AddEventHandler('playerJoining', function (srcArg, zoneState) {
  var source = globalThis.source !== undefined ? globalThis.source : srcArg;
  if (zoneState && typeof zoneState === 'object') {
    // Ghost activation after a handoff: the player is ALREADY in the world —
    // do NOT respawn. Just restore state.
    console.log(
      '[axiom-core] player ' + source + ' arrived via zone handoff; restored state: ' +
        JSON.stringify(zoneState['axiom-core'] || zoneState)
    );
    return;
  }
  console.log('[axiom-core] player joining: source=' + source);
  // Phase B spawn point: LSIA. Real coords come from the AXIOM bridge later.
  TriggerClientEvent('axiom:core:spawnCharacter', source, {
    model: 'mp_m_freemode_01',
    x: -1037.0,
    y: -2738.0,
    z: 20.0,
    heading: 0.0,
  });
});

AddEventHandler('axiom:core:onCharacterSpawned', function () {
  console.log('[axiom-core] onCharacterSpawned');
});

AddEventHandler('playerDropped', function (reason) {
  console.log('[axiom-core] player dropped: ' + reason);
});

AddEventHandler('onResourceStart', function (resource) {
  if (resource === GetCurrentResourceName()) {
    console.log('[axiom-core] started');
  }
});
