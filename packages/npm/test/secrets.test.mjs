import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { SecretVault, SessionStore } from '../index.mjs';
async function fixture(work) {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-secrets-'));
  const store = await SessionStore.open({ path: dir, tenantId: 'one' });
  try { await work(store, dir); } finally { await store.closeAsync(); rmSync(dir, { recursive: true, force: true }); }
}
test('vaults expose only metadata by default, use CAS and retain store ownership', async () => fixture(async store => {
  const key = Buffer.alloc(32, 7);
  const vault = await SecretVault.open({ store, id: 'main', key });
  try {
    const value = 'private fixture token 🦀';
    const meta = await vault.put('api', value);
    assert.equal(meta.revision, 1); assert.equal(key[0], 7);
    assert.equal(await vault.reveal('api'), value);
    assert.equal(JSON.stringify(vault).includes(value), false);
    assert.equal(JSON.stringify(await vault.list()).includes(value), false);
    await assert.rejects(vault.put('api', 'stale'), /SECRET_CONFLICT/);
    await assert.rejects(store.closeAsync(), /BUSY/);
    const pending = vault.put('second', 'other');
    await assert.rejects(vault.close(), /BUSY/); await pending;
    const deleted = await vault.delete('api', meta.revision);
    assert.equal(await vault.reveal('api'), null); assert.equal(deleted.deleted, true);
    assert.equal((await vault.inspect('api')).revision, 2);
    await vault.put('api', 'new', { revision: 2 });
    assert.equal(await vault.reveal('api'), 'new');
  } finally { await vault.close(); }
  await vault.close(); await assert.rejects(vault.info(), /CLOSED/);
}));
test('keys and tenants isolate values and rotation invalidates old handles', async () => fixture(async store => {
  const first = await store.openSecrets({ key: Buffer.alloc(32, 7) });
  const stale = await store.openSecrets({ key: Buffer.alloc(32, 7) });
  const otherStore = await store.forTenantAsync('two');
  const other = await otherStore.openSecrets({ key: Buffer.alloc(32, 7) });
  try {
    await first.put('api', 'one'); await other.put('api', 'two');
    const page = await first.list({ limit: 1 }); assert.equal(page.items.length, 1);
    await assert.rejects(store.openSecrets({ key: Buffer.alloc(32, 8) }), /AUTHENTICATION/);
    await assert.rejects(first.rekey({ keyId: 'same', key: Buffer.alloc(32, 7) }), /different key material/);
    const receipt = await first.rekey({ keyId: 'second', key: Buffer.alloc(32, 8) });
    assert.equal(receipt.key_id, 'second'); assert.equal(receipt.encryptions, 2);
    await assert.rejects(stale.reveal('api'), /AUTHENTICATION/);
    assert.equal(await first.reveal('api'), 'one'); assert.equal(await other.reveal('api'), 'two');
    const reopened = await store.openSecrets({ keyId: 'second', key: Buffer.alloc(32, 8) });
    try { assert.equal(await reopened.reveal('api'), 'one'); } finally { await reopened.close(); }
  } finally { await first.close(); await stale.close(); await other.close(); await otherStore.closeAsync(); }
}));
test('catalog references do not contain values and no secret executor is installed', async () => fixture(async store => {
  await assert.rejects(store.openSecrets({ key: 'text is not a byte key' }), /32 bytes/);
  const vault = await store.openSecrets({ key: Buffer.alloc(32, 7) });
  try {
    const token = 'fixture-value-not-in-catalog'; await vault.put('provider-key', token);
    const ref = { vault_id: 'default', name: 'provider-key' };
    const record = await store.services.collection('providers').create('fixture', {
      name: 'Fixture', protocol: 'openai_compatible', base_url: 'http://127.0.0.1:9', credential_ref: ref,
    });
    assert.deepEqual(record.data.credential_ref, ref);
    assert.equal(JSON.stringify(record).includes(token), false);
    const tools = await store.services.createTools();
    assert.equal(tools.some(tool => tool.name.includes('secret')), false);
    await assert.rejects(store.services.collection('providers').update('fixture', record.revision, { api_key: token }), /inline credentials/);
  } finally { await vault.close(); }
}));
