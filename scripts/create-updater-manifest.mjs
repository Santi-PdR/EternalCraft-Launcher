import fs from 'node:fs';
import path from 'node:path';

function fail(message) {
  throw new Error(message);
}

const [version, repository, appImagePath, windowsInstallerPath, outputPath] = process.argv.slice(2);
if (!version || !repository || !appImagePath || !windowsInstallerPath || !outputPath) {
  fail('Usage: node scripts/create-updater-manifest.mjs <version> <owner/repo> <AppImage> <Windows setup.exe> <output.json>');
}
if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version)) {
  fail('Updater version must be a stable SemVer x.y.z release without leading zeroes.');
}
if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository)) fail('Repository must be an owner/name pair.');

function updaterEntry(artifactPath, expectedExtension) {
  const absolutePath = path.resolve(artifactPath);
  const fileName = path.basename(absolutePath);
  const metadata = fs.statSync(absolutePath, { throwIfNoEntry: false });
  if (!metadata?.isFile() || metadata.size === 0 || !fileName.endsWith(expectedExtension)) {
    fail(`Missing, empty, or unexpected updater artifact: ${artifactPath}`);
  }
  const signaturePath = `${absolutePath}.sig`;
  const signatureMetadata = fs.statSync(signaturePath, { throwIfNoEntry: false });
  if (!signatureMetadata?.isFile() || signatureMetadata.size === 0) {
    fail(`Missing updater signature: ${signaturePath}`);
  }
  const signature = fs.readFileSync(signaturePath, 'utf8').trim();
  if (!signature) fail(`Missing updater signature: ${signaturePath}`);
  return {
    url: `https://github.com/${repository}/releases/download/launcher-v${version}/${encodeURIComponent(fileName)}`,
    signature
  };
}

const manifest = {
  version,
  notes: `EternalCraft Launcher ${version}`,
  pub_date: new Date().toISOString(),
  platforms: {
    'linux-x86_64': updaterEntry(appImagePath, '.AppImage'),
    'windows-x86_64': updaterEntry(windowsInstallerPath, '-setup.exe')
  }
};
fs.writeFileSync(path.resolve(outputPath), `${JSON.stringify(manifest, null, 2)}\n`);
