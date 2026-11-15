import { fileURLToPath } from 'node:url';
import { SessionStore } from '../../packages/npm/index.mjs';
const store = await SessionStore.open({ path: process.env.HIVE_DB_PATH });
try {
  const { data } = await store.services.importSkill(fileURLToPath(new URL('../fixtures/parity-skill.md', import.meta.url)));
  const { enabled, ...skill } = data;
  console.log('PARITY_RESULT:' + JSON.stringify(skill));
} finally { await store.closeAsync(); }
