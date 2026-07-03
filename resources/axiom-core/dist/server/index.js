AddEventHandler('playerConnecting', function (name, setKickReason, deferrals) {
  deferrals.defer();
  console.log('[axiom-core] player connecting: ' + name);
  deferrals.update('Checking whitelist...');
  // Phase A: everyone is allowed.
  deferrals.done();
});

AddEventHandler('playerDropped', function (reason) {
  console.log('[axiom-core] player dropped: ' + reason);
});

AddEventHandler('onResourceStart', function (resource) {
  if (resource === GetCurrentResourceName()) {
    console.log('[axiom-core] started');
  }
});
