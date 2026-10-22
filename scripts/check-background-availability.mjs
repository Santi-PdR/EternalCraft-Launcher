import fs from 'node:fs';

const app = fs.readFileSync('src/App.svelte', 'utf8');
const block = app.match(/const backgroundPresets:[^=]+=[\s\S]*?\{([\s\S]*?)\n  \};/);
if (!block) throw new Error('No se encontró el catálogo de fondos online.');

const urls = [...block[1].matchAll(/url:\s*'([^']+)'/g)].map(([, value]) => new URL(value));
if (urls.length < 2) throw new Error('Se esperaban al menos dos fondos online.');

for (const url of urls) {
  let lastError;
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    try {
      const response = await fetch(url, {
        method: 'HEAD',
        signal: AbortSignal.timeout(15000),
        headers: { 'User-Agent': 'EternalCraft-Launcher-background-check' }
      });
      const contentType = response.headers.get('content-type') ?? '';
      if (!response.ok || !contentType.toLowerCase().startsWith('image/')) {
        throw new Error(`HTTP ${response.status}; tipo ${contentType || 'desconocido'}`);
      }
      console.log(`Fondo disponible: ${url.hostname} (${contentType.split(';')[0]})`);
      lastError = undefined;
      break;
    } catch (error) {
      lastError = error;
      if (attempt < 3) await new Promise((resolve) => setTimeout(resolve, attempt * 1000));
    }
  }
  if (lastError) {
    throw new Error(`El fondo remoto no entrega una imagen disponible (${url.href}): ${lastError.message}`);
  }
}
