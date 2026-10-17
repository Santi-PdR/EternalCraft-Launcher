import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const generator = path.join(root, 'scripts', 'create-tauri-release-config.mjs');
const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'eternalcraft-release-config-'));
const output = path.join(temp, 'tauri.release.conf.json');
const decodedKey = `untrusted comment: minisign public key: integration-test\n${Buffer.alloc(42, 7).toString('base64')}`;
const publicKey = Buffer.from(decodedKey, 'utf8').toString('base64');

function run(value) {
  return spawnSync(process.execPath, [generator, output], {
    cwd: root,
    encoding: 'utf8',
    env: { ...process.env, ...(value === undefined ? {} : { ETERNAL_TAURI_UPDATER_PUBLIC_KEY: value }) }
  });
}

try {
  const valid = run(publicKey);
  assert.equal(valid.status, 0, valid.stderr || valid.stdout);
  const config = JSON.parse(fs.readFileSync(output, 'utf8'));
  assert.deepEqual(config, {
    bundle: { createUpdaterArtifacts: true },
    plugins: { updater: { pubkey: publicKey } }
  });

  assert.notEqual(run(undefined).status, 0, 'missing public key must fail closed');
  assert.notEqual(run('not-base64!').status, 0, 'malformed public key must fail closed');
  const malformedMaterial = Buffer.from('not a Minisign key', 'utf8').toString('base64');
  assert.notEqual(run(malformedMaterial).status, 0, 'non-Minisign payload must fail closed');

  console.log('Tauri release config test passed: the configured public key is embedded and malformed/missing keys are rejected.');
} finally {
  fs.rmSync(temp, { recursive: true, force: true });
}
