// baston-escrow-test — server.js
// Ce fichier sert a verifier si CFX escrow chiffre les fichiers JS.
// Si ce log apparait sur BASTON : les bytes JS sont en clair (non chiffres par CFX).
// Si ce log n'apparait pas : les bytes sont chiffres et on entre dans le pipeline ScriptDecryptor.

on('onResourceStart', (resourceName) => {
  if (resourceName !== GetCurrentResourceName()) return;

  console.log('[baston-escrow-test] server.js loaded — JS file is PLAIN (not encrypted by CFX escrow)');
  console.log('[baston-escrow-test] resource: ' + GetCurrentResourceName());
});

on('onResourceStop', (resourceName) => {
  if (resourceName !== GetCurrentResourceName()) return;
  console.log('[baston-escrow-test] server.js stopped');
});
