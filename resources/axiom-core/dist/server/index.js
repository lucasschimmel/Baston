AddEventHandler('playerConnecting', function (name, setKickReason, deferrals) {
  deferrals.defer();
  console.log('[axiom-core] player connecting: ' + name);
  deferrals.update('Checking whitelist...');
  // Phase B: everyone is allowed.
  deferrals.done();
});

// Fired once the UDP game connection is established.
AddEventHandler('playerJoining', function () {
  var source = globalThis.source;
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
