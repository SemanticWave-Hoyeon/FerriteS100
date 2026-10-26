// Persistent portrayal cache of the web demo: the engine's cache entries
// (`.ferrite_*` names) in IndexedDB: per-cell Lua portrayal caches
// (`.ferrite_cell_*`), S-102 rule instructions (`.ferrite_s102_*`) and the
// precomputed S-101 payloads the user dropped (`.ferrite_pre_*`), under one
// least-recently-used byte budget.
//
// The engine's cache store is synchronous; IndexedDB is not. The host loads
// the entries the next portrayal reads into the engine's memory cache before
// preparing (`WebEngine.missing_cache_entries` / `load_cache_entry`) and
// saves what the preparation wrote afterwards (`take_cache_writes`). Entries
// carry their own schema and integrity checks; a stale entry is rejected by
// the engine and portrayed again.
//
// Bytes and bookkeeping live in separate object stores: "entries" holds
// { name, bytes }, "meta" holds { name, size, used }. Marking entries used,
// listing, counting and pruning touch only "meta", so a reload that reads
// hundreds of MB of entries does not write them back (version 1 kept `used`
// next to the bytes and re-put every entry it read).

const DB_NAME = "ferrite-s100-portrayal-cache";
const DB_VERSION = 2;
const ENTRIES = "entries";
const META = "meta";
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

// Kind of an entry by its name.
export function entryKind(name) {
  if (name.startsWith(".ferrite_pre_")) return "payload";
  if (name.startsWith(".ferrite_s102_")) return "s102";
  if (name.startsWith(".ferrite_cell_")) return "lua";
  return "other";
}

export class CacheDb {
  constructor(db, budget) {
    this.db = db;
    this.budget = budget;
    this.pruned = 0;
  }

  // budget: bytes kept (least recently used entries beyond it are deleted).
  static async open(budget = CACHE_BUDGET_BYTES) {
    if (typeof indexedDB === "undefined") throw new Error("No IndexedDB");
    const request = indexedDB.open(DB_NAME, DB_VERSION);
    request.onupgradeneeded = () => {
      // A cache: earlier layouts are dropped, not migrated.
      const db = request.result;
      for (const name of [...db.objectStoreNames]) db.deleteObjectStore(name);
      db.createObjectStore(ENTRIES, { keyPath: "name" });
      db.createObjectStore(META, { keyPath: "name" });
    };
    return new CacheDb(await done(request), budget);
  }

  // [name, Uint8Array] pairs of the stored `names` (missing ones skipped);
  // marks them used.
  async getMany(names) {
    const tx = this.db.transaction([ENTRIES, META], "readwrite");
    const entries = tx.objectStore(ENTRIES);
    const meta = tx.objectStore(META);
    const now = Date.now();
    const found = [];
    await Promise.all(names.map(async (name) => {
      const [entry, info] = await Promise.all([done(entries.get(name)), done(meta.get(name))]);
      if (!entry) return;
      found.push([entry.name, entry.bytes]);
      meta.put({ name, size: info?.size ?? entry.bytes.byteLength, used: now });
    }));
    await finished(tx);
    return found;
  }

  // Store [name, Uint8Array] pairs, then keep the store within its budget
  // (`this.pruned`: entries the last call deleted).
  async putMany(pairs) {
    const tx = this.db.transaction([ENTRIES, META], "readwrite");
    const entries = tx.objectStore(ENTRIES);
    const meta = tx.objectStore(META);
    const now = Date.now();
    for (const [name, bytes] of pairs) {
      entries.put({ name, bytes });
      meta.put({ name, size: bytes.byteLength, used: now });
    }
    await finished(tx);
    this.pruned = await this.prune();
  }

  // Entry count and bytes.
  async stats() {
    const entries = await this.list();
    return { entries: entries.length, bytes: entries.reduce((sum, e) => sum + e.size, 0) };
  }

  // Entry count by kind (payload, s102, lua, other).
  async kinds() {
    const out = {};
    for (const { name } of await this.list()) out[entryKind(name)] = (out[entryKind(name)] ?? 0) + 1;
    return out;
  }

  // { name, size, used } of every entry (bookkeeping only, no bytes).
  async list() {
    const tx = this.db.transaction(META, "readonly");
    return done(tx.objectStore(META).getAll());
  }

  // Delete least recently used entries until the store is within `budget`
  // bytes; the number deleted.
  async prune(budget = this.budget) {
    const entries = await this.list();
    let total = entries.reduce((sum, e) => sum + e.size, 0);
    if (total <= budget) return 0;
    entries.sort((a, b) => a.used - b.used);
    const tx = this.db.transaction([ENTRIES, META], "readwrite");
    let removed = 0;
    for (const entry of entries) {
      if (total <= budget) break;
      tx.objectStore(ENTRIES).delete(entry.name);
      tx.objectStore(META).delete(entry.name);
      total -= entry.size;
      removed += 1;
    }
    await finished(tx);
    return removed;
  }

  async clear() {
    const tx = this.db.transaction([ENTRIES, META], "readwrite");
    tx.objectStore(ENTRIES).clear();
    tx.objectStore(META).clear();
    await finished(tx);
  }
}
