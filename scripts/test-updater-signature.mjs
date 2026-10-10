import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const tempDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'eternalcraft-signature-test-'));
const privateKeyPath = path.join(tempDirectory, 'ephemeral.key');
const publicKeyPath = `${privateKeyPath}.pub`;
const artifactPath = path.join(tempDirectory, 'updater-test.bin');
const configPath = path.join(tempDirectory, 'tauri-test-config.json');
const tauriCliPath = path.join(repositoryRoot, 'node_modules', '@tauri-apps', 'cli', 'main.js');
const verifierPath = path.join(repositoryRoot, 'scripts', 'verify-updater-signature.mjs');

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: repositoryRoot,
    stdio: 'inherit',
    windowsHide: true,
    env: process.env,
    ...options
  });
  if (result.error) throw result.error;
  return result.status ?? 1;
}

try {
  fs.writeFileSync(artifactPath, 'EternalCraft signed updater verifier integration test\n');
  const generateStatus = run(process.execPath, [
    tauriCliPath, 'signer', 'generate', '--ci', '--password', '', '-w', privateKeyPath
  ]);
  if (generateStatus !== 0) throw new Error('Could not generate an ephemeral Minisign keypair');

  const signStatus = run(process.execPath, [
    tauriCliPath, 'signer', 'sign', '--private-key-path', privateKeyPath, artifactPath
  ]);
  if (signStatus !== 0) throw new Error('Tauri could not sign the temporary updater artifact');

  const publicKey = fs.readFileSync(publicKeyPath, 'utf8');
  fs.writeFileSync(configPath, JSON.stringify({
    plugins: { updater: { pubkey: Buffer.from(publicKey, 'utf8').toString('base64') } }
  }));
  const verifierEnv = {
    ...process.env,
    ETERNALCRAFT_UPDATER_CONFIG_PATH: configPath,
    ETERNALCRAFT_UPDATER_CARGO_PROFILE: 'debug'
  };

  const validStatus = run(process.execPath, [verifierPath, artifactPath], { env: verifierEnv });
  if (validStatus !== 0) throw new Error('The verifier rejected a valid Tauri signature');

  fs.appendFileSync(artifactPath, 'tampered');
  const tamperedStatus = run(process.execPath, [verifierPath, artifactPath], { env: verifierEnv });
  if (tamperedStatus === 0) throw new Error('The verifier accepted a tampered updater artifact');

  console.log('Updater signing integration passed: valid signature accepted; tampered artifact rejected.');
} finally {
  fs.rmSync(tempDirectory, { recursive: true, force: true });
}
