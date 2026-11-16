import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, SessionStore } from '../index.mjs';

const PNG = 'iVBORw0KGgo=';
const image = { kind: 'image', media_type: 'image/png', data: PNG };

test('attachments reach the provider and text-only turns are unchanged', async () => {
  const seen = [];
  const provider = { complete(request) { seen.push(request.messages.at(-1)); return { content: 'done' }; } };
  const agent = new Agent({ model: 'test', provider });
  await agent.run({ text: 'Look', attachments: [image, { kind: 'image', media_type: 'image/jpeg', url: 'https://example.com/a.jpg' }] });
  await agent.run('Plain');
  agent.close();
  assert.equal(seen[0].content, 'Look');
  assert.deepEqual(seen[0].attachments, [image, { kind: 'image', media_type: 'image/jpeg', url: 'https://example.com/a.jpg' }]);
  assert.equal('attachments' in seen[1], false);
});

test('invalid attachments fail before the provider is called', async () => {
  let calls = 0;
  const agent = new Agent({ model: 'test', provider: { complete() { calls++; return { content: 'x' }; } } });
  for (const attachments of [
    [{ ...image, data: 'not base64!' }],
    [{ ...image, media_type: 'audio/wav' }],
    [{ kind: 'image', media_type: 'image/png', data: PNG, url: 'https://example.com/a.png' }],
    [{ kind: 'video', media_type: 'video/mp4', data: PNG }],
  ]) await assert.rejects(agent.run({ text: 'x', attachments }));
  await assert.rejects(agent.run({ text: 'x', attachments: Array(9).fill(image) }), /at most 8/);
  agent.close();
  assert.equal(calls, 0);
});

test('token budgets need an attachment allowance and ignore payload size', async () => {
  const estimates = [];
  const provider = { complete(request) { estimates.push(request.context.token_estimate); return { content: 'done' }; } };
  const policy = { input_token_budget: { encoding: 'o200k_base', max_tokens: 100000, framing: 'openai_chat_json' } };
  const refused = new Agent({ model: 'test', provider, policy });
  await assert.rejects(refused.run({ text: 'Look', attachments: [image] }), /attachment_tokens/);
  refused.close();
  const allowed = new Agent({ model: 'test', provider, policy: { input_token_budget: { ...policy.input_token_budget, attachment_tokens: 1200 } } });
  await allowed.run({ text: 'Look', attachments: [image] });
  await allowed.run({ text: 'Look', attachments: [{ ...image, data: 'A'.repeat(2 * 1024 * 1024) }] });
  allowed.close();
  assert.equal(estimates[0].overhead_tokens, 1200);
  assert.equal(estimates[1].overhead_tokens, 2400, 'history keeps the first attachment');
});

test('durable sessions persist attachments across reopen', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-media-')); let store;
  const users = [];
  const provider = { complete(request) { users.push(request.messages.filter(m => m.role === 'user')); return { content: 'done' }; } };
  const config = { model: 'test', provider, agentId: 'a', sessionId: 's' };
  try {
    store = new SessionStore({ path: directory });
    const first = new Agent({ ...config, store });
    await first.run({ text: 'Look', attachments: [image] });
    first.close(); store.close(); store = new SessionStore({ path: directory });
    const again = new Agent({ ...config, store });
    await again.run('Next');
    again.close();
    assert.deepEqual(users[0][0].attachments, [image]);
    assert.equal(users[1].length, 2);
    assert.deepEqual(users[1][0].attachments, [image], 'earlier media is still in the history');
    assert.equal('attachments' in users[1][1], false);
  } finally { store?.close(); rmSync(directory, { recursive: true, force: true }); }
});
