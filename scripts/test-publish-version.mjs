import assert from 'node:assert/strict';
import test from 'node:test';
import { isValidPackVersion, nextPatchVersion, parsePublishVersions } from '../src/lib/publish-version.js';

test('increments the patch without changing valid release components', () => {
  assert.equal(nextPatchVersion('1.2.3'), '1.2.4');
  assert.equal(nextPatchVersion('0.0.0'), '0.0.1');
  assert.equal(nextPatchVersion('2.10.999999'), '2.10.999999');
  assert.equal(nextPatchVersion('01.2.3'), '01.2.3');
});

test('accepts only canonical bounded x.y.z pack versions', () => {
  assert.equal(isValidPackVersion('1.0.0'), true);
  assert.equal(isValidPackVersion('01.0.0'), false);
  assert.equal(isValidPackVersion('1.0.0-beta'), false);
  assert.equal(isValidPackVersion('1000000.0.0'), false);
});

test('restores only valid per-series versions and tolerates corrupt storage', () => {
  assert.deepEqual(
    parsePublishVersions('{"siege":"1.4.2","ghouls-outbreak":"0.9.7","ignored":"1.0.0-beta"}'),
    { siege: '1.4.2', 'ghouls-outbreak': '0.9.7' }
  );
  assert.deepEqual(parsePublishVersions('not-json'), {});
  assert.deepEqual(parsePublishVersions('["1.0.0"]'), {});
  assert.deepEqual(parsePublishVersions(null), {});
});
