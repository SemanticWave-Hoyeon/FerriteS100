// Ferrite S-100 web demo: loads the ferrite-wasm module, the catalogue
// bundle and user-supplied datasets, and draws with ferrite-gpu on a canvas.
import init, { WebEngine, WebCanvas } from "./pkg/ferrite_wasm.js";

const $ = (id) => document.getElementById(id);
const canvas = $("chart");
const status = (text) => { $("status").textContent = text; };

let engine = null;
let gpu = null;
// Camera: centre in degrees (NaN = data centre) and zoom factor over the
// all-data view; `view` is the last prepared view's bounds.
const camera = { lat: NaN, lon: NaN, zoom: 1 };
let view = null;
let opened = 0;

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

let pending = 0;
function scheduleRender(delay = 0) {
  clearTimeout(pending);
  pending = setTimeout(render, delay);
}

function render() {
  if (!engine || !gpu || opened === 0) return;
  const { dpr, width, height } = pixelSize();
  canvas.width = width;
  canvas.height = height;
  try {
    const t0 = performance.now();
    const summary = JSON.parse(
      engine.prepare(width, height, dpr, camera.lat, camera.lon, camera.zoom, false));
    const t1 = performance.now();
    engine.render_to_canvas(gpu);
    const t2 = performance.now();
    view = summary.bounds;
    camera.zoom = summary.zoom;
    camera.lat = latOf((mercY(view.north) + mercY(view.south)) / 2);
    camera.lon = (view.east + view.west) / 2;
    canvas.style.transform = "";
    $("hud").textContent =
      `${summary.instructions} instructions · 1:${Math.round(summary.display_scale)} · ` +
      `zoom ${summary.zoom.toFixed(2)} · ${camera.lat.toFixed(4)}, ${camera.lon.toFixed(4)} · ` +
      `prepare ${(t1 - t0).toFixed(0)} ms, draw ${(t2 - t1).toFixed(0)} ms`;
  } catch (error) {
    status(`Render failed: ${error.message ?? error}`);
  }
}

function showReport() {
  const report = JSON.parse(engine.report());
  const list = $("datasets");
  list.replaceChildren();
  for (const d of report.datasets) {
    const li = document.createElement("li");
    li.textContent = `${d.product} ${d.name} (ed. ${d.edition}, ${d.features} features)`;
    list.append(li);
  }
  const errors = report.issues.filter((i) => i.severity === "error");
  for (const issue of errors.slice(0, 20)) {
    const li = document.createElement("li");
    li.className = "issue-error";
    li.textContent = `${issue.check}: ${issue.subject}: ${issue.message}`;
    list.append(li);
  }
  if (!report.datasets.length && !errors.length) list.innerHTML = "<li>None</li>";
  return report;
}

async function addFiles(files) {
  let data = 0;
  let payloads = 0;
  for (const { name, bytes } of files) {
    const base = name.split("/").pop();
    if (base.startsWith(".ferrite_pre_") || base.startsWith("ferrite_pre_")) {
      engine.load_precomputed(base, bytes);
      payloads += 1;
    } else {
      engine.add_file(name, bytes);
      data += 1;
    }
  }
  let message = `${payloads} precomputed payload(s) stored`;
  if (data) {
    try {
      const t0 = performance.now();
      const report = JSON.parse(engine.open());
      opened += report.datasets.length;
      message = `Opened ${report.datasets.length} dataset(s) of ${data} file(s) in ` +
        `${(performance.now() - t0).toFixed(0)} ms; ${message}` +
        (report.notices.length ? `\n${report.notices.slice(0, 5).join("\n")}` : "");
    } catch (error) {
      message = `Open failed: ${error.message ?? error}`;
    }
  }
  status(message);
  showReport();
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
  const listed = await (await fetch("samples.json")).json();
  const files = [];
  for (const entry of listed) {
    const bytes = new Uint8Array(await (await fetch(entry.url)).arrayBuffer());
    files.push({ name: entry.name, bytes });
  }
  files.sort((a, b) => Number(!a.name.includes("ferrite_pre_")) - Number(!b.name.includes("ferrite_pre_")));
  await addFiles(files);
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
  $("fit").addEventListener("click", () => { camera.lat = NaN; camera.lon = NaN; camera.zoom = 1; scheduleRender(); });
  $("palette").addEventListener("change", (e) => {
    try { engine.set_palette(e.target.value); scheduleRender(); } catch (error) { status(error.message); }
  });

  // Pan by dragging (CSS preview while dragging), zoom with the wheel about
  // the cursor, pick on click.
  let drag = null;
  canvas.addEventListener("pointerdown", (e) => {
    canvas.setPointerCapture(e.pointerId);
    drag = { x: e.clientX, y: e.clientY, dx: 0, dy: 0 };
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
    scheduleRender(120);
  }, { passive: false });
  new ResizeObserver(() => scheduleRender(100)).observe(canvas);
}

function pick(e) {
  const rect = canvas.getBoundingClientRect();
  const { dpr } = pixelSize();
  try {
    const hits = JSON.parse(engine.pick_json((e.clientX - rect.left) * dpr, (e.clientY - rect.top) * dpr, 8 * dpr));
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
  await init();
  status("Loading catalogues…");
  const bundle = new Uint8Array(await (await fetch("catalogues.bin")).arrayBuffer());
  engine = WebEngine.from_bundle(bundle);
  const select = $("palette");
  for (const name of JSON.parse(engine.palettes())) select.add(new Option(name, name, name === "Day", name === "Day"));
  const { dpr, width, height } = pixelSize();
  canvas.width = width;
  canvas.height = height;
  gpu = await WebCanvas.create("chart", dpr);
  setupInput();
  const notes = JSON.parse(engine.catalogue_notes());
  status(`Ready: ${gpu.adapter()}${notes.length ? `\n${notes.join("\n")}` : ""}\nDrop files or load the samples.`);
}

main().catch((error) => status(`Start failed: ${error.message ?? error}`));
