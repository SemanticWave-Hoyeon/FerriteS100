# Building FerriteS100

Use the current stable Rust toolchain (`rustup update stable`). Python 3 is
needed only for the optional portable build helper.

```text
python3 tools/build.py --profile dev --lua 54
python3 tools/build.py --profile release-fast --lua 54
python3 tools/build.py --profile release --lua 54
```

On Windows, use `py -3` instead of `python3`. Use `--lua 55` to select Lua 5.5
at build time. The helper enables exactly one Lua backend, preserves Cargo.lock,
and defaults to at most four parallel compilation jobs. Override with `--jobs N`.
Add `--offline` when dependencies are already cached.

`dev` retains full debugging information and debug checks. `release-fast` uses
optimization level 3, local LTO within each crate, 16 code generation units and incremental
compilation for frequent optimized builds. `release` retains full LTO and one
code generation unit for final distribution. These profiles have separate
artifact caches; changing compiler or profile requires an initial rebuild.
Keep the target directory between builds to reuse those caches.

The usual executable locations are `target/debug/ferrite-s100`,
`target/release-fast/ferrite-s100` and `target/release/ferrite-s100` (with `.exe`
on Windows). Cargo environment/configuration can override the target directory.
Timing reports are written under `target/cargo-timings/`.

Without Python, the equivalent frequent optimized build is:

```text
cargo build --locked --profile release-fast --bin ferrite-s100 --no-default-features --features lua54 --jobs 4 --timings
```

The helper locks the selected Cargo target for the full build and checks compiler
inputs before and after compilation. Root runtime folders such as `ChartData`,
`TestData` and `Trust` are not traversed; dataset links do not invalidate a build.
Symlinks in source folders remain rejected. Put compile-time embedded files under
source/assets instead of runtime folders. Runtime signature verification is
independent of this build-input check.

Build-time measurements and rendering/frame-rate measurements are separate:
faster compilation alone does not prove runtime performance or cross-OS parity.


### Constant S-102 datum adjustments (application input)

Use `--s102-datum-adjustments /absolute/path/adjustments.json` for explicit,
constant positive-down corrections within one dataset. Configuration is limited
 to 1 MiB and keyed by the SHA-256 of the exact input bytes. Dataset signatures
 do not authenticate this application configuration. When signature checking is
 OFF, configured inputs are still privately copied before hashing and HDF5 open;
 this copy carries no authenticated status.

Format: `{"datasets":[{"sha256":"<64 hexadecimal characters>","target_datum":10,
"corrections":[{"datum":23,"correction_metres":<supplied metres>,
"source":"<transformation evidence supplied by the operator>"}]}]}`.
The angle-bracket fields must be replaced with real values; this is a schema
example, not a usable transformation. Adjusted depth = raw depth + correction.
Same scoped datum uses identity; an identity correction cannot be overridden.
Unknown populated source transformations fail the file transaction. Different
files are separate scopes and their IHO datum classification codes do not prove
that they share a physical reference surface. Configured offsets are retained
while a file is loaded and reused for recolouring and point inspection.

The current application supports constant supplied offsets. Spatially varying
transforms require an error/approximation contract before display-query parity
can be claimed. Exact original cell boundary queries use commonPointRule=low;
GPU texture floor selection at those boundaries remains a known unclosed gap.
Multi-instance common grids have a 64M-cell output-work limit, besides kernel
per-axis and bounded-block limits. Unsupported partitions fail explicitly.


S-102 values-group contract (3.0 adapter)
--------------------------------------
The reader requires minimumDepth, maximumDepth, minimumUncertainty,
maximumUncertainty and timePoint on each Group_001 (S-102 3.0 §10.2.6).
Depth-only compound values are supported when uncertainty is absent from
Group_F and both group uncertainty extrema are equal (§10.2.7). The constant
is supplied on bounded window reads and retained as original source uncertainty
through conservative depth selection. 1000000 represents unknown depth or
uncertainty; zero uncertainty is admitted by the catalogue lower bound.
Unknown compound members and inconsistent feature definitions are rejected.
The adapter supports complete basic ISO8601 HDF DateTime (local time, Z or
basic UTC offset). The recommended S-102 timePoint is 00010101T000000Z;
other valid supported timestamps are retained without inferring survey dates.
S-102 3.0 cites S-100 5.2.0; the locally reviewed general type table is 5.2.1.
Per-window range diagnostics examine only requested cells; populated raw values
are retained despite inconsistent metadata extrema. Nonfinite values and negative
uncertainty are rejected. Detected range issues are logged and shown in object
details. validate_s102 is a strict standalone checker that streams
all depth/uncertainty values and checks observed extrema against declarations.
These checks do not certify every schema attribute, domainExtent masks,
noncanonical storage order or conservative GPU shared-boundary selection.

Fixed ASCII/UTF-8 metadata strings up to 4096 bytes are decoded with a fixed
string memory type. DateTime validation uses the kernel clock parser (up to
nanosecond precision; leap-second and arbitrary-precision support not claimed).


S-102 canonical storage admission
--------------------------------
The product reader enforces the SW/east-then-north storage restrictions in
S-102 3.0 clauses4.2/4.4: positive XY spacing, linear sequencing, start0,0.
Signed/reordered generic examples in Table10-4 do not trigger silent transpose.
Table5-1 CRS codes are accepted with Longitude/Latitude for4326 and
Easting/Northing for the supported WGS84 UTM/UPS codes. Native-coordinate
reads are separate from portrayal: map portrayal still supports4326 only.
Generic negative-orientation GPU tests use a separate CoverageSource wrapper
over a canonical positive-spacing HDF grid, so kernel coverage is preserved.
The composition fixture and independent integer oracle now both use positive
canonical source-node coordinates. These fixtures do not certify every schema
attribute. The strict scanner propagates traversal errors and rejects empty
input; directory memory is O(HDF file count + traversal depth), not all entries.

The strict whole-grid validator bounds each HDF window to2048 columns x128
rows; min/max/count accumulation is O(1) and does not allocate a full raster.
