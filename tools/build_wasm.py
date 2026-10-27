#!/usr/bin/env python3
"""Build the ferrite-wasm web demo (docs/ENGINE_DESIGN.md, stage 6).

Compiles crates/ferrite-wasm for wasm32-unknown-unknown, generates the
JavaScript bindings with wasm-bindgen (the CLI version must equal the
wasm-bindgen crate, 0.2.108), copies crates/ferrite-wasm/www and writes the
catalogue bundle (catalogues.bin: S-101, S-102, S-124, S-129 FC/PC).
Everything goes to --out, outside the repository; cargo output goes to
--target-dir.

    cargo install wasm-bindgen-cli --version 0.2.108 --locked --root DIR
    python3 -I tools/build_wasm.py --out OUT --wasm-bindgen DIR/bin/wasm-bindgen \\
        [--target-dir DIR] [--profile release] [--no-lua] [--no-s102] \\
        [--sample FILE_OR_DIR ...] [--precomputed DIR]
    python3 -m http.server -d OUT 8000      # open http://localhost:8000/

The build portrays S-101 with the catalogue's Lua rules in the browser
(feature lua-wasm: Lua 5.4 compiled to wasm32 with clang; set FERRITE_WASM_CC
and FERRITE_WASM_AR when clang/llvm-ar with the WebAssembly target are not
found, see crates/ferrite-lua-runtime/build.rs). --no-lua builds without it:
S-101 cells then draw only from precomputed payloads. S-102 bathymetry
(feature s102-pure, the pure-Rust HDF5 reader) is built unless --no-s102.

The page runs the engine in a Web Worker drawing into an OffscreenCanvas
(worker.js, host.js), falling back to the main thread when the browser
cannot render from workers (engine-client.js); ?worker=off forces the main
thread. Portrayal cache entries persist in IndexedDB (cache-db.js): per-cell
Lua caches, S-102 rule instructions and dropped precomputed payloads.
Page parameters for checks: ?simulate=no-webgpu,no-offscreen,no-worker-gpu,
worker-gpu-fail (fallback paths), ?residency=N (residency threshold),
?viewreuse=0 (no view-only emission reuse), ?cachebudget=MB.

--sample copies datasets to OUT/samples (listed in samples.json, loaded by
the page's "Load samples" button); --precomputed copies the .ferrite_pre_*
payloads written by `ferrite precompute` (without Lua they are required for
S-101 cells; with Lua they replace the portrayal of the cells they cover).
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CRATE = REPO / "crates" / "ferrite-wasm"
PRODUCTS = ["S-101", "S-102", "S-124", "S-129"]
WASM_BINDGEN_VERSION = "0.2.108"


def bundle(catalogues: Path) -> bytes:
    out = bytearray(b"FSB1")
    for kind in ("FC", "PC"):
        for product in PRODUCTS:
            root = catalogues / kind / product
            if not root.is_dir():
                continue
            for path in sorted(p for p in root.rglob("*") if p.is_file()):
                name = path.relative_to(catalogues).as_posix().encode()
                data = path.read_bytes()
                out += struct.pack("<I", len(name)) + name
                out += struct.pack("<I", len(data)) + data
    return bytes(out)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--wasm-bindgen", default="wasm-bindgen")
    parser.add_argument(
        "--target-dir",
        type=Path,
        default=Path(tempfile.gettempdir()) / "ferrite-wasm-target",
    )
    parser.add_argument("--profile", default="release")
    parser.add_argument("--catalogues", type=Path, default=REPO / "Catalogues")
    parser.add_argument("--sample", action="append", type=Path, default=[])
    parser.add_argument("--precomputed", type=Path)
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--no-lua", action="store_true",
                        help="build without Lua portrayal (precomputed S-101 payloads only)")
    parser.add_argument("--no-s102", action="store_true",
                        help="build without S-102 bathymetry (feature s102-pure)")
    args = parser.parse_args()

    out = args.out.resolve()
    if out == REPO or REPO in out.parents:
        print("--out must be outside the repository", file=sys.stderr)
        return 2
    version = subprocess.run(
        [args.wasm_bindgen, "--version"], capture_output=True, text=True, check=True
    ).stdout.split()[-1]
    if version != WASM_BINDGEN_VERSION:
        print(f"wasm-bindgen {version}; need {WASM_BINDGEN_VERSION}", file=sys.stderr)
        return 2

    env = dict(os.environ, CARGO_TARGET_DIR=str(args.target_dir))
    if not args.skip_build:
        subprocess.run(
            ["cargo", "build", "-p", "ferrite-wasm", "--target", "wasm32-unknown-unknown",
             "--profile", args.profile,
             *(["--no-default-features", "--features", "s124,s129"] if args.no_s102 else []),
             *([] if args.no_lua else ["--features", "lua-wasm"])],
            cwd=REPO, env=env, check=True,
        )
    profile_dir = "debug" if args.profile == "dev" else args.profile
    wasm = args.target_dir / "wasm32-unknown-unknown" / profile_dir / "ferrite_wasm.wasm"

    out.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [args.wasm_bindgen, "--target", "web", "--no-typescript", "--out-dir",
         str(out / "pkg"), str(wasm)],
        check=True,
    )
    for path in (CRATE / "www").iterdir():
        if path.is_file():
            shutil.copy2(path, out / path.name)
    (out / "catalogues.bin").write_bytes(bundle(args.catalogues))

    samples = out / "samples"
    shutil.rmtree(samples, ignore_errors=True)
    listed = []
    for source in args.sample:
        files = [source] if source.is_file() else sorted(p for p in source.rglob("*") if p.is_file())
        for path in files:
            # Exchange-set members keep their relative names (update chains,
            # CATALOG.XML); separators are normalized to "/".
            name = (path.name if source.is_file()
                    else (Path(source.name) / path.relative_to(source)).as_posix())
            name = name.replace("\\", "/")
            target = samples / "data" / f"{len(listed):04}"
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
            listed.append({"name": name, "url": target.relative_to(out).as_posix(), "kind": "data"})
    if args.precomputed:
        for path in sorted(args.precomputed.glob(".ferrite_pre_*")):
            target = samples / "precomputed" / path.name.lstrip(".")
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
            listed.append({"name": path.name, "url": target.relative_to(out).as_posix(),
                           "kind": "precomputed"})
    (out / "samples.json").write_text(json.dumps(listed, indent=1))
    size = (out / "pkg" / "ferrite_wasm_bg.wasm").stat().st_size
    print(f"{out}: ferrite_wasm_bg.wasm {size / 1e6:.1f} MB, {len(listed)} sample file(s)")
    print(f"serve: python3 -m http.server -d {out} 8000")
    return 0


if __name__ == "__main__":
    sys.exit(main())
