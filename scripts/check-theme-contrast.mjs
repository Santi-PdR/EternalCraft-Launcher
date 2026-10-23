import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('../src/App.svelte', import.meta.url), 'utf8');
const block = source.match(/const themes = \[([\s\S]*?)\n  \];/);
if (!block) throw new Error('No se encontró el catálogo de temas.');

const themes = [...block[1].matchAll(/\{\s*id:\s*'([^']+)'([\s\S]*?)\}/g)].map(([, id, fields]) => {
  const value = (key) => fields.match(new RegExp(`${key}: '([^']+)'`))?.[1];
  const theme = { id, kind: value('kind') };
  for (const key of ['bg', 'sidebar', 'surface', 'raised', 'muted', 'text', 'accent']) {
    theme[key] = value(key);
    if (!/^#[0-9a-fA-F]{6}$/.test(theme[key] ?? '')) throw new Error(`Tema ${id} tiene un color ${key} inválido.`);
  }
  if (!['Claro', 'Oscuro'].includes(theme.kind)) throw new Error(`Tema ${id} tiene un modo inválido.`);
  return theme;
});

if (themes.length < 15) throw new Error(`Se esperaban al menos 15 temas; hay ${themes.length}.`);
for (let index = 0; index < themes.length; index += 1) {
  const theme = themes[index];
  const expectedKind = index % 2 === 0 ? 'Claro' : 'Oscuro';
  if (theme.kind !== expectedKind) {
    throw new Error(`Los temas deben alternar claro/oscuro: ${theme.id} debería ser ${expectedKind}.`);
  }
}

function luminance(hex) {
  const channels = hex.slice(1).match(/../g).map((channel) => parseInt(channel, 16) / 255);
  const linear = channels.map((channel) => channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4);
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}

function contrast(a, b) {
  const first = luminance(a);
  const second = luminance(b);
  return (Math.max(first, second) + 0.05) / (Math.min(first, second) + 0.05);
}

for (const theme of themes) {
  const surfaces = [theme.bg, theme.sidebar, theme.surface, theme.raised];
  for (const textKey of ['text', 'muted', 'accent']) {
    const minimum = Math.min(...surfaces.map((surface) => contrast(theme[textKey], surface)));
    if (minimum < 4.5) {
      throw new Error(`Contraste insuficiente en ${theme.id}: ${textKey} alcanza ${minimum.toFixed(2)}:1.`);
    }
  }
  const buttonText = theme.kind === 'Claro' ? '#ffffff' : '#171a17';
  if (contrast(theme.accent, buttonText) < 4.5) {
    throw new Error(`Texto de botón primario con contraste insuficiente en ${theme.id}.`);
  }
}

console.log(`Contraste WCAG AA comprobado para ${themes.length} temas.`);
