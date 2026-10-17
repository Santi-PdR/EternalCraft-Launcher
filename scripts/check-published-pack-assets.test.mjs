import test from 'node:test';
import assert from 'node:assert/strict';
import { verifyPublishedSeriesAssets } from './check-published-pack-assets.mjs';

const series = { id: 'siege', name: 'SIEGE' };
const manifest = {
  version: '1.0.0',
  files: [{
    path: 'mods/example+mod.jar',
    url: 'https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/siege-v1.0.0/example%2Bmod.jar',
    sizeBytes: 42,
    sha256: 'a'.repeat(64),
  }],
};

function mockFetch(release, pages) {
  const calls = [];
  const fetchImpl = async (url) => {
    calls.push(url);
    if (url.endsWith('/releases/tags/siege-v1.0.0')) {
      return Response.json(release);
    }
    const page = Number(new URL(url).searchParams.get('page'));
    const assets = pages[page - 1];
    return assets === undefined ? new Response('not found', { status: 404 }) : Response.json(assets);
  };
  return { fetchImpl, calls };
}

const expectedAsset = {
  name: 'example+mod.jar',
  size: 42,
  digest: `sha256:${'a'.repeat(64)}`,
};

test('verifica release público con asset, tamaño y digest exactos', async () => {
  const { fetchImpl, calls } = mockFetch(
    { id: 123, tag_name: 'siege-v1.0.0', draft: false, prerelease: false },
    [[expectedAsset]],
  );
  assert.deepEqual(await verifyPublishedSeriesAssets(series, manifest, { fetchImpl }), {
    tag: 'siege-v1.0.0',
    assets: 1,
  });
  assert.equal(calls.length, 2);
});

test('sigue la paginación hasta encontrar todos los assets del pack', async () => {
  const firstPage = Array.from({ length: 100 }, (_, i) => ({
    name: `old-${i}.jar`,
    size: 1,
    digest: `sha256:${'b'.repeat(64)}`,
  }));
  const paginatedManifest = {
    ...manifest,
    files: [
      ...firstPage.map((asset) => ({
        path: `mods/${asset.name}`,
        url: `https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/siege-v1.0.0/${asset.name}`,
        sizeBytes: asset.size,
        sha256: 'b'.repeat(64),
      })),
      ...manifest.files,
    ],
  };
  const { fetchImpl, calls } = mockFetch(
    { id: 123, tag_name: 'siege-v1.0.0', draft: false, prerelease: false },
    [firstPage, [expectedAsset]],
  );
  assert.deepEqual(await verifyPublishedSeriesAssets(series, paginatedManifest, { fetchImpl }), {
    tag: 'siege-v1.0.0',
    assets: 101,
  });
  assert.equal(calls.length, 3);
  assert.match(calls[2], /page=2$/);
});

test('rechaza assets ausentes, sobrantes, con tamaño o digest distinto', async () => {
  const cases = [
    { assets: [], message: /Falta example\+mod\.jar/ },
    { assets: [{ ...expectedAsset, size: 43 }], message: /tamaño publicado 43/ },
    { assets: [{ ...expectedAsset, digest: null }], message: /digest SHA-256 publicado ausente o distinto/ },
    { assets: [expectedAsset, { name: 'stale.jar', size: 1, digest: `sha256:${'b'.repeat(64)}` }], message: /sin entrada en el manifiesto/ },
  ];
  for (const { assets, message } of cases) {
    const { fetchImpl } = mockFetch(
      { id: 123, tag_name: 'siege-v1.0.0', draft: false, prerelease: false },
      [assets],
    );
    await assert.rejects(verifyPublishedSeriesAssets(series, manifest, { fetchImpl }), message);
  }
});

test('rechaza un release draft o un tag distinto', async () => {
  for (const release of [
    { id: 123, tag_name: 'siege-v1.0.0', draft: true, prerelease: false },
    { id: 123, tag_name: 'other-v1.0.0', draft: false, prerelease: false },
  ]) {
    const { fetchImpl } = mockFetch(release, []);
    await assert.rejects(verifyPublishedSeriesAssets(series, manifest, { fetchImpl }), /release publicado y estable/);
  }
});
