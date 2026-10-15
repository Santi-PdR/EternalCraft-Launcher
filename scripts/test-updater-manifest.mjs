import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const scriptPath = path.join(repositoryRoot, 'scripts', 'create-updater-manifest.mjs');
const tempDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'eternalcraft-updater-manifest-'));
const appImage = path.join(tempDirectory, 'EternalCraft_0.1.22_x86_64.AppImage');
const windowsInstaller = path.join(tempDirectory, 'EternalCraft_0.1.22_x64-setup.exe');
const output = path.join(tempDirectory, 'latest.json');

function run(...args) {
  return spawnSync(process.execPath, [scriptPath, ...args], { encoding: 'utf8' });
}

try {
  fs.writeFileSync(appImage, 'appimage');
  fs.writeFileSync(`${appImage}.sig`, 'linux-signature\n');
  fs.writeFileSync(windowsInstaller, 'windows-installer');
  fs.writeFileSync(`${windowsInstaller}.sig`, 'windows-signature\n');

  const valid = run('0.1.22', 'Santi-PdR/EternalCraft-Launcher', appImage, windowsInstaller, output);
  assert.equal(valid.status, 0, valid.stderr);
  const manifest = JSON.parse(fs.readFileSync(output, 'utf8'));
  assert.equal(manifest.version, '0.1.22');
  assert.equal(manifest.notes, 'EternalCraft Launcher 0.1.22');
  assert.equal(Number.isNaN(Date.parse(manifest.pub_date)), false);
  assert.deepEqual(Object.keys(manifest.platforms).sort(), ['linux-x86_64', 'windows-x86_64']);
  assert.equal(manifest.platforms['linux-x86_64'].url, 'https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/launcher-v0.1.22/EternalCraft_0.1.22_x86_64.AppImage');
  assert.equal(manifest.platforms['linux-x86_64'].signature, 'linux-signature');
  assert.equal(manifest.platforms['windows-x86_64'].url, 'https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/launcher-v0.1.22/EternalCraft_0.1.22_x64-setup.exe');
  assert.equal(manifest.platforms['windows-x86_64'].signature, 'windows-signature');

  const missingSignature = path.join(tempDirectory, 'unsigned.AppImage');
  fs.writeFileSync(missingSignature, 'appimage');
  const invalid = run('0.1.22', 'Santi-PdR/EternalCraft-Launcher', missingSignature, windowsInstaller, output);
  assert.notEqual(invalid.status, 0);
  assert.match(invalid.stderr, /Missing updater signature/);

  const invalidVersion = run('0.1.22-beta', 'Santi-PdR/EternalCraft-Launcher', appImage, windowsInstaller, output);
  assert.notEqual(invalidVersion.status, 0);
  assert.match(invalidVersion.stderr, /stable x\.y\.z/);

  console.log('Updater manifest tests passed: platform URLs, signatures, stable version, and unsigned artifact rejection.');
} finally {
  fs.rmSync(tempDirectory, { recursive: true, force: true });
}
