// WebAssembly half of the Lua-on-web parity check (tools/lua_wasm_parity.py):
// loads the ferrite-wasm build (wasm-bindgen --target web output, built with
// the lua-wasm feature) in Node and writes the raw Part 9a output of every
// dataset group in the manifest, in the format of examples/part9a_dump.rs.
//
//     node tools/lua_wasm_parity.mjs PKG_DIR MANIFEST.json OUT.txt
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [pkg, manifestPath, outPath] = process.argv.slice(2);
const wasm = await import(pathToFileURL(join(pkg, "ferrite_wasm.js")).href);
wasm.initSync({ module: readFileSync(join(pkg, "ferrite_wasm_bg.wasm")) });
if (!wasm.WebEngine.lua_available()) throw new Error("ferrite-wasm was built without lua-wasm");

const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
const read = (pairs) => pairs.map(([name, path]) => [name, new Uint8Array(readFileSync(path))]);
const catalogues = read(manifest.catalogues);
let out = "";
for (const group of manifest.groups) {
  const started = performance.now();
  out += `group\t${group.name}\n`;
  let engine;
  try {
    engine = new wasm.WebEngine(catalogues);
    for (const [name, bytes] of read(group.files)) engine.add_file(name, bytes);
    const report = JSON.parse(engine.open());
    const issue = report.issues.find((i) => i.severity === "error");
    out += `open\t${report.datasets.length}\t${issue ? `${issue.check}: ${issue.message}` : ""}\n`;
    out += engine.part9a_dump();
  } catch (error) {
    out += `error\t${error.message ?? error}\n`;
  } finally {
    engine?.free();
  }
  console.error(`${group.name}: ${((performance.now() - started) / 1000).toFixed(1)} s`);
}
writeFileSync(outPath, out);
