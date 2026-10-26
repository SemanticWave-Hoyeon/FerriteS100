#!/usr/bin/env python3
"""S-101 pixel regression harness.

Builds a base revision (git worktree) and the working tree, renders the fixed
poses in tools/pixel_regression_poses.json with both binaries in the hidden
background-test mode, and compares the screenshots pixel by pixel.

Build output and screenshots live outside the repository (default: the
system temporary directory), so no target/debug or stray files are created.

    python3 tools/pixel_regression.py                  # HEAD vs working tree
    python3 tools/pixel_regression.py --base 3a72f5d   # a commit vs working tree
    python3 tools/pixel_regression.py --skip-build     # reuse previous builds
    python3 tools/pixel_regression.py --poses overview,jersey_z20

Exit status 0 when every pose is identical, 1 on any difference, 2 on errors.
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
import time
import zlib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PROFILE = "release-fast"
BIN = "ferrite-s100" + (".exe" if os.name == "nt" else "")
RUN_TIMEOUT = 300


# --------------------------------------------------------------------------
# PNG (standard library only)
# --------------------------------------------------------------------------

def png_rgba(path: Path) -> tuple[int, int, bytes]:
    """Decode an 8-bit RGB/RGBA non-interlaced PNG to RGBA bytes."""
    data = path.read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path} is not a PNG")
    pos, idat, ihdr = 8, [], None
    while pos < len(data):
        length, kind = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            ihdr = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            idat.append(body)
        elif kind == b"IEND":
            break
    width, height, depth, colour, _, _, interlace = ihdr
    if depth != 8 or colour not in (2, 6) or interlace:
        raise ValueError(f"{path}: unsupported PNG format")
    bpp = 4 if colour == 6 else 3
    stride = width * bpp
    raw = zlib.decompress(b"".join(idat))
    out = bytearray(height * stride)
    prev = bytearray(stride)
    for y in range(height):
        f = raw[y * (stride + 1)]
        line = bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        if f == 1:
            for i in range(bpp, stride):
                line[i] = (line[i] + line[i - bpp]) & 0xFF
        elif f == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif f == 3:
            for i in range(stride):
                left = line[i - bpp] if i >= bpp else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif f == 4:
            for i in range(stride):
                a = line[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[i] = (line[i] + pred) & 0xFF
        out[y * stride:(y + 1) * stride] = line
        prev = line
    if bpp == 3:
        rgba = bytearray(width * height * 4)
        rgba[0::4], rgba[1::4], rgba[2::4] = out[0::3], out[1::3], out[2::3]
        rgba[3::4] = b"\xff" * (width * height)
        out = rgba
    return width, height, bytes(out)


def write_png(path: Path, width: int, height: int, rgba: bytes) -> None:
    rows = b"".join(b"\x00" + rgba[y * width * 4:(y + 1) * width * 4] for y in range(height))

    def chunk(kind: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rows, 6))
        + chunk(b"IEND", b"")
    )


def compare(a: Path, b: Path, diff: Path | None = None) -> dict:
    """Pixel comparison of two PNGs over the whole RGBA buffer."""
    if a.read_bytes() == b.read_bytes():
        return {"identical": True, "pixels": 0}
    wa, ha, pa = png_rgba(a)
    wb, hb, pb = png_rgba(b)
    if (wa, ha) != (wb, hb):
        return {"identical": False, "size": [[wa, ha], [wb, hb]]}
    if pa == pb:
        return {"identical": True, "pixels": 0}
    changed, box = 0, [wa, ha, -1, -1]
    marks = bytearray(len(pa))
    for i in range(0, len(pa), 4):
        if pa[i:i + 4] != pb[i:i + 4]:
            changed += 1
            x, y = (i // 4) % wa, (i // 4) // wa
            box = [min(box[0], x), min(box[1], y), max(box[2], x), max(box[3], y)]
            marks[i:i + 4] = b"\xff\x00\x00\xff"
        else:
            g = (pa[i] + pa[i + 1] + pa[i + 2]) // 6 + 64
            marks[i:i + 4] = bytes((g, g, g, 255))
    if diff is not None:
        write_png(diff, wa, ha, bytes(marks))
    return {"identical": False, "pixels": changed, "bbox": box}


# --------------------------------------------------------------------------
# Trees, builds and catalogue variants
# --------------------------------------------------------------------------

def git(*args: str, cwd: Path = REPO) -> str:
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def prepare_base_tree(out: Path, ref: str) -> Path:
    tree = out / "base-src"
    commit = git("rev-parse", "--verify", f"{ref}^{{commit}}")
    if tree.exists():
        try:
            if git("rev-parse", "HEAD", cwd=tree) == commit:
                return tree
        except subprocess.CalledProcessError:
            pass
        subprocess.run(["git", "worktree", "remove", "--force", str(tree)], cwd=REPO, check=False)
        shutil.rmtree(tree, ignore_errors=True)
    subprocess.run(["git", "worktree", "prune"], cwd=REPO, check=True)
    git("worktree", "add", "--detach", str(tree), commit)
    return tree


def build(tree: Path, target: Path) -> Path:
    print(f"build {tree} -> {target}", flush=True)
    subprocess.run(
        ["cargo", "build", "--profile", PROFILE, "--bin", "ferrite-s100", "--target-dir", str(target)],
        cwd=tree,
        check=True,
    )
    return target / PROFILE / BIN


def swap_palettes(root: Path) -> None:
    """Make 'Day' (the default profile) carry the Night palette."""
    for xml in root.rglob("*"):
        if xml.is_file() and xml.name.lower().startswith("colorprofile") and xml.suffix.lower() == ".xml":
            text = xml.read_text(encoding="utf-8-sig")
            text = (
                text.replace('name="Day"', 'name="\x00"')
                .replace('name="Night"', 'name="Day"')
                .replace('name="\x00"', 'name="Night"')
            )
            xml.write_text(text, encoding="utf-8")


def night_inventory(out: Path, inventory: Path) -> Path:
    dest = out / "night-inventory"
    if not dest.exists():
        shutil.copytree(inventory, dest, symlinks=True)
        manifest = dest / "manifest.json"
        if manifest.exists():
            manifest.write_text(
                manifest.read_text(encoding="utf-8").replace(str(inventory), str(dest)),
                encoding="utf-8",
            )
        swap_palettes(dest)
    return dest


def run_base(out: Path, side: str, tree: Path, palette: str) -> Path:
    """Working directory whose Catalogues/ the binary resolves (exe dir has none)."""
    base = out / "run" / side / palette
    catalogues = base / "Catalogues"
    if catalogues.is_symlink() or catalogues.exists():
        return base
    base.mkdir(parents=True, exist_ok=True)
    if palette == "Night":
        shutil.copytree(tree / "Catalogues", catalogues)
        swap_palettes(catalogues)
    else:
        catalogues.symlink_to(tree / "Catalogues", target_is_directory=True)
    return base


def render(exe: Path, cwd: Path, args: list[str], png: Path, state: Path) -> float:
    env = dict(os.environ, FERRITE_BACKGROUND_TEST="1", FERRITE_NAVWARN_STATE_PATH=str(state))
    png.unlink(missing_ok=True)
    log = png.with_suffix(".log")
    started = time.monotonic()
    with log.open("w") as handle:
        proc = subprocess.run(
            [str(exe), *args, "--screenshot", str(png)],
            cwd=cwd, env=env, stdout=handle, stderr=subprocess.STDOUT, timeout=RUN_TIMEOUT,
        )
    if proc.returncode != 0 or not png.exists():
        raise RuntimeError(f"render failed ({proc.returncode}): see {log}")
    return time.monotonic() - started


# --------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", default="HEAD", help="base revision (default HEAD)")
    parser.add_argument("--out", type=Path, default=Path(tempfile.gettempdir()) / "ferrite-pixel-regression")
    parser.add_argument("--poses", help="comma-separated pose names (default: all)")
    parser.add_argument("--skip-build", action="store_true", help="reuse existing binaries")
    parser.add_argument("--remove-worktree", action="store_true", help="remove the base worktree afterwards")
    args = parser.parse_args()

    spec = json.loads((REPO / "tools" / "pixel_regression_poses.json").read_text(encoding="utf-8"))
    inventory = (REPO / spec["inventory"]).resolve()
    if not inventory.is_dir():
        print(f"Catalogue inventory not found: {inventory}", file=sys.stderr)
        return 2
    wanted = set(args.poses.split(",")) if args.poses else None
    poses = [p for p in spec["poses"] if wanted is None or p["name"] in wanted]
    if not poses:
        print("No poses selected", file=sys.stderr)
        return 2
    for name, dataset in spec["datasets"].items():
        missing = [c for c in dataset["charts"] if not (REPO / c).exists()]
        if missing and any(p["dataset"] == name for p in poses):
            print(f"Test data for '{name}' not found: {missing}", file=sys.stderr)
            return 2

    out = args.out.resolve()
    shots = out / "shots"
    shots.mkdir(parents=True, exist_ok=True)
    trees = {"base": prepare_base_tree(out, args.base), "work": REPO}
    exes = {}
    for side, tree in trees.items():
        exe = out / f"{side}-target" / PROFILE / BIN
        exes[side] = exe if args.skip_build and exe.exists() else build(tree, out / f"{side}-target")

    results, failures = [], 0
    for pose in poses:
        palette = pose.get("palette", "Day")
        dataset = spec["datasets"][pose["dataset"]]
        inv = night_inventory(out, inventory) if palette == "Night" else inventory
        cli = ["--catalogue-inventory", str(inv)]
        for chart in dataset["charts"]:
            cli += ["--chart", str((REPO / chart).resolve())]
        for key in ("fc", "pc"):
            if key in dataset:
                cli += [f"--{key}", dataset[key].replace("{inventory}", str(inv))]
        if "center" in pose:
            cli += ["--center", pose["center"]]
        if "zoom" in pose:
            cli += ["--zoom", str(pose["zoom"])]
        runs = ["cold", "warm"] if pose.get("cache") else ["cold"]
        images: dict[tuple[str, str], Path] = {}
        for side in trees:
            cache = exes[side].parent / "cache"
            shutil.rmtree(cache, ignore_errors=True)
            cwd = run_base(out, side, trees[side], palette)
            for run in runs:
                png = shots / f"{side}-{pose['name']}-{run}.png"
                seconds = render(exes[side], cwd, cli, png, out / f"navwarn-{side}.json")
                images[(side, run)] = png
                print(f"  {side:4} {pose['name']:28} {run:4} {seconds:5.1f}s", flush=True)
        for run in runs:
            diff = shots / f"diff-{pose['name']}-{run}.png"
            outcome = compare(images[("base", run)], images[("work", run)], diff)
            results.append({"pose": pose["name"], "run": run, **outcome})
            failures += not outcome["identical"]
            print(f"{'IDENTICAL' if outcome['identical'] else 'DIFFERENT'} {pose['name']} ({run})"
                  + ("" if outcome["identical"] else f": {outcome}"), flush=True)
        if pose.get("cache"):
            within = compare(images[("work", "cold")], images[("work", "warm")])
            results.append({"pose": pose["name"], "run": "work cold vs warm", **within})
            failures += not within["identical"]

    # The comparator and the palette variant must be able to see a difference.
    day, night = shots / "work-portsmouth_z12-cold.png", shots / "work-portsmouth_z12_night-cold.png"
    if day.exists() and night.exists():
        sanity = compare(day, night)
        results.append({"pose": "sanity day vs night", **sanity})
        if sanity["identical"]:
            print("SANITY FAILED: Day and Night renders are identical", file=sys.stderr)
            failures += 1

    report = out / "report.json"
    report.write_text(json.dumps({"base": args.base, "results": results}, indent=2), encoding="utf-8")
    if args.remove_worktree:
        subprocess.run(["git", "worktree", "remove", "--force", str(trees["base"])], cwd=REPO, check=False)
    print(f"{len(results)} comparisons, {failures} failing. Report: {report}")
    return 1 if failures else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (subprocess.CalledProcessError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(2)
