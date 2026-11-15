import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('../../../', import.meta.url));
const env = { ...process.env, CARGO_TARGET_DIR: path.join(root, 'target') };
execFileSync('cargo', ['build', '--release', '-p', 'hive-agent-napi', '--manifest-path', path.join(root, 'Cargo.toml')], { stdio: 'inherit', env });
const file = process.platform === 'win32' ? 'hive_agent_napi.dll'
  : process.platform === 'darwin' ? 'libhive_agent_napi.dylib' : 'libhive_agent_napi.so';
const dest = fileURLToPath(new URL('../native/', import.meta.url));
mkdirSync(dest, { recursive: true });
copyFileSync(path.join(root, 'target/release', file), path.join(dest, 'hive.node'));
