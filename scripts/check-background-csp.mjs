import fs from 'node:fs';

const app = fs.readFileSync('src/App.svelte', 'utf8');
const tauri = JSON.parse(fs.readFileSync('src-tauri/tauri.conf.json', 'utf8'));
const block = app.match(/const backgroundPresets:[^=]+=[\\s\\S]*?\\{([\\s\\S]*?)\\n  \\};/);
if (!block) throw new Error('No se encontró el catálogo de fondos online.');

const urls = [...block[1].matchAll(/url:\\s*'([^']+)'/g)].map(([, value]) => new URL(value));
if (urls.length < 2) throw new Error(`Se esperaban al menos dos fondos online; hay ${urls.length}.`);

const imageDirective = tauri.app?.security?.csp
  ?.split(';')
  .map((directive) => directive.trim())
  .find((directive) => directive.startsWith('img-src '));
if (!imageDirective) throw new Error('La CSP no declara img-src.');

const allowedOrigins = new Set(
  imageDirective
    .slice('img-src '.length)
    .split(/\\s+/)
    .filter((source) => source.startsWith('https://'))
    .map((source) => new URL(source).origin)
);

for (const image of urls) {
  if (image.protocol !== 'https:' || image.username || image.password) {
    throw new Error(`Fondo remoto inseguro: ${image.href}`);
  }
  if (!allowedOrigins.has(image.origin)) {
    throw new Error(`La CSP bloquea el fondo ${image.href}; falta permitir ${image.origin} en img-src.`);
  }
}

console.log(`CSP permite los ${urls.length} fondos online por HTTPS.`);
