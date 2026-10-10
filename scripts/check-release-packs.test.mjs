import test from 'node:test';
import assert from 'node:assert/strict';
import { validateReleasePacks } from './check-release-packs.mjs';

const catalog = {
  schemaVersion: 1,
  series: [
    { id: 'siege', name: 'SIEGE', minecraftVersion: '1.20.1', loader: 'Forge', loaderVersion: '47.4.10', packStatus: 'available' },
    { id: 'ghouls-outbreak', name: 'GHOULS OUTBREAK', minecraftVersion: '1.20.1', loader: 'Forge', loaderVersion: '47.4.10', packStatus: 'available' },
  ],
};

function manifest(series) {
  return {
    schemaVersion: 1,
    seriesId: series.id,
    version: '1.2.0',
    minecraftVersion: series.minecraftVersion,
    loader: series.loader,
    loaderVersion: series.loaderVersion,
    files: [{
      path: 'mods/example.jar',
      sizeBytes: 42,
      sha256: 'a'.repeat(64),
      url: `https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/${series.id}-v1.2.0/example.jar`,
    }],
  };
}

const manifests = Object.fromEntries(catalog.series.map((series) => [series.id, manifest(series)]));

test('acepta las series requeridas cuando sus manifiestos coinciden con el catálogo y los assets', () => {
  assert.deepEqual(validateReleasePacks(catalog, ['siege', 'ghouls-outbreak'], manifests), []);
});

test('rechaza la release si una serie requerida todavía no está publicada', () => {
  const unpublished = structuredClone(catalog);
  unpublished.series[1].packStatus = 'unpublished';
  assert.ok(validateReleasePacks(unpublished, ['siege', 'ghouls-outbreak'], manifests).some((error) => error.includes('GHOULS OUTBREAK todavía')));
});

test('rechaza una serie marcada disponible sin manifiesto', () => {
  assert.ok(validateReleasePacks(catalog, ['siege', 'ghouls-outbreak'], { siege: manifests.siege }).some((error) => error.includes('packs/ghouls-outbreak/manifest.json')));
});

test('rechaza manifiestos con incompatibilidad o hash inválido', () => {
  const invalid = structuredClone(manifests);
  invalid.siege.loaderVersion = '47.4.9';
  invalid['ghouls-outbreak'].files[0].sha256 = 'bad';
  const errors = validateReleasePacks(catalog, ['siege', 'ghouls-outbreak'], invalid);
  assert.ok(errors.some((error) => error.includes('cargador o versión de Forge')));
  assert.ok(errors.some((error) => error.includes('SHA-256 no válido')));
});

test('rechaza assets alojados fuera del release inmutable de esa serie', () => {
  const invalid = structuredClone(manifests);
  invalid.siege.files[0].url = 'https://example.com/mod.jar';
  assert.ok(validateReleasePacks(catalog, ['siege', 'ghouls-outbreak'], invalid).some((error) => error.includes('URL de release')));
});
