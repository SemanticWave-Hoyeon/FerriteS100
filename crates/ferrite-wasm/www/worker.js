// Web Worker running the chart engine (host.js) off the page's main thread:
// portrayal (Lua), opening and drawing into the OffscreenCanvas the page
// transferred. Requests are { id, op, args } and run one at a time in
// arrival order; replies are { id, ok, result } or { id, ok: false, error }.
import { EngineHost, simulate } from "./host.js";

// Test hook (?simulate=... of the page, passed in this module's URL): see
// engine-client.js. Applied before the engine or the GPU is touched.
simulate(new URL(import.meta.url).searchParams.get("simulate"), "worker");

const host = new EngineHost();
let queue = Promise.resolve();

self.onmessage = ({ data: { id, op, args } }) => {
  queue = queue.then(async () => {
    try {
      if (typeof host[op] !== "function") throw new Error(`Unknown operation ${op}`);
      const result = await host[op](...(args ?? []));
      self.postMessage({ id, ok: true, result });
    } catch (error) {
      self.postMessage({ id, ok: false, error: String(error?.message ?? error) });
    }
  });
};

// Whether this worker scope offers WebGPU (the page decides where to render).
self.postMessage({ id: 0, ok: true, result: { ready: true, webgpu: "gpu" in self.navigator } });
