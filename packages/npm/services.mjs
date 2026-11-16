/** Embedded catalog management. Records do not install tools or connect channels. */
export class CatalogServices {
  #store;
  constructor(store) { this.#store = store; }
  collection(kind) { return new CatalogCollection(this.#store, kind); }
  bundledSkills() { return this.#store._dispatchAsync({ op: 'services', command: { op: 'bundled_skills' } }); }
  importBundledSkill(name) { return this.#store._dispatchAsync({ op: 'services', command: { op: 'import_bundled_skill', name } }); }
  importSkill(path) { return this.#store._dispatchAsync({ op: 'services', command: { op: 'import_skill', path } }); }
  async createTools() {
    const definitions = await this.#store._dispatchAsync({ op: 'services', command: { op: 'tool_definitions' } });
    return definitions.map(definition => ({ ...definition, execute: arguments_ =>
      this.#store._dispatchAsync({ op: 'services', command: { op: 'execute_tool', name: definition.name, arguments: arguments_ } }) }));
  }
}

class CatalogCollection {
  #store;
  #kind;
  constructor(store, kind) { this.#store = store; this.#kind = kind; }
  #call(op, input) {
    return this.#store._dispatchAsync({ op: 'services', command: { ...input, op, kind: this.#kind } });
  }
  create(id, data) { return this.#call('create', { id, data }); }
  get(id) { return this.#call('get', { id }); }
  list({ limit = 50, cursor = null } = {}) { return this.#call('list', { limit, cursor }); }
  update(id, revision, patch) { return this.#call('update', { id, revision, patch }); }
  delete(id, revision) { return this.#call('delete', { id, revision }); }
  setEnabled(id, revision, enabled) { return this.#call('set_enabled', { id, revision, enabled }); }
  search(query, { limit = 50 } = {}) { return this.#call('search', { query, limit }); }
}
