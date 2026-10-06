#!/usr/bin/env python3
"""Portable, bounded-parallel Cargo build with an explicit Lua backend."""
import argparse
import os
from pathlib import Path
import json
import subprocess
from build_content_guard import guarded_build


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=("dev", "release-fast", "release"), default="dev")
    parser.add_argument("--lua", choices=("54", "55"), default="54")
    parser.add_argument("--jobs", type=int, default=min(4, os.cpu_count() or 1))
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    if args.jobs < 1:
        parser.error("--jobs must be positive")
    root = Path(__file__).resolve().parents[1]
    command = ["cargo", "build", "--locked", "--bin", "ferrite-s100",
               "--profile", args.profile, "--no-default-features",
               "--features", "lua" + args.lua, "--jobs", str(args.jobs), "--timings"]
    if args.offline:
        command.append("--offline")
    # Resolve Cargo's actual target, including environment and ancestor/global
    # config. Metadata resolves the locked local graph but does not compile.
    metadata = subprocess.run(["cargo", "metadata", "--no-deps", "--offline", "--locked",
                               "--format-version", "1"], cwd=root, check=True,
                              capture_output=True, text=True)
    target = Path(json.loads(metadata.stdout)["target_directory"]).resolve()
    command.extend(["--target-dir", str(target)])
    print(" ".join(command), flush=True)
    return guarded_build(root, target, command)


if __name__ == "__main__":
    raise SystemExit(main())
