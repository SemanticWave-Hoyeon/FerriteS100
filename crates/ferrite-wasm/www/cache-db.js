// Persistent portrayal cache of the web demo: the engine's cache entries
// (per-cell Lua portrayal caches, `.ferrite_*` names) in IndexedDB.
//
// The engine's cache store is synchronous; IndexedDB is not. The host loads
// the entries the next portrayal reads into the engine's memory cache before
// preparing (`WebEngine.missing_cache_entries` / `load_cache_entry`) and
// saves what the preparation wrote afterwards (`take_cache_writes`). Entries
// carry their own schema and integrity checks; a stale entry is rejected by
// the engine and portrayed again.

const DB_NAME = "ferrite-s100-portrayal-cache";
const STORE = "entries";
// Least recently used entries beyond this many bytes are deleted.
export const CACHE_BUDGET_BYTES = 512 * 1024 * 1024;

const done = (request) => new Promise((resolve, reject) => {
  request.onsuccess = () => resolve(request.result);
  request.onerror = () => reject(request.error);
});
const finished = (tx) => new Promise((resolve, reject) => {
  tx.oncomplete = () => resolve();
  tx.onabort = tx.onerror = () => reject(tx.error ?? new Error("IndexedDB transaction aborted"));
});

export class CacheDb {
  constructor(db) {
    this.db = db;
  }

  static async open() {
    if (typeof indexedDB === "undefined") throw new Error("No IndexedDB");
    const request = indexedDB.open(DB_NAME, 1);
    request.onupgradeneeded = () => request.result.createObjectStore(STORE, { keyPath: "name" });
    return new CacheDb(await done(request));
  }

  // [name, Uint8Array] pairs of the stored `names` (missing ones skipped);
  // marks them used.
  async getMany(names) {
    const tx = this.db.transaction(STORE, "readwrite");
    const store = tx.objectStore(STORE);
    const now = Date.now();
    const found = [];
    await Promise.all(names.map(async (name) => {
      const entry = await done(store.get(name));
      if (!entry) return;
      found.push([entry.name, entry.bytes]);
      entry.used = now;
      store.put(entry);
    }));
    await finished(tx);
    return found;
  }

  // Store [name, Uint8Array] pairs, then keep the store within its budget.
  async putMany(pairs) {
    const tx = this.db.transaction(STORE, "readwrite");
    const store = tx.objectStore(STORE);
    const now = Date.now();
    for (const [name, bytes] of pairs) store.put({ name, bytes, size: bytes.byteLength, used: now });
    await finished(tx);
    await this.prune();
  }

  // Entry count and bytes.
  async stats() {
    const entries = await this.list();
    return { entries: entries.length, bytes: entries.reduce((sum, e) => sum + e.size, 0) };
  }

  async list() {
    const tx = this.db.transaction(STORE, "readonly");
    const out = [];
    const request = tx.objectStore(STORE).openCursor();
    await new Promise((resolve, reject) => {
      request.onsuccess = () => {
        const cursor = request.result;
        if (!cursor) return resolve();
        const { name, size, used } = cursor.value;
        out.push({ name, size, used });
        cursor.continue();
      };
      request.onerror = () => reject(request.error);
    });
    return out;
  }

  async prune(budget = CACHE_BUDGET_BYTES) {
    const entries = await this.list();
    let total = entries.reduce((sum, e) => sum + e.size, 0);
    if (total <= budget) return 0;
    entries.sort((a, b) => a.used - b.used);
    const tx = this.db.transaction(STORE, "readwrite");
    const store = tx.objectStore(STORE);
    let removed = 0;
    for (const entry of entries) {
      if (total <= budget) break;
      store.delete(entry.name);
      total -= entry.size;
      removed += 1;
    }
    await finished(tx);
    return removed;
  }

  async clear() {
    const tx = this.db.transaction(STORE, "readwrite");
    tx.objectStore(STORE).clear();
    await finished(tx);
  }
}
