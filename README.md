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

<p align="center"><img src="Screenshot.png" alt="FerriteS100 2.0.0 with SHOM S-101 data, catalogue tree and object details" width="900"></p>

## Current capabilities

| Area | Implementation and limits |
|------|---------------------------|
| S-101 | ISO 8211 base cells and update-chain loading, multiple chart cells, FC/PC-driven Lua portrayal and source-aware inspection. Dataset and catalogue versions must be compatible. |
| S-102 | A 3.0.0 HDF5 reader with bounded window reads, depth/uncertainty preservation and an independent portrayal catalogue. Current map display accepts EPSG:4326 within device texture limits. |
| S-421 | Statically linked bounded GML subset receiver, route/waypoint inspection and generated published 1.0 minimal route export. Dedicated S-421 Routes window and own-PC host illustration work without an ENC or external DLL. Official XSLT portrayal, click editing, authenticated exchange import remain incomplete. File/folder dataset opening, per-source catalogue provenance and independent route unloading are available. |
| Catalogues | Load FC, PC or an FC/PC pair from the File menu. S-101 loading can select a compatible pair from the local multi-version inventory. Product editions are checked; choosing a newer catalogue does not convert an older dataset. |
| Rendering | GPU point, line, area, text and SVG symbol rendering; Day/Dusk/Night settings; flat pan and zoom; screenshot export. A centre-based overscale caption uses the current DataCoverage owner and active PC SCLBR colour. A separate OVERSC01 pattern pass uses gap-selected coverage masks and active PC resources; unit tests and two-chain native rendering checks pass, while independent mask/lattice/palette qualification remains pending. Chart scale boundaries and own-ship input remain pending. |
| Inspection | Collapsible Object Details with source chart, position, attribute filtering and overlapping-object selection. Polygon holes are excluded from area interior picks. |
| Dataset UI | Open Dataset opens a file; Open Dataset Folder discovers supported datasets and exchange sets recursively. Data Layers separates catalogue metadata from product chart trees, with selection and unloading. Logs has level filters, search and grouped repeated messages; loading summaries do not resize the detail panel. |
| Debug UI | The toolbar **DEBUG MODE** button (F12) enables performance statistics and profiling. |
| Security | Optional S-100 dataset signature checking; native plugin integrity/signature hooks; a separate SECOM receive client under development. |

S-102 native-coordinate reading supports additional WGS84 UTM/UPS CRS codes,
but projected and polygon-domain data are not yet supported by the normal map
renderer. Mixed vertical references require explicit supported transformations;
no datum offset is guessed. See [BUILDING.md](BUILDING.md) for the current
S-102 storage, values-group and datum-adjustment contracts.

S-102 2.1 files are currently rejected by the 3.0 adapter. The old UKHO Complete
S10X trial package contains S-101 1.0 and S-102 2.1 data. Its S-101 cells require a
compatible catalogue pair rather than the bundled 2.0 pair; automatic selection
requires that pair to be present in the local catalogue inventory. S-101 cells from
multiple product editions use their own bound FC/PC pairs in one ordered display
list. New catalogue selection prefers the exact Edition and Revision, then permits
S-101's documented same-Edition compatibility fallback. Retained compatible owners
are preserved when another dataset is appended. Unsupported product
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

## macOS application bundle

After building on macOS and preparing `Catalogues/`, `Trust/` and the S-101
version inventory, package the release executable:

```bash
python3 tools/package_macos.py --binary target/release-fast/ferrite-s100 --inventory ../S101-Catalogues --output target/macos/FerriteS100.app
```

The bundle contains its executable, icon, catalogues and trust resources. Its
runtime paths are independent of the repository and launch directory. Logs use
`~/Library/Logs/FerriteS100`; caches and cancellation history use the existing
user directories. The packager applies and verifies a local ad hoc signature;
it does not produce an Apple-notarized distribution. Architecture follows the
chosen executable. Use a new `--output` path when rebuilding: existing bundles
are preserved. Open datasets through the app's File menu.

### macOS 앱 패키지

macOS에서 release 빌드와 카탈로그 준비를 마친 뒤 위 명령으로 `.app`을 만듭니다.
실행 파일·아이콘·FC/PC·Trust가 번들에 포함되어 저장소나 실행 위치에 의존하지
않습니다. 로그는 사용자 Library에 저장합니다. 기존 `.app`은 덮어쓰지 않으므로
다시 패키징할 때 새 출력 경로를 지정하세요. 로컬 ad hoc 서명이 적용되며 Apple
공증이나 다른 CPU 아키텍처 지원을 의미하지 않습니다. 데이터는 앱의 File 메뉴로
엽니다.

## S-421 route receiver

Use the **S-421 Routes** toolbar button and its **Import** action, or pass
`--s421 path/to/route.gml` at startup. This uses the native controller; it does
not enable unsigned external plugins. Import preserves the original XML and
commits only after the bounded namespace/reference/coordinate checks succeed.
Published 1.0 and the supported CDV 2.0 subset remain distinct. Unresolved local
references and missing CRS are rejected rather than guessed.

**Export minimal route** generates a published 1.0 geometry subset, not a
lossless copy of the imported dataset. CDV 2.0 is not relabeled or exported as 1.0.
Original metadata remains retained in memory. Routes are unverified; bare GML
import is rejected when `--require-signatures` or `--operational` is active until
an authenticated S-421 exchange-capture path is implemented.

Successful native import lazily captures independent, immutable public editing
PC resources: the declared waypoint SVG, leg style and explicit Day palette.
The legacy public PC has blank product/version fields and no modern display-plane
metadata; these are preserved rather than invented. Unsupported palettes and
stale resource/camera/revision candidates are rejected. This preparation path
does not execute the official XSLT rules. A separate GPU publication now draws
waypoint symbols and the editing leg style with independent buffers, texture and
view uniform. Current camera, resource owner, device/pipeline and surface bounds
must match before drawing; unsupported palettes do not substitute Day. Hidden
checks cover pan/zoom/return, stale camera rejection, display toggles and clearing.

Declared loxodrome/orthodrome legs in the supported published/CDV subset now use
WGS84 rhumb/geodesic solvers. Original endpoint IDs, coordinates and lexical values
are retained. Camera-independent sampled curves are cached within a 4MiB retained
payload limit; pan/zoom reuse them and rebuild only the screen packet. Declared
antimeridian legs use a continuous drawing copy on the current camera's longitude
sheet. Ambiguous, unsupported or over-budget geometry is rejected, not substituted.
The 1000m sampling step is a receiver policy, not a standard requirement or proven
screen-error tolerance. The payload limit is not a process RSS guarantee.

This remains a host illustration, not full official S-421 portrayal. Official
labels/WOL/XTL, turn-radius portrayal and chart-click editing remain incomplete. Loaded FC/PC
statuses report tool catalogue metadata, not a validated per-dataset FC/PC
binding or executed official XSLT portrayal. **Open Dataset** accepts supported
S-421 GML/XML files; **Open Dataset Folder** recursively queues routes alongside
S-101/S-102 inputs. Routes retain distinct source IDs and appear under S-421 in
Chart Data; their own tool FC/PC provenance is separate in Catalogue. Reopening
the same captured input does not duplicate it. Unloading a route preserves the
ENC/raster publication and camera. Unsupported or damaged route entries are
reported in Logs without preventing other independent inputs from loading.
Native panel JSON is parsed once
per publication and waypoint widgets are limited to visible scroll rows.

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
schema; it does not imply that an external S-102 FC was loaded. Catalogue groups
contain FC/PC versions and status; Chart Data groups contain chart files. The
panel retains its resized width. The top toolbar groups file, view, status and
settings controls with icons and tooltips. Logs and Load status open separate
windows. Object selection is OFF by default; enable its cursor icon to inspect
objects. Detail previews use the selected chart's original PC and current palette.
Zoom is relative to the fitted dataset view, with a maximum of 500×; nautical
scale is indicated separately on the chart.
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
| Left click | Inspect an object when Object selection mode is ON (default OFF) |
| Esc | Clear the selected object or dataset and its details; leave selection mode unchanged |
| Object selection mode OFF | Clear the current selection and details |
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

Start with `--operational` to require authenticated datasets for the whole session.
The signature toggle and settings reset cannot disable this policy; missing trust
anchors or invalid/missing signatures reject loading. This profile does not imply
ECDIS certification or complete S-100 operational conformance.

When enabled, `ferrite-security` verifies S-100 Part 15 ECDSA P-384/SHA-384
exchange catalogue and resource signatures using separately installed X.509 trust
anchors. It checks certificate paths, validity and key usage, rejects escaping
paths and unlisted datasets, and provides private snapshots to product readers.
Signature failures block loading. Root runtime trust files are local configuration,
not automatically delivered by the repository.

Lua portrayal excludes the OS, IO and Debug libraries and restricts file/module
access to its catalogue sandbox. Memory and per-operation instruction/time budgets
limit supported execution paths. Hooks are not process isolation: caught hook
errors, native callbacks and public raw VM access still require further containment.

Instruction caches use an app-private directory; chart-adjacent cache sidecars
are ignored. Plugins are native dynamic libraries. Release currently rejects
plugins without a provisioned verification key; debug unsigned loading requires
`FERRITE_ALLOW_UNSIGNED_PLUGINS=1`. The signature hook covers the native library,
not an authenticated plugin manifest. Integrity and ABI checks do not provide an
operating-system sandbox.

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
S-101 raw decoded cells also have a session RAM LRU, retained across manual
unload/reopen, with up to 32 entries and a 96 MiB charged allocation-payload
budget. Authentication, cancellation/update planning and current FC/PC checks
still run for every load; cached snapshots precede catalogue normalization.
The budget excludes live worker copies, allocator overhead and process RSS.
This cache does not persist across application restarts or cache S-102 HDF5.
`FERRITE_NO_CACHE=1` bypasses both decoded-cell and instruction caches.

Flat navigation event coalescing is enabled by default. Camera updates remain
chronological; redundant scene preparation is merged before presentation.
`FERRITE_FLAT_EVENT_COALESCING=0` restores immediate preparation. Loaded external
plugins retain the immediate path. Current hidden 18-chart controls verified
all 500 camera transcripts and 20 complete pixel/IR/GPU snapshots in each of
Day, Dusk and Night, with four- and eight-event bursts. Day handler median fell
from about 51 ms to 13 ms; slow frames still exceed 16.7 ms. These measurements
do not establish foreground display FPS or Linux/Windows performance.

Static line-bound reuse is available with `FERRITE_STATIC_LINE_BOUNDS_CSE=1`,
OFF by default, with an 8 MiB capacity limit and exact source-lifetime binding.
It preserves current visibility, scale and clipping evaluation. Current hidden
18-chart controls reduced the line-stage median by about 16%, while handler tail
improvement remained inconclusive.

Retained world-area GPU projection and exact repeated-camera area projection
caching are implemented behind `FERRITE_RETAINED_WORLD_AREAS=1`, OFF by default.
Local coordinates corrected a GPU precision failure in actual readback. Strict
ON/OFF comparisons still differ by 1–13 pixels at tested zooms on one SHOM chart,
so this path remains experimental. The error tolerance is an engineering trial
bound, not a standard allowance. Ordinary rendering with this flag OFF has
passed the two-chart, five-zoom hidden animation controls. These controls compare
same-camera phase consistency and do not measure visible animation smoothness.

The moving-camera line northing cache is also experimental and OFF by default
(`FERRITE_MOVING_LINE_NORTHING_REUSE=1`). It reuses source projection values while
applying the current camera and material each frame. Current timing and complete
native output qualification are pending.

Hidden synthetic handler timings are not displayed FPS. Sustained foreground
60 FPS remains a goal; full physical input, DPI and Windows performance validation
is pending.

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

### Performance diagnostics and experiments

`FERRITE_GPU_FRAME_TIMESTAMPS=1` enables an optional hidden-audit batch of 64
actual chart render-pass timestamps. The normal application does not allocate
batch query/readback resources. The batch resolves in the existing submission;
readback happens only after the measured callback window. Unsupported hardware
and legacy-profiler conflicts are explicit unavailable results. This measures
only the chart render pass, not uploads, UI, surface presentation or physical FPS.

The existing default-off line-suppression growth experiment has an additional
`FERRITE_LINE_SUPPRESSION_UNCHANGED_TARGETS=1` option. It reuses prior relations
only when new source geometry cannot affect a covered target, while retaining
original exact overlap and live eligibility rules. A Mac 18-chart OFF/ON check
preserved pixels, raw GPU data and drawing instructions, but a single pair did
not improve total-window mean time. The option remains off pending wider testing.

### Native S-100 MCP

Open **Help → S-100 MCP** to enable read-only access to loaded datasets. No plugin DLL or separate server binary is required. The service starts disabled; a public ngrok tunnel is a separate opt-in. Common tools expose dataset/product/FC/PC metadata, and the S-101 adapter adds catalogue, attribute and feature queries. Multiple loaded datasets require an explicit dataset ID. See [S-100 MCP usage and limits](docs/S100-MCP.md).
