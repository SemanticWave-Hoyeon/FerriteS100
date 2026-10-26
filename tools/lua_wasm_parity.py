#!/usr/bin/env python3
"""Lua-on-web parity: the S-101 Lua portrayal compiled to WebAssembly must
produce exactly the Part 9a output of the native (mlua) build.

Both halves run the portrayal catalogue's rules through ferrite-wasm's
WebEngineCore on the same dataset groups and write every HostPortrayalEmit
call (feature reference, drawing instructions, observed context parameters)
in emission order:

- native: `cargo run -p ferrite-wasm --features lua-wasm --example part9a_dump`
- wasm:   `node tools/lua_wasm_parity.mjs` on the ferrite-wasm build made by
          `tools/build_wasm.py` (lua-wasm is on there by default)

    python3 -I tools/build_wasm.py --out WWW --wasm-bindgen BIN ...
    python3 -I tools/lua_wasm_parity.py --pkg WWW/pkg --work DIR \\
        [--target-dir DIR] [PATH ...]

PATHs (default: TestData/SHOM-S101, TestData/UKHO-S101, TestData/UKHO-S100-2026,
TestData/IHO-S101-Test-Datasets) are searched for S-101 cells (101*.000). A
cell's group is its exchange set (the folder holding S100_ROOT), else the
folder holding its CATALOG.XML, else the files beside it; groups with the
same content are checked once. Exit status 0 only when every group matches.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_ROOTS = [
    "TestData/SHOM-S101",
    "TestData/UKHO-S101",
    "TestData/UKHO-S100-2026",
    "TestData/IHO-S101-Test-Datasets",
]


def group_root(cell: Path) -> tuple[Path, bool]:
    """(root, recursive) of the dataset group of `cell`."""
    for parent in cell.parents:
        if (parent / "S100_ROOT").is_dir():
            return parent, True
        if (parent / "CATALOG.XML").is_file() and parent != cell.parent:
            return parent, True
        if parent.name in ("TestData", "") or parent == REPO:
            break
    if (cell.parent / "CATALOG.XML").is_file():
        return cell.parent, True
    return cell.parent, False


def discover(paths: list[Path]) -> list[dict]:
    roots: dict[Path, bool] = {}
    for path in paths:
        cells = [path] if path.is_file() else sorted(path.rglob("101*.000"))
        for cell in cells:
            root, recursive = group_root(cell)
            roots[root] = roots.get(root, False) or recursive
    groups, seen = [], set()
    for root, recursive in sorted(roots.items()):
        files = sorted(p for p in (root.rglob("*") if recursive else root.iterdir()) if p.is_file())
        files = [p for p in files if p.suffix.lower() not in (".pdf", ".h5", ".hdf5", ".zip")]
        digest = hashlib.sha256()
        for path in files:
            digest.update(path.relative_to(root).as_posix().encode() + b"\0")
            digest.update(hashlib.sha256(path.read_bytes()).digest())
        if digest.digest() in seen:
            continue
        seen.add(digest.digest())
        name = root.relative_to(REPO).as_posix() if REPO in root.parents else str(root)
        groups.append({
            "name": name,
            # Names as the web demo gives them: the group folder and the
            # relative path (exchange-set layout, update chains).
            "files": [[(Path(root.name) / p.relative_to(root)).as_posix(), str(p)] for p in files],
        })
    return groups


def catalogue_files(catalogues: Path) -> list[list[str]]:
    files = []
    for kind in ("FC", "PC"):
        base = catalogues / kind / "S-101"
        for path in sorted(p for p in base.rglob("*") if p.is_file()):
            files.append([path.relative_to(catalogues).as_posix(), str(path)])
    return files


def blocks(text: str) -> dict[str, list[str]]:
    out: dict[str, list[str]] = {}
    current = None
    for line in text.split("\n"):
        if line.startswith("group\t"):
            current = line[6:]
            out[current] = []
        elif current is not None and line:
            out[current].append(line)
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--pkg", required=True, type=Path, help="wasm-bindgen output (WWW/pkg)")
    parser.add_argument("--work", type=Path, default=Path(tempfile.gettempdir()) / "ferrite-lua-parity")
    parser.add_argument("--target-dir", type=Path)
    parser.add_argument("--catalogues", type=Path, default=REPO / "Catalogues")
    parser.add_argument("--node", default="node")
    parser.add_argument("--skip-native", action="store_true", help="reuse WORK/native.txt")
    parser.add_argument("paths", nargs="*", type=Path)
    args = parser.parse_args()

    paths = [p.resolve() for p in args.paths] or [REPO / p for p in DEFAULT_ROOTS if (REPO / p).exists()]
    groups = discover(paths)
    if not groups:
        print("No S-101 cells found", file=sys.stderr)
        return 2
    work = args.work.resolve()
    if work == REPO or REPO in work.parents:
        print("--work must be outside the repository", file=sys.stderr)
        return 2
    work.mkdir(parents=True, exist_ok=True)
    manifest = work / "manifest.json"
    manifest.write_text(json.dumps({"catalogues": catalogue_files(args.catalogues), "groups": groups}))
    print(f"{len(groups)} dataset groups, "
          f"{sum(1 for g in groups for f in g['files'] if f[0].endswith('.000'))} base cells")

    native, wasm = work / "native.txt", work / "wasm.txt"
    if not args.skip_native:
        env = dict(os.environ)
        if args.target_dir:
            env["CARGO_TARGET_DIR"] = str(args.target_dir)
        subprocess.run(
            ["cargo", "run", "--release", "-p", "ferrite-wasm", "--features", "lua-wasm",
             "--example", "part9a_dump", "--", str(manifest), str(native)],
            cwd=REPO, env=env, check=True,
        )
    subprocess.run([args.node, str(REPO / "tools" / "lua_wasm_parity.mjs"), str(args.pkg.resolve()),
                    str(manifest), str(wasm)], check=True)

    left, right = blocks(native.read_text()), blocks(wasm.read_text())
    failed = 0
    total_cells = total_emissions = 0
    for group in groups:
        name = group["name"]
        a, b = left.get(name), right.get(name)
        cells = sum(1 for line in (a or []) if line.startswith("cell\t"))
        emissions = sum(1 for line in (a or []) if not line.startswith(("cell\t", "error\t", "open\t")))
        errors = [line for line in (a or []) if line.startswith("error\t")]
        errors += [line for line in (a or []) if line.startswith("open\t") and not line.endswith("\t")]
        total_cells += cells
        total_emissions += emissions
        if a is None or b is None or a != b:
            failed += 1
            detail = "missing output"
            if a is not None and b is not None:
                index = next((i for i, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
                detail = (f"first difference at line {index + 1}:\n"
                          f"    native: {a[index][:300] if index < len(a) else '<end>'}\n"
                          f"    wasm:   {b[index][:300] if index < len(b) else '<end>'}")
            print(f"DIFF  {name}: {detail}")
        else:
            note = f" (both: {errors[0][6:200]})" if errors else ""
            print(f"same  {name}: {cells} cells, {emissions} emissions{note}")
    print(f"\n{len(groups) - failed}/{len(groups)} groups identical; "
          f"{total_cells} cells, {total_emissions} Part 9a emissions compared")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
