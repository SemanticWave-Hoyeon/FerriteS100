#!/usr/bin/env python3
"""Build that leaves only the application in target/release.

Compiles into target/package-build, then replaces target/release with just
the application:

  macOS    target/release/FerriteS100.app   (Catalogues, Trust and icons in
                                            Contents/Resources)
  Windows  target/release/ferrite-s100.exe  (icon embedded; Catalogues and
                                            Trust are found in the checkout)
  Linux    target/release/ferrite-s100

target/package-build is the compiler cache: it is kept, so later builds only
recompile what changed. Options:

  --fast         release-fast profile (incremental, per-crate LTO): quick
                 rebuilds for testing; the default `release` profile (full
                 LTO) is the one to distribute
  --clean-cache  remove target/package-build afterwards (next build starts
                 from scratch)
"""
import argparse
import os
import plistlib
import platform
import shutil
import subprocess
import tempfile
from pathlib import Path

APP_NAME = "FerriteS100"
BINARY = "ferrite-s100"
BUNDLE_ID = "org.semanticwave.ferrite-s100"
# Runtime folders the application resolves from its base directory.
RESOURCES = ("Catalogues", "Trust", "icon.ico")


def version(root):
    for line in (root / "Cargo.toml").read_text(encoding="utf-8").splitlines():
        if line.startswith("version = "):
            return line.split('"')[1]
    raise RuntimeError("Workspace version not found in Cargo.toml")


def icns(png, out):
    """Apple icon set from the 1024 px PNG (sips + iconutil, both built in)."""
    with tempfile.TemporaryDirectory() as temp:
        iconset = Path(temp, "icon.iconset")
        iconset.mkdir()
        for size in (16, 32, 128, 256, 512):
            for scale in (1, 2):
                pixels = size * scale
                name = f"icon_{size}x{size}{'@2x' if scale == 2 else ''}.png"
                subprocess.run(["sips", "-z", str(pixels), str(pixels), str(png),
                                "--out", str(iconset / name)],
                               check=True, capture_output=True)
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(out)], check=True)


def mac_bundle(root, binary, out):
    contents = out / f"{APP_NAME}.app" / "Contents"
    (contents / "MacOS").mkdir(parents=True)
    resources = contents / "Resources"
    resources.mkdir()
    shutil.copy2(binary, contents / "MacOS" / BINARY)
    for name in RESOURCES:
        source = root / name
        if source.is_dir():
            shutil.copytree(source, resources / name, symlinks=False)
        elif source.is_file():
            shutil.copy2(source, resources / name)
    icns(root / "icon.png", resources / f"{APP_NAME}.icns")
    with open(contents / "Info.plist", "wb") as plist:
        plistlib.dump({
            "CFBundleName": APP_NAME,
            "CFBundleDisplayName": APP_NAME,
            "CFBundleIdentifier": BUNDLE_ID,
            "CFBundleExecutable": BINARY,
            "CFBundleIconFile": APP_NAME,
            "CFBundlePackageType": "APPL",
            "CFBundleVersion": version(root),
            "CFBundleShortVersionString": version(root),
            "CFBundleInfoDictionaryVersion": "6.0",
            "LSMinimumSystemVersion": "11.0",
            "NSHighResolutionCapable": True,
        }, plist)
    return contents.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--lua", choices=("54", "55"), default="54")
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 1)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--fast", action="store_true",
                        help="release-fast profile for quick test builds")
    parser.add_argument("--clean-cache", action="store_true",
                        help="remove target/package-build afterwards")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    build = root / "target" / "package-build"
    out = root / "target" / "release"
    profile = "release-fast" if args.fast else "release"
    command = ["cargo", "build", "--locked", "--bin", BINARY, "--profile", profile,
               "--no-default-features", "--features", "lua" + args.lua,
               "--jobs", str(args.jobs), "--target-dir", str(build)]
    if args.offline:
        command.append("--offline")
    print(" ".join(command), flush=True)
    status = subprocess.call(command, cwd=root)
    if status != 0:
        return status
    executable = BINARY + (".exe" if os.name == "nt" else "")
    binary = build / profile / executable
    # Empty the folder rather than removing it: a Finder window open on it
    # recreates .DS_Store while the tree is deleted, which fails rmtree.
    out.mkdir(parents=True, exist_ok=True)
    for entry in out.iterdir():
        if entry.is_dir() and not entry.is_symlink():
            shutil.rmtree(entry)
        else:
            entry.unlink()
    if platform.system() == "Darwin":
        result = mac_bundle(root, binary, out)
    else:
        result = out / executable
        shutil.copy2(binary, result)
    if args.clean_cache:
        shutil.rmtree(build)
    print(f"Packaged {result}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
