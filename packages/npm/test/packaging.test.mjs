import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, copyFileSync, rmSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import path from 'node:path';
const hash = data => createHash('sha256').update(data).digest('hex');
function fixture(callback) {
  const root = mkdtempSync(path.join(tmpdir(), 'hive-package-notices-'));
  for (const dir of ['scripts', 'packages/npm', 'third_party']) mkdirSync(path.join(root, dir), { recursive: true });
  copyFileSync(new URL('../../../scripts/package-native.mjs', import.meta.url), path.join(root, 'scripts/package-native.mjs'));
  copyFileSync(new URL('../platforms.json', import.meta.url), path.join(root, 'packages/npm/platforms.json'));
  const parent = { name: '@fixture/sdk', version: '0.1.0', private: true, license: 'MIT', engines: { node: '>=24' } };
  writeFileSync(path.join(root, 'packages/npm/package.json'), JSON.stringify(parent));
  writeFileSync(path.join(root, 'LICENSE'), 'SDK fixture'); writeFileSync(path.join(root, 'Cargo.lock'), 'locked fixture');
  writeFileSync(path.join(root, 'THIRD_PARTY_NOTICES.txt'), 'third-party fixture');
  writeFileSync(path.join(root, 'third_party/inventory.json'), JSON.stringify({ status: 'incomplete', pending: [{ name: 'demo' }],
    cargo_lock_sha256: hash('locked fixture'), notices_sha256: hash('third-party fixture') }));
  const elf = Buffer.alloc(64); elf.write('7f454c46', 0, 'hex'); elf[4] = 2; elf[5] = 1; elf.writeUInt16LE(62, 18);
  writeFileSync(path.join(root, 'fixture.node'), elf);
  const run = () => spawnSync(process.execPath, [path.join(root, 'scripts/package-native.mjs'), '--target', 'linux-x64-gnu', '--binary', 'fixture.node', '--profile', 'debug'], { cwd: root, encoding: 'utf8' });
  try { callback({ root, parent, run, output: path.join(root, 'dist/npm/linux-x64-gnu') }); }
  finally { rmSync(root, { recursive: true, force: true }); }
}
test('preview native packages carry notices, provenance and unresolved status', () => fixture(({ root, run, output }) => {
  const result = run(); assert.equal(result.status, 0, result.stderr);
  const manifest = JSON.parse(readFileSync(path.join(output, 'package.json')));
  assert.equal(manifest.private, true); assert.equal(manifest.hiveBuild.profile, 'debug');
  assert.equal(manifest.hiveBuild.notices.pending, 1); assert.equal(manifest.hiveBuild.notices.status, 'incomplete');
  assert.equal(manifest.hiveBuild.notices.sha256, hash(readFileSync(path.join(output, 'THIRD_PARTY_NOTICES.txt'))));
  assert.deepEqual(readFileSync(path.join(output, 'THIRD_PARTY_INVENTORY.json')), readFileSync(path.join(root, 'third_party/inventory.json')));
}));
test('public native packages cannot be generated while notice review is incomplete', () => fixture(({ root, parent, run }) => {
  writeFileSync(path.join(root, 'packages/npm/package.json'), JSON.stringify({ ...parent, private: false }));
  const result = run(); assert.notEqual(result.status, 0); assert.match(result.stderr, /THIRD_PARTY_REVIEW_REQUIRED/);
}));
test('native packaging rejects modified notices and Cargo.lock drift', () => fixture(({ root, run }) => {
  writeFileSync(path.join(root, 'THIRD_PARTY_NOTICES.txt'), 'modified');
  let result = run(); assert.notEqual(result.status, 0); assert.match(result.stderr, /THIRD_PARTY_NOTICES_STALE/);
  writeFileSync(path.join(root, 'THIRD_PARTY_NOTICES.txt'), 'third-party fixture'); writeFileSync(path.join(root, 'Cargo.lock'), 'changed');
  result = run(); assert.notEqual(result.status, 0); assert.match(result.stderr, /THIRD_PARTY_NOTICES_STALE/);
}));
