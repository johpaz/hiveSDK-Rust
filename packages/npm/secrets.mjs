/** Explicit host capability. Keys never pass through JSON or catalog commands. */
const finalizer = new FinalizationRegistry(({ store, native }) => {
  try { native.close(); } catch {} finally { store._release(); }
});
export function copySecretKey(key) {
  if (!(key instanceof Uint8Array) || key.byteLength !== 32) throw new TypeError('Secret master key must be exactly 32 bytes in a Uint8Array');
  return Buffer.from(key);
}
export class SecretVault {
  #store; #native; #active = 0; #closed = false; #closing = false; #token = {};
  constructor(store, native) {
    this.#store = store; this.#native = native;
    finalizer.register(this, { store, native }, this.#token);
  }
  static open({ store, ...options }) { return store.openSecrets(options); }
  toJSON() { return { type: 'SecretVault' }; }
  #enter() {
    if (this.#closed) throw new Error('CLOSED: secret vault is closed');
    if (this.#closing) throw new Error('BUSY: secret vault close is pending');
    this.#store._retain(); this.#active++;
  }
  #leave() { this.#active--; this.#store._release(); }
  async #call(op, fields = {}) {
    this.#enter();
    try { return JSON.parse(await this.#native.dispatchAsync(JSON.stringify({ ...fields, op }))); }
    finally { this.#leave(); }
  }
  info() { return this.#call('info'); }
  inspect(name) { return this.#call('inspect', { name }); }
  reveal(name) { return this.#call('reveal', { name }); }
  put(name, value, { revision = 0 } = {}) { return this.#call('put', { name, value, revision }); }
  delete(name, revision) { return this.#call('delete', { name, revision }); }
  list({ limit = 50, cursor = null, includeDeleted = false } = {}) {
    return this.#call('list', { limit, cursor, include_deleted: includeDeleted });
  }
  async rekey({ keyId, key }) {
    const material = copySecretKey(key); let entered = false;
    try { this.#enter(); entered = true; return JSON.parse(await this.#native.rekeyAsync(keyId, material)); }
    finally { material.fill(0); if (entered) this.#leave(); }
  }
  async close() {
    if (this.#closed) return;
    if (this.#closing || this.#active) throw new Error('BUSY: wait for secret operations before closing');
    this.#closing = true;
    try {
      await this.#native.closeAsync();
      this.#closed = true; finalizer.unregister(this.#token); this.#store._release();
    } finally { this.#closing = false; }
  }
}
