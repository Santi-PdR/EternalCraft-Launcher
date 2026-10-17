import fs from 'node:fs';
import path from 'node:path';

function fail(message) {
  throw new Error(message);
}

const outputPath = process.argv[2];
if (!outputPath) {
  fail('Usage: node scripts/create-tauri-release-config.mjs <output.json>');
}

const publicKey = process.env.ETERNAL_TAURI_UPDATER_PUBLIC_KEY?.trim();
if (!publicKey) {
  fail('Define la variable de Actions ETERNAL_TAURI_UPDATER_PUBLIC_KEY con la clave pública Minisign de Tauri.');
}
if (!/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(publicKey)) {
  fail('La clave pública Tauri debe ser Base64 válido.');
}
const decoded = Buffer.from(publicKey, 'base64').toString('utf8').trim();
const lines = decoded.split(/\r?\n/);
if (
  lines.length !== 2
  || !lines[0].startsWith('untrusted comment: minisign public key:')
  || !/^[A-Za-z0-9+/]{40,60}={0,2}$/.test(lines[1])
) {
  fail('La clave pública no tiene el formato Minisign generado por Tauri.');
}

const config = {
  bundle: { createUpdaterArtifacts: true },
  plugins: { updater: { pubkey: publicKey } }
};
const destination = path.resolve(outputPath);
fs.writeFileSync(destination, `${JSON.stringify(config, null, 2)}\n`, { mode: 0o600 });
console.log('Configuración de release: artefactos firmados y clave pública Tauri validada.');
