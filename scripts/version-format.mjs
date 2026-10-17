const STABLE_SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

export function isStableVersion(version) {
  return typeof version === 'string' && STABLE_SEMVER.test(version);
}
