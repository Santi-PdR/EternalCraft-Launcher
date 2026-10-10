import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const artifactPath = process.argv[2] ? path.resolve(process.argv[2]) : '';
if (!artifactPath || !fs.statSync(artifactPath, { throwIfNoEntry: false })?.isFile()) {
  console.error('Usage: node scripts/verify-updater-signature.mjs <signed-artifact>');
  process.exit(2);
}

const signaturePath = `${artifactPath}.sig`;
if (!fs.statSync(signaturePath, { throwIfNoEntry: false })?.isFile()) {
  throw new Error(`Missing updater signature: ${signaturePath}`);
}

const configPath = process.env.ETERNALCRAFT_UPDATER_CONFIG_PATH
  ? path.resolve(process.env.ETERNALCRAFT_UPDATER_CONFIG_PATH)
  : path.join(repositoryRoot, 'src-tauri', 'tauri.conf.json');
const config = JSON.parse(fs.readFileSync(configPath, 'utf8'));
const encodedPublicKey = config.plugins?.updater?.pubkey;
if (typeof encodedPublicKey !== 'string' || !encodedPublicKey) {
  throw new Error('Tauri updater public key is missing from tauri.conf.json');
}

const publicKeyContents = Buffer.from(encodedPublicKey, 'base64').toString('utf8');
if (!publicKeyContents.startsWith('untrusted comment: minisign public key:')) {
  throw new Error('Tauri updater public key has an invalid Minisign format');
}

const tempDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'eternalcraft-updater-verify-'));
const publicKeyPath = path.join(tempDirectory, 'updater.pub');
try {
  fs.writeFileSync(publicKeyPath, publicKeyContents, { mode: 0o600 });
  const cargoProfile = process.env.ETERNALCRAFT_UPDATER_CARGO_PROFILE;
  if (cargoProfile && cargoProfile !== 'debug' && cargoProfile !== 'release') {
    throw new Error('ETERNALCRAFT_UPDATER_CARGO_PROFILE must be debug or release');
  }
  const result = spawnSync(process.platform === 'win32' ? 'cargo.exe' : 'cargo', [
    'run', ...(cargoProfile === 'debug' ? [] : ['--release']), '--locked', '--manifest-path', path.join(repositoryRoot, 'src-tauri', 'Cargo.toml'),
    '--bin', 'verify-updater-signature', '--', publicKeyPath, signaturePath, artifactPath
  ], { cwd: repositoryRoot, stdio: 'inherit', windowsHide: true });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exitCode = result.status ?? 1;
} finally {
  fs.rmSync(tempDirectory, { recursive: true, force: true });
}
