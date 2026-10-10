import fs from 'node:fs';

const packageJson = JSON.parse(fs.readFileSync('package.json', 'utf8'));
const packageLock = JSON.parse(fs.readFileSync('package-lock.json', 'utf8'));
const tauriConfig = JSON.parse(fs.readFileSync('src-tauri/tauri.conf.json', 'utf8'));
const cargoManifest = fs.readFileSync('src-tauri/Cargo.toml', 'utf8');
const cargoLock = fs.readFileSync('src-tauri/Cargo.lock', 'utf8');

const cargoVersion = cargoManifest.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
const lockedCargoVersion = cargoLock.match(/\[\[package\]\]\r?\nname = "eternalcraft-launcher"\r?\nversion = "([^"]+)"/)?.[1];
const versions = {
  'package.json': packageJson.version,
  'package-lock.json': packageLock.version,
  'package-lock root package': packageLock.packages?.['']?.version,
  'Tauri config': tauriConfig.version,
  'Cargo.toml': cargoVersion,
  'Cargo.lock': lockedCargoVersion,
};

const uniqueVersions = new Set(Object.values(versions));
if (Object.values(versions).some((version) => typeof version !== 'string') || uniqueVersions.size !== 1) {
  const details = Object.entries(versions).map(([source, version]) => `${source}: ${version ?? 'missing'}`).join('\n');
  throw new Error(`Launcher version declarations do not match:\n${details}`);
}

console.log(`Launcher version is consistent across all manifests: ${packageJson.version}`);
