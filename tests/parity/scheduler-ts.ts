import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
const base = process.argv[2];
const { parseCronExpression, nextOccurrence } = await import(pathToFileURL(`${base}/packages/core/src/scheduler/cron/index.ts`).href);
const cases = JSON.parse(readFileSync(new URL('./calendar-cases.json', import.meta.url), 'utf8'));
const results = cases.map((value: any) => nextOccurrence(parseCronExpression(value.expression), new Date(value.from),
  { timeZone: value.timezone, domAndDow: value.dom_and_dow ?? false })?.toISOString() ?? null);
console.log('PARITY_RESULT:' + JSON.stringify(results));
