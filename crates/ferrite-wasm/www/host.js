// The engine side of the web demo: one WebEngine with its WebCanvas and the
// persistent portrayal cache. It runs in a Web Worker drawing into an
// OffscreenCanvas (worker.js), or on the page's main thread drawing into the
// canvas element when workers cannot render (engine-client.js picks).
// Every method returns structured-cloneable data.
import init, { WebEngine, WebCanvas } from "./pkg/ferrite_wasm.js";
import { CacheDb } from "./cache-db.js";

const PAYLOAD = /^\.?ferrite_pre_/;

export class EngineHost {
  // canvas: an OffscreenCanvas, or the id of a canvas element (main thread).
  async start({ canvas, density, backend, persistentCache = true }) {
    const t0 = performance.now();
    await init();
    const bundle = new Uint8Array(await (await fetch(new URL("./catalogues.bin", import.meta.url),
      { cache: "no-store" })).arrayBuffer());
    this.engine = WebEngine.from_bundle(bundle);
    this.gpu = typeof canvas === "string"
      ? await WebCanvas.create(canvas, density, backend)
      : await WebCanvas.from_offscreen(canvas, density, backend);
    this.cache = null;
    let cache = "off";
    if (persistentCache) {
      try {
        this.cache = await CacheDb.open();
        const { entries, bytes } = await this.cache.stats();
        cache = `${entries} entries, ${(bytes / 1e6).toFixed(1)} MB`;
      } catch (error) {
        cache = `unavailable (${error.message ?? error})`;
      }
    }
    return {
      adapter: this.gpu.adapter(),
      lua: WebEngine.lua_available(),
      palettes: JSON.parse(this.engine.palettes()),
      notes: JSON.parse(this.engine.catalogue_notes()),
      cache,
      startMs: performance.now() - t0,
    };
  }

  // files: [{ name, bytes: Uint8Array }]; payloads (.ferrite_pre_*) are
  // stored, the rest opened together.
  addFiles(files) {
    let data = 0;
    let payloads = 0;
    for (const { name, bytes } of files) {
      const base = name.split("/").pop();
      if (PAYLOAD.test(base)) {
        this.engine.load_precomputed(base, bytes);
        payloads += 1;
      } else {
        this.engine.add_file(name, bytes);
        data += 1;
      }
    }
    if (!data) return { payloads, data, report: null };
    const t0 = performance.now();
    const report = JSON.parse(this.engine.open());
    return { payloads, data, report, openMs: performance.now() - t0 };
  }

  report() { return JSON.parse(this.engine.report()); }
  setPalette(name) { this.engine.set_palette(name); }
  setPortrayal(choice) { this.engine.set_portrayal(choice); }
  portrayal() { return JSON.parse(this.engine.portrayal()); }

  // Load the cache entries the next portrayal reads from IndexedDB.
  async preload() {
    const names = JSON.parse(this.engine.missing_cache_entries());
    if (!names.length || !this.cache) return { wanted: names.length, loaded: 0, ms: 0 };
    const t0 = performance.now();
    const entries = await this.cache.getMany(names);
    for (const [name, bytes] of entries) this.engine.load_cache_entry(name, bytes);
    return { wanted: names.length, loaded: entries.length, ms: performance.now() - t0 };
  }

  // Save the entries the engine wrote to IndexedDB.
  async flush() {
    const writes = this.engine.take_cache_writes();
    if (!writes.length || !this.cache) return { saved: 0, ms: 0 };
    const t0 = performance.now();
    await this.cache.putMany(writes);
    return { saved: writes.length, ms: performance.now() - t0 };
  }

  // Prepare (or re-project) the view and draw it.
  async render({ width, height, dpr, lat, lon, zoom }) {
    const preload = this.engine.scene_current() ? null : await this.preload();
    const t0 = performance.now();
    const summary = JSON.parse(this.engine.set_view(width, height, dpr, lat, lon, zoom, false));
    const t1 = performance.now();
    this.engine.render_to_canvas(this.gpu);
    const t2 = performance.now();
    const saved = await this.flush();
    return { summary, prepareMs: t1 - t0, drawMs: t2 - t1, preload, saved };
  }

  pick(x, y, radius) { return JSON.parse(this.engine.pick_json(x, y, radius)); }

  async clearCache() {
    if (this.cache) await this.cache.clear();
  }
}
