<p align="center">
  <img src="icon.png" alt="FerriteS100 Logo" width="160" height="160">
</p>

<h1 align="center">FerriteS100</h1>

<p align="center"><strong>A Rust viewer for S-101 electronic navigational charts and S-102 bathymetry</strong></p>

<p align="center">
  <a href="https://github.com/SemanticWave-Hoyeon/FerriteS100/actions/workflows/rust.yml"><img src="https://github.com/SemanticWave-Hoyeon/FerriteS100/actions/workflows/rust.yml/badge.svg" alt="Rust CI"></a>
  <img src="https://img.shields.io/badge/Platform-Windows%20%7C%20Linux%20%7C%20macOS-lightgrey" alt="Platform">
  <img src="https://img.shields.io/badge/GPU-Vulkan%20%7C%20DX12%20%7C%20Metal-green" alt="GPU">
  <img src="https://img.shields.io/badge/License-PolyForm%20NC-blue" alt="License">
</p>

FerriteS100 reads marine datasets, executes catalogue-driven Lua portrayal rules,
and renders a flat 2D chart using wgpu and egui. Feature inspection retains source
cell identity and attributes; bathymetry inspection includes depth, uncertainty
and vertical reference metadata. The spherical 3D view has been removed.

This is development and research software. It is not a certified ECDIS or a
navigation system. Implemented readers and passing tests do not establish complete
S-100, product specification or S-98 conformance.

## Screenshot

<p align="center"><img src="Screenshot.png" alt="S-101 chart in FerriteS100" width="900"></p>

## Current capabilities

| Area | Implementation and limits |
|------|---------------------------|
| S-101 | ISO 8211 base cells and update-chain loading, multiple chart cells, FC/PC-driven Lua portrayal and source-aware inspection. Dataset and catalogue versions must be compatible. |
| S-102 | A 3.0.0 HDF5 reader with bounded window reads, depth/uncertainty preservation and an independent portrayal catalogue. Current map display accepts EPSG:4326 within device texture limits. |
| Catalogues | Load FC, PC or an FC/PC pair from the File menu. S-101 loading can select a compatible pair from the local multi-version inventory. Product editions are checked; choosing a newer catalogue does not convert an older dataset. |
| Rendering | GPU point, line, area, text and SVG symbol rendering; Day/Dusk/Night settings; flat pan and zoom; screenshot export. |
| Inspection | Source chart, feature attributes and overlapping-object selection. Polygon holes are excluded from area interior picks. |
| Debug UI | The toolbar **DEBUG MODE** button (F12) enables performance statistics and profiling. |
| Security | Optional S-100 dataset signature checking; signed plugin manifests; a separate SECOM receive client under development. |

S-102 native-coordinate reading supports additional WGS84 UTM/UPS CRS codes,
but projected and polygon-domain data are not yet supported by the normal map
renderer. Mixed vertical references require explicit supported transformations;
no datum offset is guessed. See [BUILDING.md](BUILDING.md) for the current
S-102 storage, values-group and datum-adjustment contracts.

S-102 2.1 files are currently rejected by the 3.0 adapter. The old UKHO Complete
S10X trial package contains S-101 1.0 and S-102 2.1 data. Its S-101 cells require a
compatible catalogue pair rather than the bundled 2.0 pair; automatic selection
requires that pair to be present in the local catalogue inventory. Multiple S-101 product
editions cannot currently share one global active FC/PC pair. Unsupported product
files are not made compatible by changing a version string or treating every HDF5
file as S-102.

`ISODGR01`, a magenta isolated underwater danger symbol, is supplied by the
portrayal catalogue. A pink cross is not by itself evidence of a missing symbol.
The official rule and safety-contour context determine its portrayal.

## Build

Use a current stable Rust toolchain. The October 2026 CI repair was verified with
Rust 1.99; older compiler compatibility is not established. Install the platform
C/C++ build tools and a GPU driver supporting Metal, Vulkan or DirectX 12.
Python 3 is needed only for the portable build helper.

```bash
git clone https://github.com/SemanticWave-Hoyeon/FerriteS100.git
cd FerriteS100
rustup update stable
python3 tools/build.py --profile dev --lua 54
python3 tools/build.py --profile release-fast --lua 54
python3 tools/build.py --profile release --lua 54
```

On Windows, replace `python3` with `py -3`. The helper defaults to at most four
compilation jobs, preserves `Cargo.lock`, locks the selected target and verifies
source inputs around the build. Keep `target/` between builds to reuse artifacts.

| Profile | Executable | Intended use |
|---------|------------|--------------|
| `dev` | `target/debug/ferrite-s100` | Debug information and checks |
| `release-fast` | `target/release-fast/ferrite-s100` | Frequent optimized builds with incremental compilation |
| `release` | `target/release/ferrite-s100` | Distribution builds with full LTO |

Windows executables have the `.exe` suffix. Cargo configuration can change the
target directory. Build timings are saved under `target/cargo-timings/`.

Lua execution is isolated in the [ferrite-lua-runtime](crates/ferrite-lua-runtime/README.md)
package. Lua 5.4 is the default; select Lua 5.5 at build time with `--lua 55`.
Exactly one backend must be enabled. `--all-features` enables incompatible
backends and is intentionally unsupported. Changing Lua requires a rebuild and
catalogue compatibility validation.

For a direct Cargo build:

```bash
cargo build --locked --profile release-fast --bin ferrite-s100 --no-default-features --features lua54 --jobs 4 --timings
```

See [BUILDING.md](BUILDING.md) for build profiles and input provenance.

## Open and inspect data

Run the executable from the repository or retain the `Catalogues/` resource
layout alongside it. Downloaded datasets and local trust configuration are not
included in a Git clone.

<!-- DATASET_MENU_CURRENT_START -->
Use the File menu or toolbar:

- **Open Dataset** selects one dataset file. Selecting `CATALOG.XML` opens the
  exchange-set folder containing it.
- **Open Dataset Folder** searches the selected folder recursively, including
  nested exchange sets. It opens supported S-101 update chains and S-102 3.0
  files without asking you to choose one exchange set from a collection.

Discovery runs in a worker. HDF5 product metadata determines routing; an `.h5`
extension alone does not imply S-102. Unsupported products and editions, damaged
files and conflicting S-101 deliveries are reported. Other eligible datasets
continue loading; failures do not turn incompatible inputs into valid charts.
S-101 updates remain attached to their base cell, and duplicate deliveries are
checked for equivalent content before being merged.

The left **Data Layers** panel groups loaded files by product. Expand a group to
select a dataset or unload it. Unloading removes the in-memory dataset without
deleting its file or issuing a producer cancellation. The panel shows the actual
active S-101 FC/PC versions and paths. S-102 shows its PC path and embedded product
schema; it does not imply that an external S-102 FC was loaded.
<!-- DATASET_MENU_CURRENT_END -->

You can also start with explicit product paths and catalogue selections:

```bash
./target/release-fast/ferrite-s100 --chart /path/to/S101-exchange-set
./target/release-fast/ferrite-s100 --chart /path/to/cell.000 --fc /path/to/FC --pc /path/to/PC
./target/release-fast/ferrite-s100 --s102 /path/to/S102-exchange-set --s102-pc /path/to/S102-PC
```

`--chart` and `--s102` accept files or directories and can be used together.
Use `--catalogue-inventory /path/to/S101-Catalogues` to select a local version
inventory. The default inventory is `S101-Catalogues` beside the application
resource directory; downloaded versions are not included automatically in a
clone. Catalogue compatibility and supported data structure checks still apply.

| Input | Action |
|-------|--------|
| Scroll | Zoom |
| Left drag | Pan |
| Left click | Inspect an object |
| Right click | Reset view |
| F12 / DEBUG MODE | Toggle debug statistics |
| File → Save Screenshot | Export an image |

## Test datasets

Keep local datasets under a project-owned `TestData/` folder. Datasets, captures,
private signing material and compiled artifacts are excluded from Git. Neither
local launchers nor a developer's downloaded catalogue inventory should be
assumed to exist after cloning.

Useful source locations:

- [IHO S-100 resources](https://iho-ohi.github.io/S100Resources/): published product specifications and catalogue references.
- [UKHO trial dataset listing](https://datahub.admiralty.co.uk/portal/home/item.html?id=6966cb7ce9454ccf9afbbd3c9a105f9e): check the delivered product editions before selecting FC/PC.
- [SHOM](https://data.shom.fr/): public hydrographic data; local signed S-101 update chains have been used for regression comparisons.
- [BSH S-102 test data](https://linchart60.bsh.de/chartserver/): projected bathymetry test packages. Public availability does not establish compatibility with the current renderer.
- [NOAA marine navigation developer resources](https://marinenavigation.noaa.gov/developer.html): public S-102 data access.

A successful download is not a successful loading test. Preserve the delivered
bytes and report unsupported editions, products, domains and datum requirements.

## Signature verification and SECOM

**Dataset signature verification defaults to OFF.** Enable it using the Settings
toggle or start with `--require-signatures`. OFF allows unauthenticated evaluation
inputs; it does not mark them as verified or bypass product/schema compatibility
checks. Changing the toggle does not authenticate data already loaded.

When enabled, `ferrite-security` verifies S-100 Part 15 ECDSA P-384/SHA-384
exchange catalogue and resource signatures using separately installed X.509 trust
anchors. It checks certificate paths, validity and key usage, rejects escaping
paths and unlisted datasets, and provides private snapshots to product readers.
Signature failures block loading. Root runtime trust files are local configuration,
not automatically delivered by the repository.

Lua portrayal restricts libraries and file/module access to its catalogue
sandbox. Plugins are native dynamic libraries; signed manifests, integrity and
ABI checks do not turn native plugin code into an operating-system sandbox.

`ferrite-secom` implements bounded blocking HTTPS/mTLS reads and separate payload
certificate/signature authentication for an interoperability profile. Transport
trust and payload trust are distinct. Run it outside the UI thread. Received
exchange-set publication, producer cancellation authority, revocation, durable
recovery, acknowledgements and full service interoperability remain incomplete.
It is not an IEC 63173-2 conformance claim. Permit licensing and encryption are
also separate from dataset signature verification.

## Verification and performance

The Rust workflow builds and tests on macOS, Linux and Windows, with separate
formatting and warning-denying Clippy jobs. All five jobs passed for commit
`a4c6e7e` in [run 37563121273](https://github.com/SemanticWave-Hoyeon/FerriteS100/actions/runs/37563121273).
That verifies this source revision on CI; it does not establish native interaction
or performance on a user's Windows PC, nor the state of an older local executable.

```bash
cargo fmt -- --check
cargo clippy -- -D warnings
cargo test --locked
```

Optimization currently includes deterministic instruction ordering, bounded
instruction/source caches, exact triangulation and static line-relation reuse,
Mercator source-northing reuse and frame-local coverage upload reuse. Current
camera, visibility, coverage and source identity remain part of frame evaluation.
`FERRITE_NO_CACHE=1` disables instruction cache reads and writes for controls.

A newer event-coalescing candidate reduced median hidden handler time from
approximately 74 ms to 25 ms in the first controlled OFF/ON navigation pair;
20 sampled poses and final output matched exactly. This candidate is not yet
installed or qualified by a complete repeated experiment. Hidden synthetic
handler timings are not displayed FPS. Sustained foreground 60 FPS remains a
goal, and full physical input, DPI and Windows performance validation is pending.

## Repository layout

```text
src/                       Application, loading and UI orchestration
crates/ferrite-s101/        S-101 product adapter
crates/ferrite-s102/        S-102 HDF5 adapter
crates/ferrite-kernel/      Product-independent geometry and data contracts
crates/ferrite-lua/         S-100 Lua host and portrayal rules
crates/ferrite-lua-runtime/ Build-time interpreter selection
crates/ferrite-render/      Rendering instructions and geometry
crates/ferrite-wgpu/        GPU renderer and egui integration
crates/ferrite-security/    Exchange authentication
crates/ferrite-secom/       SECOM receive foundation
Catalogues/                Tracked catalogue resources
TestData/                  Local downloaded datasets (ignored)
Trust/                     Local trust configuration (ignored)
tools/                     Portable build and provenance helpers
```

## License

See [LICENSE](LICENSE) for the **PolyForm Noncommercial License 1.0.0** terms.

Developed by [Hoyeon Cho](https://github.com/SemanticWave-Hoyeon) at Korea Maritime
and Ocean University, with standards and catalogue resources from the IHO.
