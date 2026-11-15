import { readFileSync } from 'node:fs';
import { SessionStore } from '../../packages/npm/index.mjs';
const cases = JSON.parse(readFileSync(new URL('./calendar-cases.json', import.meta.url), 'utf8'));
const store = await SessionStore.open({ path: process.env.HIVE_DB_PATH });
try {
  const queue = await store.openQueue('calendar');
  const results = [];
  for (const value of cases) {
    const next = await queue.nextOccurrence({ kind: 'cron', expression: value.expression, timezone: value.timezone,
      dom_and_dow: value.dom_and_dow ?? false }, Date.parse(value.from));
    results.push(next === null ? null : new Date(next).toISOString());
  }
  console.log('PARITY_RESULT:' + JSON.stringify(results));
} finally { await store.closeAsync(); }
