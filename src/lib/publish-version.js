const PACK_VERSION_PATTERN = /^(0|[1-9]\d{0,5})\.(0|[1-9]\d{0,5})\.(0|[1-9]\d{0,5})$/;

/** @param {string} version */
export function isValidPackVersion(version) {
  return PACK_VERSION_PATTERN.test(version);
}

/** @param {string} version */
export function nextPatchVersion(version) {
  const match = PACK_VERSION_PATTERN.exec(version);
  if (!match) return version;
  const patch = Number(match[3]);
  if (patch >= 999999) return version;
  return `${match[1]}.${match[2]}.${patch + 1}`;
}

/** @param {string | null} serialized */
export function parsePublishVersions(serialized) {
  if (!serialized) return {};
  try {
    /** @type {unknown} */
    const parsed = JSON.parse(serialized);
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {};
    /** @type {Record<string, string>} */
    const versions = {};
    for (const [seriesId, version] of Object.entries(parsed)) {
      if (typeof version === 'string' && isValidPackVersion(version)) {
        versions[seriesId] = version;
      }
    }
    return versions;
  } catch {
    return {};
  }
}
