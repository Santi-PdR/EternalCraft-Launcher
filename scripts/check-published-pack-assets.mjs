import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repository = 'Santi-PdR/EternalCraft-Launcher';
const api = 'https://api.github.com';
const pageSize = 100;
const maxPages = 10;

export async function verifyPublishedSeriesAssets(series, manifest, {
  fetchImpl = fetch,
  token = undefined,
  repo = repository,
} = {}) {
  const headers = {
    Accept: 'application/vnd.github+json',
    'X-GitHub-Api-Version': '2022-11-28',
    'User-Agent': 'EternalCraft-Launcher-release-check',
  };
  if (token) headers.Authorization = `Bearer ${token}`;

  const tag = `${series.id}-v${manifest.version}`;
  const release = await githubJson(
    fetchImpl,
    `${api}/repos/${repo}/releases/tags/${encodeURIComponent(tag)}`,
    headers,
    `No se encontró el release público ${tag}`,
  );
  if (release.tag_name !== tag || release.draft || release.prerelease) {
    throw new Error(`${series.name}: ${tag} debe ser un release publicado y estable.`);
  }
  if (!Number.isSafeInteger(release.id) || release.id <= 0) {
    throw new Error(`${series.name}: GitHub devolvió un ID de release inválido para ${tag}.`);
  }

  const assets = [];
  for (let page = 1; page <= maxPages; page += 1) {
    const response = await githubJson(
      fetchImpl,
      `${api}/repos/${repo}/releases/${release.id}/assets?per_page=${pageSize}&page=${page}`,
      headers,
      `${series.name}: no se pudo consultar la página ${page} de assets`,
    );
    if (!Array.isArray(response)) {
      throw new Error(`${series.name}: GitHub devolvió una lista de assets inválida.`);
    }
    assets.push(...response);
    if (response.length < pageSize) break;
    if (page === maxPages) {
      throw new Error(`${series.name}: el release excede el límite de ${pageSize * maxPages} assets.`);
    }
  }

  const expected = new Map();
  for (const file of manifest.files) {
    const encodedName = file.url.split('/').at(-1);
    const name = decodeURIComponent(encodedName ?? '');
    if (!name || expected.has(name)) {
      throw new Error(`${series.name}: el manifiesto tiene nombres de asset vacíos o duplicados.`);
    }
    expected.set(name, file);
  }

  const actual = new Map();
  for (const asset of assets) {
    if (typeof asset?.name !== 'string' || actual.has(asset.name)) {
      throw new Error(`${series.name}: GitHub devolvió nombres de asset inválidos o duplicados.`);
    }
    actual.set(asset.name, asset);
  }

  const errors = [];
  for (const [name, file] of expected) {
    const asset = actual.get(name);
    if (!asset) {
      errors.push(`Falta ${name}`);
      continue;
    }
    if (asset.size !== file.sizeBytes) errors.push(`${name}: tamaño publicado ${asset.size}, esperado ${file.sizeBytes}`);
    if (asset.digest !== `sha256:${file.sha256}`) errors.push(`${name}: digest SHA-256 publicado ausente o distinto`);
  }
  for (const name of actual.keys()) {
    if (!expected.has(name)) errors.push(`Asset publicado sin entrada en el manifiesto: ${name}`);
  }
  if (errors.length) {
    throw new Error(`${series.name} ${tag} no coincide con su manifiesto:\n- ${errors.join('\n- ')}`);
  }
  return { tag, assets: assets.length };
}

async function githubJson(fetchImpl, url, headers, context) {
  let response;
  try {
    response = await fetchImpl(url, { headers });
  } catch (error) {
    throw new Error(`${context}: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (!response.ok) {
    const body = await response.text().catch(() => '');
    throw new Error(`${context} (HTTP ${response.status})${body ? `: ${body.slice(0, 300)}` : ''}`);
  }
  try {
    return await response.json();
  } catch (error) {
    throw new Error(`${context}: respuesta JSON inválida (${error instanceof Error ? error.message : String(error)}).`);
  }
}

function loadJson(file) {
  return JSON.parse(fs.readFileSync(path.join(root, file), 'utf8'));
}

async function main() {
  const catalog = loadJson('resources/series/catalog.json');
  const requiredIds = loadJson('scripts/release-required-series.json');
  const token = process.env.GITHUB_TOKEN;
  const required = new Set(requiredIds);
  const seriesToVerify = catalog.series.filter((series) => required.has(series.id) || series.packStatus === 'available');
  for (const series of seriesToVerify) {
    const manifest = loadJson(`packs/${series.id}/manifest.json`);
    const result = await verifyPublishedSeriesAssets(series, manifest, { token });
    console.log(`${series.name} ${result.tag}: ${result.assets}/${manifest.files.length} assets verificados por tamaño y SHA-256.`);
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(`No se pudo verificar la publicación de los modpacks:\n${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  });
}
