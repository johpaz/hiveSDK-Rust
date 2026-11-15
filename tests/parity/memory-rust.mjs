import { SessionStore } from '../../packages/npm/index.mjs';
const store = await SessionStore.open({ path: process.env.HIVE_DB_PATH, tenantId: 'first' });
const other = await store.forTenantAsync('second');
try {
  const tools = await store.services.createTools();
  const write = tools.find(tool => tool.name === 'memory_write').execute;
  const memories = store.services.collection('memory');
  await write({ title: 'Entrega', content: 'Jueves' });
  await write({ title: 'Entrega', content: 'Viernes' });
  await other.services.collection('memory').create('Entrega', { title: 'Entrega', content: 'Lunes' });
  const a = (await memories.get('Entrega')).data.content;
  const b = (await other.services.collection('memory').get('Entrega')).data.content;
  const list = (await memories.list()).items.map(row => row.data.title).sort();
  const search = (await memories.search('VIERNES')).map(hit => hit.record.data.title).sort();
  const row = await memories.get('Entrega');
  await memories.delete('Entrega', row.revision);
  const absent = await memories.get('Entrega') === null;
  await write({ title: 'Entrega', content: 'Sábado' });
  const recreated = (await memories.get('Entrega')).data.content;
  console.log('PARITY_RESULT:' + JSON.stringify({ a, b, list, search, absent, recreated }));
} finally { await other.closeAsync(); await store.closeAsync(); }
