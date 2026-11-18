// Package an already-built binding. Building and validating each target happens in CI.
import { readFileSync, writeFileSync, copyFileSync, mkdirSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { parseArgs } from 'node:util';

const root = fileURLToPath(new URL('../', import.meta.url));
const { values } = parseArgs({ options: {
  target: { type: 'string' }, binary: { type: 'string' }, output: { type: 'string', default: 'dist/npm' },
  profile: { type: 'string', default: 'release' },
} });
const platforms = JSON.parse(readFileSync(path.join(root, 'packages/npm/platforms.json')));
const pair = Object.entries(platforms).find(([key, config]) => key === values.target || config.target === values.target);
if (!pair) throw new Error('Select a supported --target from packages/npm/platforms.json');
if (!['debug', 'release'].includes(values.profile)) throw new Error('profile must be debug or release');
const [key, config] = pair;
const parent = JSON.parse(readFileSync(path.join(root, 'packages/npm/package.json')));
const inventoryBytes = readFileSync(path.join(root, 'third_party/inventory.json'));
const inventory = JSON.parse(inventoryBytes);
const notices = readFileSync(path.join(root, 'THIRD_PARTY_NOTICES.txt'));
const digest = data => createHash('sha256').update(data).digest('hex');
if (inventory.cargo_lock_sha256 !== digest(readFileSync(path.join(root, 'Cargo.lock')))
  || inventory.notices_sha256 !== digest(notices)) throw new Error('THIRD_PARTY_NOTICES_STALE: regenerate notices from Cargo.lock');
if (!parent.private && inventory.pending.length) throw new Error('THIRD_PARTY_REVIEW_REQUIRED: resolve inventory gaps before creating public packages');
const source = path.resolve(root, values.binary ?? `target/${config.target}/${values.profile}/${config.library}`);
const bytes = readFileSync(source);
// Reject architecture/container mismatches rather than labelling a host binary as another target.
if (config.os === 'linux') {
  if (bytes.subarray(0, 4).toString('hex') !== '7f454c46' || bytes[4] !== 2 || bytes[5] !== 1
    || bytes.readUInt16LE(18) !== (config.cpu === 'x64' ? 62 : 183)) throw new Error('ELF target mismatch');
} else if (config.os === 'darwin') {
  if (bytes.readUInt32LE(0) !== 0xfeedfacf || bytes.readUInt32LE(4) !== (config.cpu === 'x64' ? 0x01000007 : 0x0100000c)) throw new Error('Mach-O target mismatch');
} else {
  const pe = bytes.readUInt32LE(0x3c);
  if (bytes.subarray(0, 2).toString() !== 'MZ' || bytes.readUInt32LE(pe) !== 0x4550 || bytes.readUInt16LE(pe + 4) !== 0x8664) throw new Error('PE target mismatch');
}
const destination = path.resolve(root, values.output, key);
mkdirSync(destination, { recursive: true });
// npm verifies the provenance of a published package against its `repository.url`: without it the registry
// answers 422 ("repository.url is \"\", expected to match ... from provenance"). The native packages inherit it.
if (!parent.repository?.url) throw new Error('REPOSITORY_REQUIRED: packages/npm/package.json needs repository.url (npm provenance)');
const manifest = {
  name: `${parent.name}-${key}`, version: parent.version, private: parent.private ?? false,
  description: `Native Hive SDK binding for ${key}`, license: parent.license,
  repository: { type: 'git', url: parent.repository.url },
  main: 'hive.node', files: ['hive.node', 'README.md', 'LICENSE', 'THIRD_PARTY_NOTICES.txt', 'THIRD_PARTY_INVENTORY.json'],
  os: [config.os], cpu: [config.cpu], ...(config.libc ? { libc: [config.libc] } : {}),
  engines: parent.engines,
  hiveBuild: { target: config.target, profile: values.profile,
    sha256: createHash('sha256').update(bytes).digest('hex'), napi: 8,
    notices: { status: inventory.status, pending: inventory.pending.length,
      cargoLockSha256: inventory.cargo_lock_sha256, sha256: inventory.notices_sha256 } },
};
copyFileSync(source, path.join(destination, 'hive.node'));
copyFileSync(path.join(root, 'LICENSE'), path.join(destination, 'LICENSE'));
writeFileSync(path.join(destination, 'THIRD_PARTY_NOTICES.txt'), notices);
writeFileSync(path.join(destination, 'THIRD_PARTY_INVENTORY.json'), inventoryBytes);
writeFileSync(path.join(destination, 'package.json'), `${JSON.stringify(manifest, null, 2)}\n`);
writeFileSync(path.join(destination, 'README.md'), `# ${manifest.name}\n\nPlatform binding for ${parent.name}@${parent.version}. Install the main package.\n`);
console.log(destination);
