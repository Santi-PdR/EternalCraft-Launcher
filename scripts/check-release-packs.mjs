import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repository = 'Santi-PdR/EternalCraft-Launcher';

export function validateReleasePacks(catalog, requiredSeriesIds, manifests, repo = repository) {
  const errors = [];
  if (!catalog || catalog.schemaVersion !== 1 || !Array.isArray(catalog.series)) {
    return ['El catálogo debe usar schemaVersion 1 y contener una lista de series.'];
  }
  if (!Array.isArray(requiredSeriesIds) || requiredSeriesIds.length === 0) {
    return ['La release debe declarar al menos una serie jugable requerida.'];
  }

  const seriesById = new Map();
  for (const series of catalog.series) {
    if (typeof series?.id !== 'string' || !series.id) {
      errors.push('El catálogo contiene una serie sin ID válido.');
    } else if (seriesById.has(series.id)) {
      errors.push(`El catálogo repite el ID de serie ${series.id}.`);
    } else {
      seriesById.set(series.id, series);
    }
  }
  if (errors.length) return errors;

  const uniqueRequired = new Set(requiredSeriesIds);
  if (uniqueRequired.size !== requiredSeriesIds.length) {
    errors.push('La lista de series requeridas contiene IDs duplicados.');
  }

  const releaseSeries = new Map();
  for (const id of uniqueRequired) {
    const series = seriesById.get(id);
    if (!series) {
      errors.push(`La serie requerida ${id} no aparece en el catálogo.`);
      continue;
    }
    if (series.packStatus !== 'available') {
      errors.push(`${series.name ?? id} todavía no tiene un modpack publicado.`);
    }
    releaseSeries.set(id, series);
  }
  for (const series of catalog.series) {
    if (series.packStatus === 'available') releaseSeries.set(series.id, series);
  }

  for (const [id, series] of releaseSeries) {
    const manifest = manifests[id];
    if (!manifest) {
      errors.push(`${series.name ?? id}: falta packs/${id}/manifest.json.`);
      continue;
    }
    errors.push(...validateManifest(series, manifest, repo));
  }
  return errors;
}

function validateManifest(series, manifest, repo) {
  const errors = [];
  const label = series.name ?? series.id;
  if (manifest.schemaVersion !== 1) errors.push(`${label}: schemaVersion del manifiesto debe ser 1.`);
  if (manifest.seriesId !== series.id) errors.push(`${label}: seriesId del manifiesto no coincide.`);
  if (!isStableVersion(manifest.version)) errors.push(`${label}: versión del manifiesto no es MAJOR.MINOR.PATCH estable.`);
  if (manifest.minecraftVersion !== series.minecraftVersion) errors.push(`${label}: versión de Minecraft del manifiesto no coincide con el catálogo.`);
  if (manifest.loader !== series.loader || manifest.loaderVersion !== series.loaderVersion) {
    errors.push(`${label}: cargador o versión de Forge del manifiesto no coincide con el catálogo.`);
  }
  if (!Array.isArray(manifest.files) || manifest.files.length === 0) {
    errors.push(`${label}: el manifiesto debe incluir al menos un mod oficial.`);
    return errors;
  }

  const paths = new Set();
  const expectedReleasePath = `/${repo}/releases/download/${series.id}-v${manifest.version}/`;
  for (const file of manifest.files) {
    const name = typeof file?.path === 'string' && file.path.startsWith('mods/')
      ? file.path.slice('mods/'.length)
      : '';
    if (!name || name.includes('/') || !name.toLowerCase().endsWith('.jar')) {
      errors.push(`${label}: ruta de mod no válida (${file?.path ?? 'sin ruta'}).`);
    } else if (paths.has(file.path)) {
      errors.push(`${label}: ruta duplicada ${file.path}.`);
    } else {
      paths.add(file.path);
    }
    if (!Number.isSafeInteger(file?.sizeBytes) || file.sizeBytes <= 0) {
      errors.push(`${label}: tamaño no válido para ${file?.path ?? 'un mod'}.`);
    }
    if (typeof file?.sha256 !== 'string' || !/^[a-f0-9]{64}$/.test(file.sha256)) {
      errors.push(`${label}: SHA-256 no válido para ${file?.path ?? 'un mod'}.`);
    }
    try {
      const url = new URL(file.url);
      const encodedName = url.pathname.startsWith(expectedReleasePath)
        ? url.pathname.slice(expectedReleasePath.length)
        : '';
      if (url.origin !== 'https://github.com' || !encodedName || encodedName.includes('/') || decodeURIComponent(encodedName) !== name) {
        errors.push(`${label}: URL de release no corresponde a ${file?.path ?? 'un mod'}.`);
      }
    } catch {
      errors.push(`${label}: URL no válida para ${file?.path ?? 'un mod'}.`);
    }
  }
  return errors;
}

function isStableVersion(value) {
  return typeof value === 'string' && /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(value);
}

function loadJson(file) {
  return JSON.parse(fs.readFileSync(path.join(root, file), 'utf8'));
}

function run() {
  const catalog = loadJson('resources/series/catalog.json');
  const requiredIds = loadJson('scripts/release-required-series.json');
  const manifests = {};
  for (const series of catalog.series ?? []) {
    if (requiredIds.includes(series.id) || series.packStatus === 'available') {
      const manifestPath = path.join(root, 'packs', series.id, 'manifest.json');
      if (fs.existsSync(manifestPath)) manifests[series.id] = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
    }
  }
  const errors = validateReleasePacks(catalog, requiredIds, manifests);
  if (errors.length) {
    console.error(`La release jugable no está lista:\n${errors.map((error) => `- ${error}`).join('\n')}`);
    process.exitCode = 1;
    return;
  }
  console.log(`Release jugable validada: ${requiredIds.join(', ')} y todos los packs disponibles tienen manifiestos íntegros.`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) run();
