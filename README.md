<p align="center">
  <img src="icon.png" alt="FerriteS100 Logo" width="160" height="160">
</p>

<h1 align="center">FerriteS100</h1>

<p align="center"><strong>A GPU-accelerated S-100 chart viewer written in Rust</strong></p>

<p align="center">
  <a href="https://github.com/SemanticWave-Hoyeon/FerriteS100/actions/workflows/rust.yml"><img src="https://github.com/SemanticWave-Hoyeon/FerriteS100/actions/workflows/rust.yml/badge.svg" alt="Rust CI"></a>
  <img src="https://img.shields.io/badge/Platform-Windows%20%7C%20Linux%20%7C%20macOS-lightgrey" alt="Platform">
  <img src="https://img.shields.io/badge/GPU-Vulkan%20%7C%20DX12%20%7C%20Metal-green" alt="GPU">
  <img src="https://img.shields.io/badge/License-PolyForm%20NC-blue" alt="License">
</p>

FerriteS100 loads IHO S-100 datasets, runs the catalogue-supplied Lua portrayal
rules and draws a flat 2D chart with wgpu and egui. It currently handles S-101
electronic navigational charts, S-102 bathymetric surfaces and S-421 routes.

> **Not for navigation.** This is development and research software, not a
> certified ECDIS. Passing tests do not establish complete S-100, product
> specification or S-98 conformance.

<p align="center"><img src="Screenshot.png" alt="FerriteS100 with SHOM S-101 data, catalogue tree and object details" width="900"></p>

## Features

| Product | Support |
|---------|---------|
| **S-101** ENC | ISO 8211 base cells and update chains, multiple cells and product editions in one display list, FC/PC-driven Lua portrayal, overscale indication, source-aware object inspection |
| **S-102** bathymetry | 3.0.0 HDF5 reader with bounded window reads, depth/uncertainty and vertical-reference metadata, independent portrayal catalogue. EPSG:4326 grids are displayed; UTM/UPS grids are read but not yet drawn |
| **S-421** routes | Bounded GML subset import (published 1.0 and a CDV 2.0 subset), route/waypoint inspection, WGS84 rhumb/geodesic legs, minimal published 1.0 export |

Common to all products:

- **Rendering** — GPU points, lines, areas, text and SVG symbols; Day/Dusk/Night palettes; pan, zoom and screenshot export.
- **Datasets** — open a file, a `CATALOG.XML` or a whole folder; exchange sets are discovered recursively and loaded in a worker. Unsupported or damaged inputs are reported without blocking the others.
- **Catalogues** — FC/PC pairs are selected per product edition from a local inventory; a newer catalogue never silently converts an older dataset.
- **Security** — optional S-100 Part 15 signature verification, an `--operational` profile that requires authenticated data, and a sandboxed Lua runtime.
- **S-100 MCP** — a built-in, read-only [Model Context Protocol](docs/S100-MCP.md) server that lets an LLM client query loaded datasets.

## Build

Requirements: a current stable Rust toolchain (verified with Rust 1.99), platform C/C++
build tools, and a GPU driver with Metal, Vulkan or DirectX 12. Python 3 is used
only by the build helper.

```bash
git clone https://github.com/SemanticWave-Hoyeon/FerriteS100.git
cd FerriteS100
python3 tools/build.py --profile release-fast --lua 54
```

On Windows use `py -3` instead of `python3`.

| Profile | Executable | Use |
|---------|------------|-----|
| `dev` | `target/debug/ferrite-s100` | Debugging |
| `release-fast` | `target/release-fast/ferrite-s100` | Day-to-day optimized builds (incremental) |
| `release` | `target/release/ferrite-s100` | Distribution (full LTO) |

The Lua backend is chosen at build time: Lua 5.4 by default, or `--lua 55`.
Exactly one backend must be enabled, so `--all-features` is not supported. A
plain Cargo build is also possible:

```bash
cargo build --locked --profile release-fast --bin ferrite-s100 --no-default-features --features lua54
```

See [BUILDING.md](BUILDING.md) for profiles, input provenance and S-102 storage details.

### macOS app bundle

```bash
python3 tools/package_macos.py --binary target/release-fast/ferrite-s100 --inventory ../S101-Catalogues --output target/macos/FerriteS100.app
```

The bundle contains the executable, icon, catalogues and trust resources, and
is ad hoc signed (not notarized). Existing bundles are never overwritten.

## Usage

Start the application and use **File → Open Dataset** or **Open Dataset Folder**.
Datasets can also be given on the command line:

```bash
./target/release-fast/ferrite-s100 --chart /path/to/S101-exchange-set
./target/release-fast/ferrite-s100 --chart /path/to/cell.000 --fc /path/to/FC --pc /path/to/PC
./target/release-fast/ferrite-s100 --s102 /path/to/S102-exchange-set
./target/release-fast/ferrite-s100 --s421 /path/to/route.gml
```

| Option | Purpose |
|--------|---------|
| `--chart`, `--s102`, `--s421` | Load S-101, S-102 or S-421 input (file or folder) |
| `--fc`, `--pc`, `--s102-pc` | Use specific catalogues |
| `--catalogue-inventory <dir>` | Local multi-version catalogue inventory (default: `S101-Catalogues` beside the resources) |
| `--center <lat,lon>`, `--zoom <n>` | Initial view |
| `--require-signatures` | Reject unsigned datasets |
| `--operational` | Require authenticated datasets for the whole session |
| `--screenshot <file>` | Render, save an image and exit |

| Input | Action |
|-------|--------|
| Scroll | Zoom |
| Left drag | Pan |
| Left click | Inspect objects (when Object selection mode is on) |
| Esc | Clear the current selection |
| Right click | Reset view |
| F12 | Toggle debug statistics |

The **Data Layers** panel groups loaded catalogues and chart files by product,
where each dataset can be selected or unloaded. **Logs** and **Load status**
open in separate windows.

## Signatures and security

Signature verification is **off by default** so evaluation data can be opened.
Turn it on in Settings or with `--require-signatures`; `--operational` enforces
it for the whole session. Verification uses ECDSA P-384/SHA-384 against locally
installed X.509 trust anchors (`Trust/`, not included in the repository).

Lua portrayal runs without the OS, IO and Debug libraries, with file access
confined to the catalogue and per-operation memory/instruction budgets. This is
not process isolation. Native plugins must be signed in release builds.

`ferrite-secom` contains a SECOM receive client under development; it is not an
IEC 63173-2 conformance claim.

## Performance

The renderer caches decoded cells (RAM LRU, 96 MiB), portrayal instructions,
triangulations and static line relations, and coalesces navigation events so
that redundant scene preparation is merged before each frame. Scene rebuilds
after pan and zoom run on a background thread. `FERRITE_NO_CACHE=1` disables the
decoded-cell and instruction caches.

Several experimental optimizations are off by default:
`FERRITE_STATIC_LINE_BOUNDS_CSE=1`, `FERRITE_RETAINED_WORLD_AREAS=1`,
`FERRITE_MOVING_LINE_NORTHING_REUSE=1` and
`FERRITE_LINE_SUPPRESSION_UNCHANGED_TARGETS=1`. `FERRITE_GPU_FRAME_TIMESTAMPS=1`
records GPU timestamps for the chart pass in hidden audits.

## Development

```bash
cargo fmt -- --check
cargo clippy -- -D warnings
cargo test --locked
```

CI builds and tests on macOS, Linux and Windows, plus formatting and Clippy jobs.
Test datasets belong in the git-ignored `TestData/` folder. Useful sources:

- [IHO S-100 resources](https://iho-ohi.github.io/S100Resources/) — specifications and catalogues
- [UKHO trial datasets](https://datahub.admiralty.co.uk/portal/home/item.html?id=6966cb7ce9454ccf9afbbd3c9a105f9e)
- [SHOM](https://data.shom.fr/) — public hydrographic data
- [BSH S-102 test data](https://linchart60.bsh.de/chartserver/)
- [NOAA marine navigation developer resources](https://marinenavigation.noaa.gov/developer.html)

```text
src/                         Application, loading and UI orchestration
crates/ferrite-s101/          S-101 product adapter
crates/ferrite-s102/          S-102 HDF5 adapter
crates/ferrite-s421/          S-421 route adapter
crates/ferrite-kernel/        Product-independent geometry and geodesy
crates/ferrite-lua/           S-100 Lua host and portrayal rules
crates/ferrite-lua-runtime/   Build-time Lua interpreter selection
crates/ferrite-render/        Rendering instructions and geometry
crates/ferrite-wgpu/          GPU renderer and egui integration
crates/ferrite-mcp/           Built-in S-100 MCP service
crates/ferrite-security/      Exchange-set authentication
crates/ferrite-secom/         SECOM receive client
Catalogues/                   Bundled feature and portrayal catalogues
tools/                        Build, packaging and provenance helpers
```

## License

[PolyForm Noncommercial License 1.0.0](LICENSE).

Developed by [Hoyeon Cho](https://github.com/SemanticWave-Hoyeon) at Korea Maritime
and Ocean University, with standards and catalogue resources from the IHO.
