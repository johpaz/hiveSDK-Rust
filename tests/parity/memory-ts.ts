// Test reference only. Bun is never used by the Rust SDK implementation.
import { pathToFileURL } from 'node:url';
const source = process.argv[2];
const { writeMemory, readMemory, listMemories, searchMemories, deleteMemory } = await import(pathToFileURL(`${source}/packages/core/src/services/memory.ts`).href);
const { getHiveDb, closeHiveDb } = await import(pathToFileURL(`${source}/packages/core/src/storage/hivedb.ts`).href);
const heartbeat = setInterval(() => {}, 20);
try {
  const db = await getHiveDb();
  await db.collection('memory').createIndex('user_id');
  await writeMemory('Entrega', 'Jueves', 'first');
  await writeMemory('Entrega', 'Viernes', 'first');
  await writeMemory('Entrega', 'Lunes', 'second');
  const a = (await readMemory('Entrega', 'first'))?.content;
  const b = (await readMemory('Entrega', 'second'))?.content;
  const list = (await listMemories('first')).map((row: any) => row.title).sort();
  const search = (await searchMemories('VIERNES', 'first')).map((row: any) => row.title).sort();
  await deleteMemory('Entrega', 'first');
  const absent = await readMemory('Entrega', 'first') === null;
  await writeMemory('Entrega', 'Sábado', 'first');
  const recreated = (await readMemory('Entrega', 'first'))?.content;
  console.log('PARITY_RESULT:' + JSON.stringify({ a, b, list, search, absent, recreated }));
} finally { closeHiveDb(); clearInterval(heartbeat); }
// The reference keeps native runtime handles after closing its DB. This fixture
// compares memory semantics, not process-shutdown behavior.
process.exit(0);
