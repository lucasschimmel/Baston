// Build the zone map editor into one self-contained HTML file.
//
// The validator is compiled from `baston-zonemap-wasm` and inlined as base64
// rather than fetched: a page that fetches a sibling .wasm cannot be opened
// from `file://`, and needing a local web server to edit a config file is
// exactly the friction this tool exists to remove.
//
//   bun tools/zone-map-editor/build.mjs
//   node tools/zone-map-editor/build.mjs

import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, '..', '..');

const CRATE = 'baston-zonemap-wasm';
const PROFILE = 'wasm-release';
const TARGET = 'wasm32-unknown-unknown';

function run(cmd, args) {
  return execFileSync(cmd, args, { cwd: repo, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] });
}

console.log(`building ${CRATE} for ${TARGET}…`);
try {
  run('cargo', ['build', '-p', CRATE, '--target', TARGET, '--profile', PROFILE]);
} catch {
  console.error(
    `\ncargo failed. If the target is missing:\n  rustup target add ${TARGET}\n`
  );
  process.exit(1);
}

// Ask cargo where it put things rather than assuming ./target — this workspace
// is often built with CARGO_TARGET_DIR pointing elsewhere.
const meta = JSON.parse(run('cargo', ['metadata', '--format-version', '1', '--no-deps']));
const wasmPath = join(meta.target_directory, TARGET, PROFILE, 'baston_zonemap_wasm.wasm');

const wasm = readFileSync(wasmPath);
const template = readFileSync(join(here, 'editor.template.html'), 'utf8');

if (!template.includes('__WASM_BASE64__')) {
  console.error('the template no longer has a __WASM_BASE64__ placeholder');
  process.exit(1);
}

const out = join(here, 'zone-map-editor.html');
writeFileSync(out, template.replace('__WASM_BASE64__', wasm.toString('base64')));

const kb = n => `${(n / 1024).toFixed(0)} KB`;
console.log(`wasm ${kb(wasm.length)}  →  ${out} (${kb(readFileSync(out).length)})`);
console.log('open it directly in a browser; no server needed.');
