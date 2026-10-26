// Where the chart engine runs for the page. Preferred: a module Web Worker
// (worker.js) that renders into an OffscreenCanvas transferred from the
// page's canvas, so opening and the first Lua portrayal (seconds) never block
// the page. Fallback: the same host (host.js) on the main thread, drawing
// into the canvas element, when the browser has no OffscreenCanvas or module
// workers, or the worker cannot render (no WebGPU in workers while the page
// has it, or no GPU context for the offscreen canvas).
//
// Both return { kind, canvas, info, call(op, ...args) }; `call` runs host
// operations in order and resolves with their results. `canvas` is the
// element to attach input to (replaced when a failed worker took control of
// the original).
import { EngineHost } from "./host.js";

class WorkerEngine {
  constructor(worker) {
    this.worker = worker;
    this.next = 1;
    this.pending = new Map();
    this.ready = new Promise((resolve, reject) => this.pending.set(0, { resolve, reject }));
    worker.onmessage = ({ data }) => {
      const entry = this.pending.get(data.id);
      if (!entry) return;
      this.pending.delete(data.id);
      if (data.ok) entry.resolve(data.result);
      else entry.reject(new Error(data.error));
    };
    worker.onerror = (event) => {
      const error = new Error(event.message || "Worker failed");
      for (const entry of this.pending.values()) entry.reject(error);
      this.pending.clear();
    };
  }

  call(op, ...args) {
    return this.callTransfer(op, args, []);
  }

  callTransfer(op, args, transfer) {
    const id = this.next++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.worker.postMessage({ id, op, args }, transfer);
    });
  }
}

class LocalEngine {
  constructor() {
    this.host = new EngineHost();
    this.queue = Promise.resolve();
  }

  // Same ordering as the worker: one operation at a time.
  call(op, ...args) {
    const run = this.queue.then(() => this.host[op](...args));
    this.queue = run.catch(() => {});
    return run;
  }
}

function canUseWorker(canvas) {
  return typeof Worker !== "undefined" && typeof OffscreenCanvas !== "undefined" &&
    typeof canvas.transferControlToOffscreen === "function";
}

// mode: "auto" (worker when it can render with the requested backend),
// "on" (worker, still falling back on failure) or "off" (main thread).
// options: further EngineHost.start options (residency, viewReuse,
// cacheBudget). simulate: the test flags of host.js `simulate` (applied to
// the page by the caller before connecting; passed on to the worker).
export async function connect({ canvas, backend, density, mode = "auto", persistentCache = true, options = {},
  simulate = "", log = () => {} }) {
  const width = canvas.width;
  const height = canvas.height;
  const fallbacks = [];
  if (mode !== "off" && canUseWorker(canvas)) {
    let worker = null;
    try {
      const url = new URL("./worker.js", import.meta.url);
      if (simulate) url.searchParams.set("simulate", simulate);
      worker = new Worker(url, { type: "module" });
      const engine = new WorkerEngine(worker);
      const ready = await Promise.race([
        engine.ready,
        new Promise((_, reject) => setTimeout(() => reject(new Error("Worker did not start")), 10000)),
      ]);
      const pageWebGpu = "gpu" in navigator;
      // WebGPU is preferred: when the page has it but workers do not, keep
      // WebGPU on the main thread (unless WebGL2 was asked for, or forced).
      if (!ready.webgpu && pageWebGpu && backend !== "webgl2" && mode === "auto") {
        throw new Error("no WebGPU in workers");
      }
      const workerBackend = backend === "auto" && ready.webgpu ? "webgpu" : backend;
      const offscreen = canvas.transferControlToOffscreen();
      try {
        const info = await engine.callTransfer("start",
          [{ ...options, canvas: offscreen, density, backend: workerBackend, persistentCache }], [offscreen]);
        return { kind: "worker", canvas, info: { ...info, workerWebGpu: ready.webgpu }, fallbacks,
          call: engine.call.bind(engine) };
      } catch (error) {
        // The canvas belongs to the failed worker now: use a fresh one.
        const fresh = canvas.cloneNode(false);
        fresh.width = width;
        fresh.height = height;
        canvas.replaceWith(fresh);
        canvas = fresh;
        throw error;
      }
    } catch (error) {
      worker?.terminate();
      fallbacks.push(String(error.message ?? error));
      log(`Worker rendering unavailable (${error.message ?? error}); using the main thread`);
    }
  } else if (mode !== "off") {
    fallbacks.push("no OffscreenCanvas or module workers");
  }
  const engine = new LocalEngine();
  if (!canvas.id) canvas.id = "chart";
  const info = await engine.call("start", { ...options, canvas: canvas.id, density, backend, persistentCache });
  return { kind: "main", canvas, info, fallbacks, call: engine.call.bind(engine) };
}
