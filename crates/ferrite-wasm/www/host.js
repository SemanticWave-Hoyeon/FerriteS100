// The engine side of the web demo: one WebEngine with its WebCanvas and the
// persistent portrayal cache. It runs in a Web Worker drawing into an
// OffscreenCanvas (worker.js), or on the page's main thread drawing into the
// canvas element when workers cannot render (engine-client.js picks).
// Every method returns structured-cloneable data.
import init, { WebEngine, WebCanvas } from "./pkg/ferrite_wasm.js";
import { CacheDb, entryKind } from "./cache-db.js";

const PAYLOAD = /^\.?ferrite_pre_/;
// Cache entry name of a dropped payload (the engine's leading dot is optional).
const payloadEntry = (base) => (base.startsWith(".") ? base : `.${base}`);

// SHA-256 of a cache entry's payload (after the 40-byte header: magic,
// schema version, stored hash) with Web Crypto, which hashes natively and
// off this thread. The engine compares it with the stored hash and then
// skips hashing the payload in WebAssembly, the bulk of a cache-hit
// preparation (see WebEngine.load_cache_entry_digest). Null without Web
// Crypto (insecure context): the engine checks the entry itself.
async function payloadDigest(bytes) {
  const subtle = globalThis.crypto?.subtle;
  if (!subtle || bytes.byteLength < 40) return null;
  try {
    return new Uint8Array(await subtle.digest("SHA-256", bytes.subarray(40)));
  } catch {
    return null;
  }
}

// Test hook for the fallbacks of engine-client.js, comma-separated flags of
// the page's ?simulate=:
//   no-webgpu        neither the page nor workers offer WebGPU
//   no-offscreen     no OffscreenCanvas / transferControlToOffscreen (page)
//   no-worker-gpu    workers offer no WebGPU (the page still does)
//   worker-gpu-fail  workers offer WebGPU but get no adapter, so the worker's
//                    GPU context for the transferred canvas cannot be made
// `scope` is "page" or "worker". Returns the flags.
export function simulate(value, scope) {
  const flags = new Set((value ?? "").split(",").map((f) => f.trim()).filter(Boolean));
  const hideGpu = () => {
    const proto = scope === "worker" ? globalThis.WorkerNavigator?.prototype : globalThis.Navigator?.prototype;
    if (proto && "gpu" in proto) delete proto.gpu;
  };
  if (flags.has("no-webgpu")) hideGpu();
  if (scope === "worker" && flags.has("no-worker-gpu")) hideGpu();
  if (scope === "worker" && flags.has("worker-gpu-fail") && globalThis.GPU) {
    GPU.prototype.requestAdapter = async () => null;
  }
  if (scope === "page" && flags.has("no-offscreen")) {
    delete globalThis.OffscreenCanvas;
    if (globalThis.HTMLCanvasElement) delete HTMLCanvasElement.prototype.transferControlToOffscreen;
  }
  return flags;
}

export class EngineHost {
  // canvas: an OffscreenCanvas, or the id of a canvas element (main thread).
  // residency: the engine's residency threshold (estimated instructions;
  // null: default). viewReuse: re-projected frames reuse the scene's
  // emission set-up (false: set every frame up again, for comparisons).
  // cacheBudget: IndexedDB byte budget (null: the default 512 MiB).
  async start({ canvas, density, backend, persistentCache = true, residency = null, viewReuse = true,
    cacheBudget = null }) {
    const t0 = performance.now();
    this.wasm = await init();
    const bundle = new Uint8Array(await (await fetch(new URL("./catalogues.bin", import.meta.url),
      { cache: "no-store" })).arrayBuffer());
    this.engine = WebEngine.from_bundle(bundle);
    if (residency != null) this.engine.set_residency_threshold(residency);
    this.gpu = typeof canvas === "string"
      ? await WebCanvas.create(canvas, density, backend)
      : await WebCanvas.from_offscreen(canvas, density, backend);
    this.gpu.set_view_only_reuse(viewReuse);
    this.target = typeof canvas === "string" ? document.getElementById(canvas) : canvas;
    this.cache = null;
    let cache = "off";
    if (persistentCache) {
      try {
        this.cache = await CacheDb.open(cacheBudget ?? undefined);
        cache = describe(await this.cache.stats());
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
  // stored (also in IndexedDB, so a reload finds them), the rest opened
  // together.
  async addFiles(files) {
    let data = 0;
    const payloads = [];
    for (const { name, bytes } of files) {
      const base = name.split("/").pop();
      if (PAYLOAD.test(base)) {
        this.engine.load_precomputed(base, bytes);
        payloads.push([payloadEntry(base), bytes]);
      } else {
        this.engine.add_file(name, bytes);
        data += 1;
      }
    }
    let kept = 0;
    if (payloads.length && this.cache) {
      try {
        await this.cache.putMany(payloads);
        kept = payloads.length;
      } catch (error) {
        console.warn(`Payloads not kept in IndexedDB: ${error.message ?? error}`);
      }
    }
    if (!data) return { payloads: payloads.length, kept, data, report: null };
    const t0 = performance.now();
    const report = JSON.parse(this.engine.open());
    return { payloads: payloads.length, kept, data, report, openMs: performance.now() - t0 };
  }

  report() { return JSON.parse(this.engine.report()); }
  setPalette(name) { this.engine.set_palette(name); }
  setPortrayal(choice) { this.engine.set_portrayal(choice); }
  portrayal() { return JSON.parse(this.engine.portrayal()); }

  // Load the cache entries the next portrayal reads from IndexedDB: S-102
  // rule instructions and precomputed payloads first, then (when the
  // preparation still portrays with Lua) the per-cell Lua caches.
  async preload() {
    const t0 = performance.now();
    const out = { wanted: 0, loaded: 0, bytes: 0, kinds: {}, digestMs: 0, ms: 0 };
    for (let round = 0; round < 2; round++) {
      const names = JSON.parse(this.engine.missing_cache_entries());
      if (!names.length) break;
      out.wanted += names.length;
      const entries = this.cache ? await this.cache.getMany(names) : [];
      const hashed = performance.now();
      const digests = await Promise.all(entries.map(([, bytes]) => payloadDigest(bytes)));
      out.digestMs += performance.now() - hashed;
      for (const [index, [name, bytes]] of entries.entries()) {
        if (digests[index]) this.engine.load_cache_entry_digest(name, bytes, digests[index]);
        else this.engine.load_cache_entry(name, bytes);
        const kind = entryKind(name);
        out.kinds[kind] = (out.kinds[kind] ?? 0) + 1;
        out.bytes += bytes.byteLength;
      }
      out.loaded += entries.length;
      // Another round only when payloads were asked for (Lua may be avoided).
      if (!names.some((name) => name.startsWith(".ferrite_pre_"))) break;
    }
    out.ms = performance.now() - t0;
    return out;
  }

  // Save the entries the engine wrote to IndexedDB.
  async flush() {
    const writes = this.engine.take_cache_writes();
    if (!writes.length || !this.cache) return { saved: 0, kinds: {}, ms: 0 };
    const t0 = performance.now();
    const kinds = {};
    for (const [name] of writes) kinds[entryKind(name)] = (kinds[entryKind(name)] ?? 0) + 1;
    await this.cache.putMany(writes);
    return { saved: writes.length, kinds, pruned: this.cache.pruned, ms: performance.now() - t0 };
  }

  // Prepare (or re-project) the view and draw it.
  async render({ width, height, dpr, lat, lon, zoom }) {
    const preload = this.engine.scene_current() ? null : await this.preload();
    const t0 = performance.now();
    const summary = JSON.parse(this.engine.set_view(width, height, dpr, lat, lon, zoom, false));
    const t1 = performance.now();
    this.engine.render_to_canvas(this.gpu);
    const t2 = performance.now();
    const emit = JSON.parse(this.gpu.emit_timings());
    const saved = await this.flush();
    return { summary, prepareMs: t1 - t0, drawMs: t2 - t1, emit, preload, saved, memory: this.memory() };
  }

  // Memory: the wasm linear memory and, where offered (Chromium), the JS heap.
  memory() {
    const heap = globalThis.performance?.memory;
    return {
      wasmBytes: this.wasm?.memory?.buffer?.byteLength ?? null,
      jsHeapBytes: heap ? heap.usedJSHeapSize : null,
    };
  }

  async cacheStats() {
    return this.cache ? { ...(await this.cache.stats()), kinds: await this.cache.kinds() } : null;
  }

  // The current scene drawn again and copied out as an image file
  // (Uint8Array; e.g. "image/jpeg"), for scripted checks. The copy is taken
  // in the same task as the draw, before the frame is presented.
  async snapshot(type = "image/jpeg", quality = 0.9) {
    this.engine.render_to_canvas(this.gpu);
    const { width, height } = this.target;
    let blob;
    if (typeof OffscreenCanvas !== "undefined") {
      const copy = new OffscreenCanvas(width, height);
      copy.getContext("2d").drawImage(this.target, 0, 0);
      blob = await copy.convertToBlob({ type, quality });
    } else {
      const copy = Object.assign(document.createElement("canvas"), { width, height });
      copy.getContext("2d").drawImage(this.target, 0, 0);
      blob = await new Promise((resolve) => copy.toBlob(resolve, type, quality));
    }
    return new Uint8Array(await blob.arrayBuffer());
  }

  pick(x, y, radius) { return JSON.parse(this.engine.pick_json(x, y, radius)); }

  async clearCache() {
    if (this.cache) await this.cache.clear();
  }
}

const describe = ({ entries, bytes }) => `${entries} entries, ${(bytes / 1e6).toFixed(1)} MB`;
