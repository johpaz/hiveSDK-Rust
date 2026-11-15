import { spawnSync } from 'node:child_process';
import { mkdtempSync, existsSync, copyFileSync, writeFileSync, rmSync } from 'node:fs';
import path from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const [main, platform] = process.argv.slice(2).map(file => path.resolve(file));
if (!main || !platform) throw new Error('Pass main and platform tarballs');
const nodeDir = path.dirname(process.execPath);
const npmCli = [path.join(nodeDir, 'node_modules/npm/bin/npm-cli.js'),
  path.join(nodeDir, '../lib/node_modules/npm/bin/npm-cli.js')].find(existsSync);
if (!npmCli) throw new Error('Cannot locate npm next to the current Node installation');
const directory = mkdtempSync(path.join(tmpdir(), 'hive-npm-consumer-'));
const env = { ...process.env, PATH: nodeDir, npm_config_cache: path.join(directory, 'cache') };
delete env.NODE_PATH;
function run(args) {
  const result = spawnSync(process.execPath, args, { cwd: directory, env, stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`Installed consumer failed with status ${result.status}`);
}
try {
  writeFileSync(path.join(directory, 'package.json'), JSON.stringify({ private: true, type: 'module' }));
  // Supply the unpublished native tarball explicitly. Registry selection is tested after publication.
  run([npmCli, 'install', '--offline', '--ignore-scripts', '--omit=optional', '--no-audit', '--no-fund', main, platform]);
  if (existsSync(path.join(directory, 'node_modules/@johpaz/hive-sdk-rust/native'))) {
    throw new Error('Consumer accidentally contains the source-development binding');
  }
  copyFileSync(path.join(root, 'tests/installed-smoke.mjs'), path.join(directory, 'smoke.mjs'));
  run(['smoke.mjs']);
} finally { rmSync(directory, { recursive: true, force: true }); }
