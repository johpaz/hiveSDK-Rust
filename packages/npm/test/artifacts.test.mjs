import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtempSync, readdirSync, rmSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { SessionStore } from '../index.mjs';

// PNG signature + IHDR for a 640x480 image; enough for header inspection.
const png = Buffer.concat([Buffer.from('89504e470d0a1a0a0000000d49484452', 'hex'), Buffer.from([0, 0, 2, 128, 0, 0, 1, 224]), Buffer.alloc(9)]);

test('artifacts save, read, inspect and expire through agent-ready tools', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-artifacts-')); let store;
  try {
    store = new SessionStore({ path: path.join(directory, 'db'), tenantId: 't' });
    const root = path.join(directory, 'files');
    const artifacts = store.artifacts(root);
    const tools = await artifacts.createTools();
    assert.deepEqual(tools.map(tool => tool.name), ['artifact_save', 'artifact_inspect', 'artifact_read', 'artifact_list', 'artifact_delete', 'image_info', 'artifact_transform_image']);
    const call = (name, args) => tools.find(tool => tool.name === name).execute(args);

    const note = await call('artifact_save', { kind: 'note', mime_type: 'text/plain', text: 'hola 🦀', expires_in_days: 1 });
    assert.equal((await call('artifact_read', { id: note.id })).text, 'hola 🦀');
    const image = await call('artifact_save', { kind: 'shot', mime_type: 'application/octet-stream', data: png.toString('base64') });
    assert.deepEqual([image.mime_type, image.width, image.height], ['image/png', 640, 480]);
    assert.deepEqual(await call('image_info', { id: image.id }), { mime_type: 'image/png', width: 640, height: 480 });
    assert.equal((await call('artifact_read', { id: image.id, encoding: 'base64' })).data, png.toString('base64'));
    assert.equal((await call('artifact_list', {})).items.length, 2);
    assert.equal(readdirSync(path.join(root, 'artifacts')).length, 1, 'files live under <root>/artifacts/<day>');

    assert.deepEqual(await artifacts.expire({ now: note.expires_at - 1 }), []);
    assert.deepEqual(await artifacts.expire({ now: note.expires_at }), [note.id]);
    await assert.rejects(call('artifact_read', { id: note.id }), /ARTIFACT_EXPIRED/);
    assert.equal((await call('artifact_inspect', { id: note.id })).status, 'expired');
    await assert.rejects(call('artifact_read', { id: '../etc/passwd' }), /do not match schema/);
    assert.throws(() => store.artifacts(''), /root directory/);

    const other = (await store.forTenantAsync('other')).artifacts(root);
    assert.equal((await other.execute('artifact_list', {})).items.length, 0, 'tenants are isolated');
    await assert.rejects(other.execute('artifact_read', { id: image.id }), /ARTIFACT_NOT_FOUND/);
  } finally { store?.close(); rmSync(directory, { recursive: true, force: true }); }
});

// A real, decodable 4x2 PNG: red on the left half, blue on the right.
const real = 'iVBORw0KGgoAAAANSUhEUgAAAAQAAAACCAIAAADwyuo0AAAAFUlEQVR4nGP4z8Dwn4EBTPxnQOYAAGeyB/nadYY6AAAAAElFTkSuQmCC';

test('artifact_transform_image makes a new image artifact and keeps the source', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-artifacts-image-')); let store;
  try {
    store = new SessionStore({ path: path.join(directory, 'db'), tenantId: 't' });
    const artifacts = store.artifacts(path.join(directory, 'files'));
    const tools = await artifacts.createTools();
    const call = (name, args) => tools.find(tool => tool.name === name).execute(args);
    const source = await call('artifact_save', { kind: 'photo', mime_type: 'image/png', data: real });
    assert.deepEqual([source.width, source.height], [4, 2]);
    const turned = await call('artifact_transform_image', { id: source.id, rotate: 90, format: 'webp' });
    assert.deepEqual([turned.width, turned.height, turned.artifact.mime_type, turned.source], [2, 4, 'image/webp', source.id]);
    const bytes = Buffer.from((await call('artifact_read', { id: turned.artifact.id, encoding: 'base64' })).data, 'base64');
    assert.equal(bytes.subarray(8, 12).toString(), 'WEBP');
    assert.deepEqual(await call('image_info', { id: turned.artifact.id }), { mime_type: 'image/webp', width: 2, height: 4 });
    assert.equal((await call('artifact_inspect', { id: source.id })).status, 'active', 'the source is kept');
    await assert.rejects(call('artifact_transform_image', { id: source.id }), /no transformation/);
    await assert.rejects(call('artifact_transform_image', { id: source.id, rotate: 45 }), /do not match schema/);
    await assert.rejects(call('artifact_transform_image', { id: source.id, crop: { x: 3, y: 0, width: 2, height: 2 } }), /outside the image/);
  } finally { store?.close(); rmSync(directory, { recursive: true, force: true }); }
});
