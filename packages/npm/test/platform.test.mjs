import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { platformKey, nativePackage } from '../platform.mjs';

test('platform resolution distinguishes GNU, musl and all supported architectures', () => {
  const cases = [
    [{ platform: 'linux', arch: 'x64', report: { header: { glibcVersionRuntime: '2.39' } } }, 'linux-x64-gnu'],
    [{ platform: 'linux', arch: 'x64', report: { header: {} } }, 'linux-x64-musl'],
    [{ platform: 'linux', arch: 'arm64', report: { header: { glibcVersionRuntime: '2.39' } } }, 'linux-arm64-gnu'],
    [{ platform: 'darwin', arch: 'x64' }, 'darwin-x64'],
    [{ platform: 'darwin', arch: 'arm64' }, 'darwin-arm64'],
    [{ platform: 'win32', arch: 'x64' }, 'win32-x64-msvc'],
  ];
  for (const [runtime, key] of cases) assert.equal(platformKey(runtime), key);
  assert.equal(nativePackage({ platform: 'linux', arch: 'x64', report: { getReport: () => ({ header: {} }) } }), '@johpaz/hive-sdk-rust-linux-x64-musl');
  for (const runtime of [{ platform: 'freebsd', arch: 'x64' }, { platform: 'linux', arch: 'arm64' }, { platform: 'win32', arch: 'arm64' }]) {
    assert.throws(() => platformKey(runtime), /UNSUPPORTED_PLATFORM/);
  }
});

test('npm ships exact optional versions and excludes the development binary', () => {
  const parent = JSON.parse(readFileSync(new URL('../package.json', import.meta.url)));
  const platforms = JSON.parse(readFileSync(new URL('../platforms.json', import.meta.url)));
  assert.equal(Object.keys(parent.optionalDependencies).length, Object.keys(platforms).length);
  for (const key of Object.keys(platforms)) assert.equal(parent.optionalDependencies[`${parent.name}-${key}`], parent.version);
  assert.equal(parent.files.includes('native/'), false);
  assert.equal(parent.files.includes('platform.mjs'), true);
});
