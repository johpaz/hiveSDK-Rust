import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, SessionStore } from '../index.mjs';

const provider = { complete: () => ({ content: 'Hi' }) };
const options = store => ({ model: 'test', system: 'Be brief', provider, store,
  agentId: 'agent', sessionId: 'session' });

test('persisted native history reopens with tools/callbacks registered by the application', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-session-'));
  let store;
  try {
    store = new SessionStore({ path: dir, tenantId: 'tenant' });
    const agent = new Agent(options(store));
    await agent.run('Hello'); agent.close(); store.close();
    store = new SessionStore({ path: dir, tenantId: 'tenant' });
    const reopened = new Agent(options(store));
    assert.equal(reopened.messages.length, 3);
    await reopened.run('Follow up');
    assert.equal(reopened.messages.length, 5);
    reopened.close();
    assert.throws(() => new Agent({ ...options(store), model: 'other' }), /SESSION_CONFIG_MISMATCH/);
  } finally { store?.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('a stale session is rejected before any model or tool is called', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-conflict-'));
  const store = new SessionStore({ path: dir });
  try {
    let calls = 0;
    const config = { ...options(store), provider: { complete() { calls++; return { content: 'Hi' }; } } };
    const first = new Agent(config); const stale = new Agent(config);
    await first.run('Hello');
    await assert.rejects(stale.run('Stale'), /SESSION_CONFLICT/);
    assert.equal(calls, 1);
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('failed execution requires explicit recovery and retains completed history', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-recover-'));
  const store = new SessionStore({ path: dir });
  try {
    const agent = new Agent(options(store)); await agent.run('Hello'); agent.close();
    const failing = new Agent({ ...options(store), provider: { complete() { throw new Error('offline'); } } });
    await assert.rejects(failing.run('Work'), /offline/);
    assert.throws(() => new Agent(options(store)), /SESSION_INTERRUPTED/);
    failing.close();
    store.recover({ agentId: 'agent', sessionId: 'session' });
    const recovered = new Agent(options(store));
    assert.equal(recovered.messages.length, 3);
    assert.equal((await recovered.run('Explicit retry')).status, 'completed');
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('tenant-scoped stores share a database while keeping history separate', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-tenants-'));
  const store = new SessionStore({ path: dir, tenantId: 'one' });
  const other = store.forTenant('two');
  try {
    await new Agent(options(store)).run('Private to one');
    assert.equal(new Agent(options(other)).messages.length, 1);
    assert.equal(new Agent(options(store)).messages.length, 3);
  } finally { other.close(); store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('active runs block store close and recovery', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-active-'));
  const store = new SessionStore({ path: dir });
  let release;
  const waiting = new Promise(resolve => { release = resolve; });
  try {
    const agent = new Agent({ ...options(store), provider: { complete: () => waiting } });
    const run = agent.run('Wait');
    assert.throws(() => store.close(), /BUSY/);
    assert.throws(() => store.recover({ agentId: 'agent', sessionId: 'session' }), /BUSY/);
    release({ content: 'Done' }); await run;
  } finally { release?.({ content: 'Done' }); store.close(); rmSync(dir, { recursive: true, force: true }); }
});
