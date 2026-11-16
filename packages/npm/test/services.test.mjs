import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, NativeTools, SessionStore } from '../index.mjs';

test('embedded catalog isolates tenants, fences stale writes and survives reopening', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-catalog-'));
  let store; let other;
  try {
    store = await SessionStore.open({ path: directory, tenantId: 'first' });
    other = await store.forTenantAsync('second');
    const memory = store.services.collection('memory');
    const created = await memory.create('same', { title: 'Preferencias', content: 'Hablar español' });
    assert.equal(created.revision, 1);
    assert.equal(await other.services.collection('memory').get('same'), null);
    const writes = await Promise.allSettled([
      memory.update('same', 1, { content: 'Uno' }), memory.update('same', 1, { content: 'Dos' }),
    ]);
    assert.equal(writes.filter(w => w.status === 'fulfilled').length, 1);
    const row = await memory.get('same');
    assert.equal(row.revision, 2);
    await assert.rejects(memory.delete('same', 1), /CATALOG_CONFLICT/);
    await assert.rejects(memory.update('same', 2, { user_id: 'second' }), /schema/);
    await other.closeAsync(); other = null;
    await store.closeAsync();
    store = await SessionStore.open({ path: directory, tenantId: 'first' });
    assert.equal((await store.services.collection('memory').get('same')).revision, 2);
    const retained = store.services;
    await store.closeAsync();
    await assert.rejects(retained.collection('memory').get('same'), /CLOSED/);
  } finally { await other?.closeAsync(); await store?.closeAsync(); rmSync(directory, { recursive: true, force: true }); }
});

test('native catalog tools run through Agent and their writes are visible to services', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-catalog-tools-'));
  const store = await SessionStore.open({ path: directory });
  let agent;
  try {
    const tools = await store.services.createTools();
    assert.equal(tools.length, 8);
    let calls = 0;
    agent = await Agent.open({ store, model: 'fixture', agentId: 'a', sessionId: 's', tools,
      provider: { complete(request) {
        calls++;
        if (calls === 1) return { content: null, tool_calls: [{ id: 'remember', name: 'memory_write', arguments: { title: 'Entrega', content: 'Viernes' } }] };
        assert.equal(JSON.parse(request.messages.at(-1).content).data.content, 'Viernes');
        return { content: 'Guardado' };
      } },
    });
    const result = await agent.run('Recuerda la entrega');
    assert.equal(result.content, 'Guardado');
    assert.equal((await store.services.collection('memory').get('Entrega')).data.content, 'Viernes');
    assert.equal(calls, 2);
    const hits = await store.services.collection('memory').search('VIERNES');
    assert.equal(hits[0].record.id, 'Entrega');
  } finally { agent?.close(); await store.closeAsync(); rmSync(directory, { recursive: true, force: true }); }
});

test('imports YAML workflow metadata and rejects missing native tool references', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-catalog-skills-'));
  const store = await SessionStore.open({ path: path.join(directory, 'db') });
  try {
    const skillPath = path.join(directory, 'SKILL.md');
    writeFileSync(skillPath, '---\nname: recordar\ndescription: Recordar datos\ntools: [memory_write]\n---\nGuarda los datos.\n');
    await assert.rejects(store.services.importSkill(skillPath), /reference not found/);
    await store.services.collection('tools').create('memory_write', { name: 'memory_write', description: 'Write', parameters: { type: 'object' } });
    const skill = await store.services.importSkill(skillPath);
    assert.equal(skill.data.content, 'Guarda los datos.\n');
    assert.equal((await store.services.collection('skills').search('datos'))[0].record.id, 'recordar');
  } finally { await store.closeAsync(); rmSync(directory, { recursive: true, force: true }); }
});

test('bundled skills are listed without enabling anything and import once their SDK tools are in the catalog', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-bundled-skills-'));
  const store = await SessionStore.open({ path: path.join(directory, 'db') });
  try {
    const listed = await store.services.bundledSkills();
    assert.deepEqual(listed.map(skill => skill.name).sort(), ['obscura_browser', 'office_document_manager', 'pdf_report_writer']);
    assert.ok(listed.every(skill => skill.description.length > 20 && skill.tools.length > 0));
    assert.equal((await store.services.collection('skills').list()).items.length, 0, 'listing imports nothing');
    await assert.rejects(store.services.importBundledSkill('nope'), /unknown bundled skill/);
    // A skill whose tools are not in the catalog is refused, like any workflow without executors.
    await assert.rejects(store.services.importBundledSkill('pdf_report_writer'), /reference not found/);
    const tools = new NativeTools({ root: directory, office: {}, obscura: { allow_evaluate: true, allow_credentials: true } });
    for (const definition of await tools.definitions()) {
      await store.services.collection('tools').create(definition.name, { name: definition.name, description: definition.description, parameters: definition.parameters });
    }
    for (const skill of listed) {
      const imported = await store.services.importBundledSkill(skill.name);
      assert.equal(imported.id, skill.name);
    }
    const browser = (await store.services.collection('skills').get('obscura_browser')).data;
    assert.match(browser.content, /Obscura/);
    assert.ok(browser.rules.some(rule => /never launch, install, suggest or fall back to Chromium/i.test(rule)));
  } finally { await store.closeAsync(); rmSync(directory, { recursive: true, force: true }); }
});
