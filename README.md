<p align="center">
  <img src="icon.png" alt="FerriteS100 Logo" width="200" height="200">
</p>

<h1 align="center">FerriteS100</h1>

<p align="center">
  <strong>A high-performance S-100/S-101 Electronic Navigational Chart (ENC) viewer written in Rust</strong>
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#screenshots">Screenshots</a> •
  <a href="#installation">Installation</a> •
  <a href="#usage">Usage</a> •
  <a href="#security">Security</a> •
  <a href="#sbom-software-bill-of-materials">SBOM</a> •
  <a href="#license">License</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Rust-1.70+-orange?logo=rust" alt="Rust">
  <img src="https://img.shields.io/badge/License-PolyForm%20NC-blue" alt="License">
  <img src="https://img.shields.io/badge/Platform-Windows%20%7C%20Linux%20%7C%20macOS-lightgrey" alt="Platform">
  <img src="https://img.shields.io/badge/GPU-Vulkan%20%7C%20DX12%20%7C%20Metal-green" alt="GPU">
  <img src="https://img.shields.io/badge/Navigation-Please%20Don't-red" alt="Not for Navigation">
</p>

---

## Overview

**FerriteS100** is a Rust application *trying* to parse and render IHO S-100/S-101 electronic navigational charts. Because apparently reading PDFs of maritime standards wasn't painful enough, we decided to implement them in Rust.

The project leverages modern GPU rendering via [wgpu](https://wgpu.rs/) and *attempts* to follow the S-100 portrayal model using Lua scripts. No ships were harmed in the making of this software (please don't actually navigate with this).

## Screenshots

<p align="center">
  <img src="Screenshot.png" alt="FerriteS100 Screenshot" width="900">
</p>

<p align="center"><em>S-101 Electronic Navigational Chart rendered with FerriteS100</em></p>

## Features

| Category | Description |
|----------|-------------|
| **Chart Parsing** | Full ISO 8211 binary format parsing for S-101 ENC files (`.000`) |
| **Memory-Mapped I/O** | Zero-copy file loading using OS page cache for faster chart loading |
| **Dynamic Catalogues** | Runtime loading of Feature Catalogue (FC) and Portrayal Catalogue (PC) from XML |
| **Lua Portrayal** | Standard-compliant portrayal using official S-100 Lua scripts |
| **GPU Rendering** | Hardware-accelerated rendering with wgpu (Vulkan/DX12/Metal) |
| **Multi-cell Support** | Load and display multiple chart cells simultaneously |
| **Symbol Rendering** | SVG-based symbol rendering with color profile support |
| **String Interning** | Game-style optimization reducing memory usage for symbol references |
| **Plugin System** | Extensible architecture with ABI-stable plugin support |

### Interactive Features

- **Pan & Zoom** — Smooth navigation with mouse/trackpad
- **Feature Inspector** — Click any feature to view detailed attributes
- **Real-time Coordinates** — Live latitude/longitude display
- **Screenshot Export** — Save rendered charts as PNG

### Plugin System

FerriteS100 supports plugins for extended functionality. Each plugin:
- Loads independently as a dynamic library (DLL)
- Has its own FC/PC catalogue loading (e.g., S-421 for route planning)
- Uses ABI-stable interface via [abi_stable](https://docs.rs/abi_stable)
- Runs in a sandboxed environment with limited host API access

**Included Plugin: S-421 Route Planner**

| Feature | Description |
|---------|-------------|
| **Waypoint Management** | Click to add, right-click to remove waypoints |
| **Distance Calculation** | Haversine formula for accurate nautical miles |
| **S-421 Export/Import** | Standard-compliant GML route exchange format |
| **Customizable Display** | Line color, width, distance units, waypoint symbols |

## Installation

### Prerequisites

- **Rust 1.70+** with Cargo
- GPU with **Vulkan**, **DirectX 12**, or **Metal** support

### Build from Source

```bash
# Clone the repository
git clone https://github.com/hoyeonchoKMOU/FerriteS100.git
cd FerriteS100

# Build (release mode recommended)
cargo build --release

# Run
cargo run --release
```

### Build with Plugins (Windows PowerShell)

```powershell
# Build main app and all plugins
.\scripts\build.ps1

# Build main app only (skip plugins)
.\scripts\build.ps1 -SkipPlugins

# Create release package
.\scripts\release.ps1

# Create KMOU edition (includes Catalogues and ChartData)
.\scripts\release.ps1 -KMOU
```

## Usage

### Quick Start

1. Place S-101 chart files (`.000`) in the `ChartData/` directory
2. Ensure catalogues are configured:
   - Feature Catalogue: `Catalogues/FC/S-101/`
   - Portrayal Catalogue: `Catalogues/PC/S-101/`
3. Run the application

### S-101 Test Data

S-101 sample charts can be downloaded from the **UKHO Data Hub**:

- [UKHO S-101 Test Data](https://datahub.admiralty.co.uk/portal/home/item.html?id=6966cb7ce9454ccf9afbbd3c9a105f9e)

> **Note**: UKHO provides official S-101 test datasets for development and testing purposes. Registration may be required.

### Controls

| Input | Action |
|-------|--------|
| **Scroll** | Zoom in/out |
| **Left Drag** | Pan the view |
| **Left Click** | Inspect feature |
| **Right Click** | Reset view |
| **File → Open** | Load additional charts |

#### Route Plugin Controls (when active)

| Input | Action |
|-------|--------|
| **Left Click** | Add waypoint at position |
| **Right Click** | Remove last waypoint |
| **Export** | Save route as S-421 GML file |
| **Import** | Load route from S-421 GML file |

## Project Structure

```
FerriteS100/
├── src/
│   ├── main.rs                    # Application entry point
│   └── plugins.rs                 # Plugin system integration
├── crates/
│   ├── ferrite-iso8211/           # ISO 8211 binary format parser
│   ├── ferrite-s100-core/         # S-100 core data structures
│   ├── ferrite-feature-catalog/   # Feature Catalogue XML parser
│   ├── ferrite-portrayal-catalog/ # Portrayal Catalogue XML parser
│   ├── ferrite-lua/               # Lua portrayal engine (sandboxed)
│   ├── ferrite-render/            # Abstract rendering instructions
│   ├── ferrite-wgpu/              # GPU renderer
│   ├── ferrite-plugin-api/        # Plugin API (ABI-stable traits)
│   └── ferrite-plugin-loader/     # Plugin DLL loader & verifier
├── plugins/
│   └── route-plugin/              # S-421 Route Planner plugin
├── Catalogues/
│   ├── FC/
│   │   ├── S-101/                 # S-101 Feature Catalogue
│   │   └── S-421/                 # S-421 Feature Catalogue (for plugins)
│   └── PC/
│       ├── S-101/                 # S-101 Portrayal Catalogue
│       └── S-421/                 # S-421 Portrayal Catalogue (for plugins)
├── ChartData/                     # S-101 chart files (.000)
└── scripts/
    ├── build.ps1                  # Build main app & plugins
    ├── release.ps1                # Create release package
    └── upload.ps1                 # Upload to GitHub releases
```

## Key Principles

1. **No Hardcoding** — Feature and Portrayal catalogues are loaded dynamically at runtime (because we learned the hard way)
2. **Standard Compliance** — Lua portrayal rules follow IHO international standards (or at least we're trying)
3. **S-100 Specification** — Implementation *inspired by* IHO S-100/S-101 specifications (we read most of the 500+ pages, probably)

## Security

FerriteS100 implements a **sandboxed Lua environment** via [mlua](https://github.com/mlua-rs/mlua):

| Protection | Description |
|------------|-------------|
| **Safe Libraries Only** | Only loads base, string, table, math |
| **No Dangerous Libraries** | `os`, `io`, `debug` are **NOT** loaded |
| **No File Loading** | `loadfile` and `dofile` disabled |
| **No C Modules** | `package.loadlib` and C searchers disabled |
| **Restricted Search** | Only Portrayal Catalogue directory is searchable |

This design mitigates potential security risks from malicious Portrayal Catalogue scripts (**CWE-829**, **CWE-749**).

### Plugin Security

Plugins are loaded as dynamic libraries with security verification:

| Protection | Description |
|------------|-------------|
| **Ed25519 Signatures** | DLLs are verified against signed manifests (production mode) |
| **SHA-256 Hash** | DLL integrity verified before loading |
| **Sandboxed Host API** | Plugins can only call whitelisted host functions |
| **ABI Stable Interface** | [abi_stable](https://docs.rs/abi_stable) ensures safe FFI |
| **Version Compatibility** | API version checked before plugin initialization |

## SBOM (Software Bill of Materials)

FerriteS100 supports generating SBOM for supply chain security and compliance.

### Generate SBOM

```bash
# Install cargo-sbom (SPDX format)
cargo install cargo-sbom

# Generate SPDX SBOM
cargo sbom > sbom.spdx.json

# Alternative: CycloneDX format
cargo install cargo-cyclonedx
cargo cyclonedx --format json > sbom.cdx.json
```

### Dependency Audit

```bash
# Install cargo-audit
cargo install cargo-audit

# Scan for known vulnerabilities
cargo audit

# Generate audit report
cargo audit --json > audit-report.json
```

### Dependency Tracking

| File | Description |
|------|-------------|
| `Cargo.lock` | Exact dependency versions (committed to repo) |
| `Cargo.toml` | Direct dependency declarations |

All dependencies are sourced from [crates.io](https://crates.io) and audited via [RustSec Advisory Database](https://rustsec.org/).

## Dependencies

### Graphics & GUI

| Crate | Version | Purpose |
|-------|---------|---------|
| [wgpu](https://wgpu.rs/) | 24 | GPU rendering (Vulkan/DX12/Metal) |
| [winit](https://github.com/rust-windowing/winit) | 0.30 | Cross-platform window management |
| [egui](https://github.com/emilk/egui) | 0.31 | Immediate mode GUI |
| [resvg](https://github.com/RazrFalcon/resvg) | 0.45 | SVG symbol rendering |
| [image](https://github.com/image-rs/image) | 0.25 | Image processing (PNG export) |

### Parsing & Data

| Crate | Version | Purpose |
|-------|---------|---------|
| [nom](https://github.com/rust-bakery/nom) | 8 | ISO 8211 binary parsing |
| [quick-xml](https://github.com/tafia/quick-xml) | 0.37 | FC/PC XML catalogue parsing |
| [serde](https://serde.rs/) | 1 | Serialization framework |
| [encoding_rs](https://github.com/hsivonen/encoding_rs) | 0.8 | Character encoding (ISO-8859-1) |
| [memmap2](https://github.com/RazrFalcon/memmap2-rs) | 0.9 | Memory-mapped file I/O for zero-copy chart loading |

### Scripting & Runtime

| Crate | Version | Purpose |
|-------|---------|---------|
| [mlua](https://github.com/mlua-rs/mlua) | 0.10 | Sandboxed Lua 5.4 portrayal engine |
| [rayon](https://github.com/rayon-rs/rayon) | 1.10 | Parallel chart loading & processing |

### Plugin System

| Crate | Version | Purpose |
|-------|---------|---------|
| [abi_stable](https://docs.rs/abi_stable) | 0.11 | ABI-stable trait objects for plugins |
| [libloading](https://github.com/nagisa/rust_libloading) | 0.8 | Dynamic library loading |
| [ed25519-dalek](https://github.com/dalek-cryptography/ed25519-dalek) | 2 | Plugin signature verification |
| [sha2](https://github.com/RustCrypto/hashes) | 0.10 | DLL hash verification |

### Utilities

| Crate | Version | Purpose |
|-------|---------|---------|
| [tracing](https://github.com/tokio-rs/tracing) | 0.1 | Structured logging |
| [anyhow](https://github.com/dtolnay/anyhow) | 1 | Error handling |
| [thiserror](https://github.com/dtolnay/thiserror) | 2 | Error derive macros |
| [earcutr](https://github.com/frewsxcv/earcutr) | 0.4 | Polygon triangulation (earcut) |

## References

- [IHO S-100 / S-101 Standards](https://registry.iho.int/)

## License

This project is licensed under the **PolyForm Noncommercial License 1.0.0**.

| Usage | Allowed |
|-------|---------|
| Research / Academic | Yes |
| Personal / Non-profit | Yes |
| Modification | Yes (with attribution) |
| Commercial / Enterprise | **No** |

**Key points:**
- Free for research, education, and non-commercial use
- Modifications allowed with proper attribution
- **Not** a copyleft license (your modifications don't have to use the same license)
- Commercial use requires separate licensing agreement

See the [LICENSE](LICENSE) file for full terms.

## Contributing

This is a personal project and is not accepting contributions at this time. Feel free to fork it if you'd like to experiment on your own!

## Acknowledgments

- **IHO** (International Hydrographic Organization) for the S-100/S-101 standards
- **Korea Maritime and Ocean University (KMOU)** for research support

---

<p align="center">
  <sub>Developed by <a href="https://github.com/hoyeonchoKMOU">Hoyeon Cho</a> at KMOU</sub>
</p>
