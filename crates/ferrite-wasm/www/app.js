// Ferrite S-100 web demo: the page (input, panels, HUD). The chart engine
// runs in a Web Worker drawing into an OffscreenCanvas, or on this thread
// when the browser cannot render from workers (engine-client.js); either
// way the page talks to it through `engine.call(op, ...)`.
import { connect } from "./engine-client.js";
import { simulate } from "./host.js";

const $ = (id) => document.getElementById(id);
let canvas = $("chart");
const status = (text) => { $("status").textContent = text; };

let engine = null;
// Camera: centre in degrees (NaN = data centre) and zoom factor over the
// all-data view; `view` is the last drawn view's bounds.
const camera = { lat: NaN, lon: NaN, zoom: 1 };
// Bumped by every user camera change, so a frame finishing late does not
// overwrite a newer camera with its own.
let cameraVersion = 0;
let view = null;
let opened = 0;
const params = new URLSearchParams(location.search);
// Renderer backend: ?backend=auto|webgpu|webgl2 (the select reloads the page).
const BACKENDS = ["auto", "webgpu", "webgl2"];
const backend = BACKENDS.includes(params.get("backend")) ? params.get("backend") : "auto";
// Engine thread: ?worker=auto|on|off.
const WORKER_MODES = ["auto", "on", "off"];
const workerMode = WORKER_MODES.includes(params.get("worker")) ? params.get("worker") : "auto";
// Persistent portrayal cache: ?cache=0 disables it; ?cachebudget=MB sets
// its least-recently-used byte budget (default 512 MiB).
const persistentCache = params.get("cache") !== "0";
const cacheBudget = Number(params.get("cachebudget")) > 0 ? Number(params.get("cachebudget")) * 1e6 : null;
// Residency threshold (estimated instructions; 0: always): ?residency=N.
const residency = params.has("residency") && Number.isFinite(Number(params.get("residency")))
  ? Number(params.get("residency")) : null;
// ?viewreuse=0: re-projected frames set the emission up again (comparisons).
const viewReuse = params.get("viewreuse") !== "0";
// Test hook for the engine-thread fallbacks: ?simulate=no-webgpu,
// no-offscreen, no-worker-gpu, worker-gpu-fail (see host.js `simulate`).
const simulated = params.get("simulate") ?? "";
simulate(simulated, "page");
let adapter = "";

const mercY = (lat) => Math.log(Math.tan(Math.PI / 4 + (lat * Math.PI) / 360));
const latOf = (y) => (360 / Math.PI) * Math.atan(Math.exp(y)) - 90;

function pixelSize() {
  const dpr = window.devicePixelRatio || 1;
  return {
    dpr,
    width: Math.max(1, Math.round(canvas.clientWidth * dpr)),
    height: Math.max(1, Math.round(canvas.clientHeight * dpr)),
  };
}

// Main-thread responsiveness: the longest gap between animation frames
// while a frame request is in flight (the first Lua portrayal takes
// seconds; in a worker the page keeps animating).
// Long tasks (over 50 ms, reported by the browser even for a hidden page,
// where animation frames pause) give the longest main-thread block.
const ui = { busy: false, maxGap: 0, last: 0, longTasks: [] };
function watchFrames(now) {
  if (ui.busy && ui.last) ui.maxGap = Math.max(ui.maxGap, now - ui.last);
  ui.last = now;
  requestAnimationFrame(watchFrames);
}
requestAnimationFrame(watchFrames);
// ?probe=1: also measure the longest main-thread block with a message-loop
// probe while a frame is in flight (works in hidden pages, where animation
// frames pause and long tasks may go unreported; it keeps one core busy).
const probing = params.get("probe") === "1";
const probe = new MessageChannel();
probe.port1.onmessage = () => {
  const now = performance.now();
  if (!ui.busy) return;
  ui.probeMax = Math.max(ui.probeMax, now - ui.probeLast);
  ui.probeLast = now;
  probe.port2.postMessage(0);
};
function startProbe() {
  ui.probeMax = 0;
  ui.probeLast = performance.now();
  if (probing) probe.port2.postMessage(0);
}
try {
  new PerformanceObserver((list) => {
    for (const entry of list.getEntries()) {
      ui.longTasks.push({ start: entry.startTime, duration: entry.duration });
    }
    ui.longTasks = ui.longTasks.slice(-200);
  }).observe({ type: "longtask" });
} catch {
  // No Long Tasks API: only the frame gap is measured.
}

let pending = 0;
function scheduleRender(delay = 0) {
  clearTimeout(pending);
  pending = setTimeout(render, delay);
}

// Below this CSS size the canvas is still being laid out (e.g. a pane that is
// opening); fitting the data to it would produce a meaningless scale that the
// camera would then keep. The ResizeObserver renders once the canvas grows.
const MIN_CANVAS_CSS_PX = 64;

// Per-frame timings (prepare/set_view and draw), for measurements.
const timings = [];
let inFlight = false;
let again = false;
let dragging = false;

// One frame request at a time; requests made meanwhile collapse into one.
async function render() {
  if (!engine || opened === 0) return;
  if (inFlight) { again = true; return; }
  inFlight = true;
  try {
    do {
      again = false;
      await renderOnce();
    } while (again);
  } finally {
    inFlight = false;
  }
}

async function renderOnce() {
  if (canvas.clientWidth < MIN_CANVAS_CSS_PX || canvas.clientHeight < MIN_CANVAS_CSS_PX) return;
  const { dpr, width, height } = pixelSize();
  if (engine.kind === "main" && (canvas.width !== width || canvas.height !== height)) {
    canvas.width = width;
    canvas.height = height;
  }
  const version = cameraVersion;
  ui.busy = true;
  ui.maxGap = 0;
  ui.last = 0;
  startProbe();
  const t0 = performance.now();
  try {
    // set_view re-projects the prepared scene when only the camera or size
    // changed and the resident cells stay; otherwise it portrays again.
    const frame = await engine.call("render",
      { width, height, dpr, lat: camera.lat, lon: camera.lon, zoom: camera.zoom });
    const total = performance.now() - t0;
    // Long-task entries arrive after the task; wait for them briefly.
    await new Promise((resolve) => setTimeout(resolve, 50));
    const longTask = Math.max(0, ...ui.longTasks.filter((e) => e.start >= t0 - 1).map((e) => e.duration));
    const { summary } = frame;
    view = summary.bounds;
    if (version === cameraVersion) {
      camera.zoom = summary.zoom;
      camera.lat = latOf((mercY(view.north) + mercY(view.south)) / 2);
      camera.lon = (view.east + view.west) / 2;
    } else {
      again = true;
    }
    if (!dragging) canvas.style.transform = "";
    const timing = {
      prepare: frame.prepareMs, draw: frame.drawMs, total, reprojected: summary.reprojected,
      portrayal: summary.portrayal, instructions: summary.instructions, cacheReads: summary.cache_reads,
      resident: summary.resident_cells, cells: summary.cells, zoom: summary.zoom, emit: frame.emit,
      memory: frame.memory, preload: frame.preload, saved: frame.saved, uiMaxGap: ui.maxGap, longTask,
      probeMax: probing ? ui.probeMax : null, thread: engine.kind,
    };
    timings.push(timing);
    const source = summary.portrayal == null ? "" :
      ` · S-101 ${summary.portrayal === "lua" ? "Lua" : "precomputed"}`;
    const s102 = summary.bathymetry_files ? ` · S-102 ${summary.bathymetry_files} file(s)` : "";
    const resident = summary.resident_cells == null ? "" : ` · ${summary.resident_cells}/${summary.cells} cells resident`;
    const kinds = (counts) => Object.entries(counts ?? {}).map(([k, n]) => `${n} ${k}`).join(", ");
    const cache = frame.preload ? ` · cache ${frame.preload.loaded}/${frame.preload.wanted} from IndexedDB` +
      `${frame.preload.loaded ? ` (${kinds(frame.preload.kinds)})` : ""}, ${summary.cache_reads} read` : "";
    const savedText = frame.saved?.saved ? `, ${frame.saved.saved} saved (${kinds(frame.saved.kinds)})` : "";
    const emitText = frame.emit ? ` (emit ${frame.emit.instructions_ms.toFixed(0)}` +
      `${frame.emit.view_only ? ", view-only" : `, set-up ${(frame.emit.setup_ms + frame.emit.prefill_ms).toFixed(0)}`})` : "";
    const memory = frame.memory?.wasmBytes ? ` · wasm ${(frame.memory.wasmBytes / 2 ** 20).toFixed(0)} MiB` : "";
    $("hud").textContent = `${adapter} · ${engine.kind === "worker" ? "worker" : "main thread"}${source}${s102} · ` +
      `${summary.instructions} instructions · 1:${Math.round(summary.display_scale)} · ` +
      `zoom ${summary.zoom.toFixed(2)} · ${camera.lat.toFixed(4)}, ${camera.lon.toFixed(4)}${resident} · ` +
      `${summary.reprojected ? "view" : "portray"} ${frame.prepareMs.toFixed(0)} ms, draw ${frame.drawMs.toFixed(0)} ms` +
      `${emitText}${cache}${savedText}${memory} · UI max frame gap ${ui.maxGap.toFixed(0)} ms, ` +
      `longest main-thread task ${longTask.toFixed(0)} ms` +
      `${probing ? `, longest block ${ui.probeMax.toFixed(0)} ms` : ""}`;
  } catch (error) {
    status(`Render failed: ${error.message ?? error}`);
  } finally {
    ui.busy = false;
  }
}

async function showReport() {
  const report = await engine.call("report");
  const list = $("datasets");
  list.replaceChildren();
  for (const d of report.datasets) {
    const li = document.createElement("li");
    li.textContent = `${d.product} ${d.name} (ed. ${d.edition}, ${d.features} features)`;
    list.append(li);
  }
  for (const b of report.bathymetry ?? []) {
    const li = document.createElement("li");
    li.textContent = `S-102 ${b.source.split("/").pop()} (${b.product_specification}, ` +
      `${b.grid[0]}×${b.grid[1]}${b.inspection_only ? ", inspection only" : ""})`;
    list.append(li);
  }
  const errors = report.issues.filter((i) => i.severity === "error");
  for (const issue of errors.slice(0, 20)) {
    const li = document.createElement("li");
    li.className = "issue-error";
    li.textContent = `${issue.check}: ${issue.subject}: ${issue.message}`;
    list.append(li);
  }
  if (!report.datasets.length && !report.bathymetry?.length && !errors.length) list.innerHTML = "<li>None</li>";
  return report;
}

async function addFiles(files) {
  let message;
  try {
    const { payloads, kept, data, report, openMs } = await engine.call("addFiles", files);
    message = `${payloads} precomputed payload(s) stored` + (kept ? ` (${kept} kept in IndexedDB)` : "");
    if (report) {
      opened += report.datasets.length;
      const bathymetry = report.bathymetry?.length ?? 0;
      opened += bathymetry;
      message = `Opened ${report.datasets.length} dataset(s) of ${data} file(s) in ` +
        `${openMs.toFixed(0)} ms; ${message}` +
        (report.notices.length ? `\n${report.notices.slice(0, 5).join("\n")}` : "");
      if (bathymetry) message += `\nS-102: ${bathymetry} file(s)`;
    }
  } catch (error) {
    message = `Open failed: ${error.message ?? error}`;
  }
  status(message);
  await showReport();
  scheduleRender();
}

async function readFileList(list) {
  const files = [];
  for (const file of list) {
    files.push({ name: file.webkitRelativePath || file.name, bytes: new Uint8Array(await file.arrayBuffer()) });
  }
  // Payloads first, so S-101 cells can be portrayed when opened.
  files.sort((a, b) => Number(!a.name.includes("ferrite_pre_")) - Number(!b.name.includes("ferrite_pre_")));
  return files;
}

async function loadSamples() {
  status("Fetching samples…");
  // Not from the HTTP cache: sample names (samples/data/NNNN) and their
  // modification times (copied) repeat across builds with other contents.
  const listed = await (await fetch("samples.json", { cache: "no-store" })).json();
  const files = [];
  for (const entry of listed) {
    const bytes = new Uint8Array(await (await fetch(entry.url, { cache: "no-store" })).arrayBuffer());
    files.push({ name: entry.name, bytes });
  }
  files.sort((a, b) => Number(!a.name.includes("ferrite_pre_")) - Number(!b.name.includes("ferrite_pre_")));
  await addFiles(files);
}

function reloadWith(key, value) {
  const url = new URL(location.href);
  url.searchParams.set(key, value);
  location.assign(url);
}

function setupInput() {
  const drop = $("drop");
  drop.addEventListener("dragover", (e) => { e.preventDefault(); drop.classList.add("over"); });
  drop.addEventListener("dragleave", () => drop.classList.remove("over"));
  drop.addEventListener("drop", async (e) => {
    e.preventDefault();
    drop.classList.remove("over");
    await addFiles(await readFileList(e.dataTransfer.files));
  });
  $("pick-files").addEventListener("click", () => $("files").click());
  $("files").addEventListener("change", async (e) => addFiles(await readFileList(e.target.files)));
  $("samples").addEventListener("click", () => loadSamples().catch((e) => status(`Samples: ${e}`)));
  $("fit").addEventListener("click", () => {
    camera.lat = NaN; camera.lon = NaN; camera.zoom = 1; cameraVersion++; scheduleRender();
  });
  $("backend").addEventListener("change", (e) => reloadWith("backend", e.target.value));
  $("thread").addEventListener("change", (e) => reloadWith("worker", e.target.value));
  $("clear-cache").addEventListener("click", async () => {
    await engine.call("clearCache");
    status("Portrayal cache cleared (IndexedDB).");
  });
  $("portrayal").addEventListener("change", async (e) => {
    try { await engine.call("setPortrayal", e.target.value); scheduleRender(); } catch (error) { status(error.message); }
  });
  $("palette").addEventListener("change", async (e) => {
    try { await engine.call("setPalette", e.target.value); scheduleRender(); } catch (error) { status(error.message); }
  });

  // Pan by dragging (CSS preview while dragging), zoom with the wheel about
  // the cursor, pick on click.
  let drag = null;
  canvas.addEventListener("pointerdown", (e) => {
    canvas.setPointerCapture(e.pointerId);
    drag = { x: e.clientX, y: e.clientY, dx: 0, dy: 0 };
    dragging = true;
  });
  canvas.addEventListener("pointermove", (e) => {
    if (!drag) return;
    drag.dx = e.clientX - drag.x;
    drag.dy = e.clientY - drag.y;
    canvas.style.transform = `translate(${drag.dx}px, ${drag.dy}px)`;
  });
  canvas.addEventListener("pointerup", (e) => {
    const d = drag;
    drag = null;
    dragging = false;
    if (!d || !view) return;
    if (Math.hypot(d.dx, d.dy) < 4) {
      canvas.style.transform = "";
      pick(e);
      return;
    }
    const lonPerPx = (view.east - view.west) / canvas.clientWidth;
    const yPerPx = (mercY(view.north) - mercY(view.south)) / canvas.clientHeight;
    camera.lon -= d.dx * lonPerPx;
    camera.lat = latOf(mercY(camera.lat) + d.dy * yPerPx);
    cameraVersion++;
    scheduleRender();
  });
  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    if (!view) return;
    const factor = Math.exp(-e.deltaY * 0.002);
    const rect = canvas.getBoundingClientRect();
    const fx = (e.clientX - rect.left) / rect.width;
    const fy = (e.clientY - rect.top) / rect.height;
    const lon = view.west + fx * (view.east - view.west);
    const y = mercY(view.north) - fy * (mercY(view.north) - mercY(view.south));
    camera.lon = lon + (camera.lon - lon) / factor;
    camera.lat = latOf(y + (mercY(camera.lat) - y) / factor);
    camera.zoom *= factor;
    cameraVersion++;
    scheduleRender(120);
  }, { passive: false });
  new ResizeObserver(() => scheduleRender(100)).observe(canvas);
}

async function pick(e) {
  const rect = canvas.getBoundingClientRect();
  const { dpr } = pixelSize();
  try {
    const hits = await engine.call("pick", (e.clientX - rect.left) * dpr, (e.clientY - rect.top) * dpr, 8 * dpr);
    const out = $("pick");
    out.replaceChildren();
    if (!hits.length) { out.textContent = "Nothing here."; return; }
    for (const hit of hits.slice(0, 5)) {
      const title = document.createElement("div");
      title.innerHTML = `<b></b> <span style="color:var(--muted)"></span>`;
      title.firstChild.textContent = hit.feature_type;
      title.lastChild.textContent = hit.cell_index == null ? "overlay" : `cell ${hit.cell_index}`;
      const table = document.createElement("table");
      for (const [k, v] of hit.attributes.slice(0, 30)) {
        const tr = table.insertRow();
        tr.insertCell().textContent = k;
        tr.insertCell().textContent = typeof v === "string" ? v : JSON.stringify(v);
      }
      out.append(title, table);
    }
  } catch (error) {
    status(`Pick failed: ${error.message ?? error}`);
  }
}

async function main() {
  status("Loading engine and catalogues…");
  const { dpr, width, height } = pixelSize();
  canvas.width = width;
  canvas.height = height;
  $("backend").value = backend;
  $("thread").value = workerMode;
  const notices = [];
  engine = await connect({ canvas, backend, density: dpr, mode: workerMode, persistentCache,
    options: { residency, viewReuse, cacheBudget }, simulate: simulated,
    log: (text) => notices.push(text) });
  canvas = engine.canvas;
  const { info } = engine;
  adapter = info.adapter;
  $("portrayal").querySelector("option[value=lua]").disabled = !info.lua;
  if (!info.lua) $("portrayal").value = "precomputed";
  const select = $("palette");
  for (const name of info.palettes) select.add(new Option(name, name, name === "Day", name === "Day"));
  $("hud").textContent = adapter;
  setupInput();
  status(`Ready in ${info.startMs.toFixed(0)} ms: ${adapter} [backend ${backend}] on the ` +
    `${engine.kind === "worker" ? "worker (OffscreenCanvas)" : "main thread"}, ` +
    `S-101 ${info.lua ? "Lua portrayal in the browser" : "precomputed payloads only"}, ` +
    `portrayal cache: ${info.cache}` +
    `${simulated ? `\nSimulating: ${simulated}` : ""}` +
    `${notices.length ? `\n${notices.join("\n")}` : ""}` +
    `${info.notes.length ? `\n${info.notes.join("\n")}` : ""}\nDrop files or load the samples.`);
  // For scripted checks: the engine connection, the camera, frame timings
  // and the main-thread frame monitor.
  window.ferrite = {
    engine, camera, timings, render, ui, loadSamples, simulated,
    // The current frame as image bytes (see host.js `snapshot`).
    snapshot: (type, quality) => engine.call("snapshot", type, quality),
    setPortrayal: async (v) => { await engine.call("setPortrayal", v); $("portrayal").value = v; },
    setCamera: (lat, lon, zoom) => {
      Object.assign(camera, { lat, lon, zoom }); cameraVersion++; return render();
    },
  };
}

main().catch((error) => status(`Start failed: ${error.message ?? error}`));
