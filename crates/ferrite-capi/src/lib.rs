//! C ABI over the S-100 chart engine ([`ChartEngine`]); the header is
//! `include/ferrite.h`.
//!
//! - Opaque handles: [`FerriteEngine`] is created by
//!   [`ferrite_engine_create`] and released by [`ferrite_engine_destroy`].
//! - Status codes: [`FERRITE_OK`] (0) or a negative error; the message of the
//!   last failure on the calling thread is [`ferrite_last_error`].
//! - Strings returned by the library (`char *`) are UTF-8 JSON owned by the
//!   caller and released with [`ferrite_string_free`].
//! - No panic crosses the boundary: every entry point catches unwinding and
//!   reports [`FERRITE_PANIC`].
//! - A handle is used by one thread at a time (it is not internally locked).
#![allow(clippy::missing_safety_doc)]

use anyhow::{ensure, Context, Result};
use ferrite_cli::{
    engine_config, feature_json, inspection_json, issue_json, OffscreenChartRenderer,
};
use ferrite_engine::chart_engine::{
    Camera, ChartEngine, DatasetSource, DisplayOptions, OpenReport, View, Zoom,
};
use ferrite_engine::platform::{DirectoryCacheStore, NoCacheStore, Platform};
use ferrite_engine::scene::PreparedScene;
use ferrite_engine::{PortrayalSource, ViewportPixels};
use std::cell::RefCell;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;

pub const FERRITE_OK: i32 = 0;
/// The operation failed; see [`ferrite_last_error`].
pub const FERRITE_ERROR: i32 = -1;
/// A null handle or pointer, invalid UTF-8 or an out-of-range value.
pub const FERRITE_INVALID_ARGUMENT: i32 = -2;
/// No scene was prepared (call [`ferrite_engine_prepare`] first).
pub const FERRITE_NO_SCENE: i32 = -3;
/// The output buffer is smaller than width * height * 4 bytes.
pub const FERRITE_BUFFER_TOO_SMALL: i32 = -4;
/// A panic inside the library was caught.
pub const FERRITE_PANIC: i32 = -5;

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_error(message: &str) {
    let text = CString::new(message.replace('\0', " ")).unwrap_or_default();
    LAST_ERROR.with(|e| *e.borrow_mut() = text);
}

/// Failure kinds mapped to status codes.
enum Failure {
    Error(anyhow::Error),
    Invalid(String),
    NoScene,
    BufferTooSmall(usize),
}
impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self::Error(error)
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Run `body`, mapping failures and panics to status codes.
fn status(body: impl FnOnce() -> std::result::Result<(), Failure>) -> i32 {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(())) => {
            set_error("");
            FERRITE_OK
        }
        Ok(Err(Failure::Error(error))) => {
            set_error(&format!("{error:#}"));
            FERRITE_ERROR
        }
        Ok(Err(Failure::Invalid(message))) => {
            set_error(&message);
            FERRITE_INVALID_ARGUMENT
        }
        Ok(Err(Failure::NoScene)) => {
            set_error("No prepared scene; call ferrite_engine_prepare first");
            FERRITE_NO_SCENE
        }
        Ok(Err(Failure::BufferTooSmall(needed))) => {
            set_error(&format!("Output buffer needs {needed} bytes"));
            FERRITE_BUFFER_TOO_SMALL
        }
        Err(payload) => {
            set_error(&format!("panic: {}", panic_message(payload.as_ref())));
            FERRITE_PANIC
        }
    }
}

/// Run `body` returning an owned C string (null on failure).
fn string_result(body: impl FnOnce() -> std::result::Result<String, Failure>) -> *mut c_char {
    let mut out = std::ptr::null_mut();
    let code = status(|| {
        let text = body()?;
        out = CString::new(text)
            .map_err(|_| Failure::Invalid("Result contains NUL".into()))?
            .into_raw();
        Ok(())
    });
    if code != FERRITE_OK {
        return std::ptr::null_mut();
    }
    out
}

unsafe fn text<'a>(pointer: *const c_char, what: &str) -> std::result::Result<&'a str, Failure> {
    if pointer.is_null() {
        return Err(Failure::Invalid(format!("{what} is null")));
    }
    CStr::from_ptr(pointer)
        .to_str()
        .map_err(|_| Failure::Invalid(format!("{what} is not UTF-8")))
}

unsafe fn optional_text<'a>(
    pointer: *const c_char,
    what: &str,
) -> std::result::Result<Option<&'a str>, Failure> {
    if pointer.is_null() {
        Ok(None)
    } else {
        text(pointer, what).map(Some)
    }
}

/// An engine with its prepared scene and offscreen renderer.
pub struct FerriteEngine {
    engine: ChartEngine,
    /// Issues and notices of every open so far.
    opened: OpenReport,
    scene: Option<Scene>,
    renderer: Option<OffscreenChartRenderer>,
}

struct Scene {
    scene: PreparedScene,
    viewport: ViewportPixels,
    zoom: f64,
}

unsafe fn engine<'a>(
    handle: *mut FerriteEngine,
) -> std::result::Result<&'a mut FerriteEngine, Failure> {
    handle
        .as_mut()
        .ok_or_else(|| Failure::Invalid("Engine handle is null".into()))
}

/// The camera of [`ferrite_engine_prepare`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FerriteCamera {
    /// Non-zero: centre the view on `latitude`/`longitude` (degrees).
    pub has_center: i32,
    pub latitude: f64,
    pub longitude: f64,
    /// Zoom over the all-data view (> 0), or 0 to use `scale_denominator`.
    pub zoom: f64,
    /// Display scale 1:N when `zoom` is 0 (0 with zoom 0: all data).
    pub scale_denominator: f64,
}

/// Library version (static string).
#[no_mangle]
pub extern "C" fn ferrite_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// Message of the last failure on this thread ("" after a success). The
/// pointer stays valid until the next library call on this thread.
#[no_mangle]
pub extern "C" fn ferrite_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

/// Release a string returned by the library (null is ignored).
#[no_mangle]
pub unsafe extern "C" fn ferrite_string_free(string: *mut c_char) {
    if !string.is_null() {
        let _ = catch_unwind(|| drop(CString::from_raw(string)));
    }
}

/// Create an engine with the catalogue folder `catalogues` (`FC/S-101`,
/// `PC/S-101`, optional S-124/S-129). `inventory` (nullable) is an installed
/// S-101 catalogue inventory for datasets of other editions; `cache_dir`
/// (nullable) keeps per-cell portrayal caches and precomputed payloads
/// (null: no disk cache). Returns null on failure.
#[no_mangle]
pub unsafe extern "C" fn ferrite_engine_create(
    catalogues: *const c_char,
    inventory: *const c_char,
    cache_dir: *const c_char,
) -> *mut FerriteEngine {
    let mut out = std::ptr::null_mut();
    status(|| {
        let catalogues = text(catalogues, "catalogues")?;
        let inventory = optional_text(inventory, "inventory")?;
        let cache_dir = optional_text(cache_dir, "cache_dir")?;
        let platform = match cache_dir {
            Some(dir) => Platform::native().with_cache(Arc::new(
                DirectoryCacheStore::new(dir).with_context(|| format!("Cannot use cache {dir}"))?,
            )),
            None => Platform::native().with_cache(Arc::new(NoCacheStore)),
        };
        let mut config = engine_config(platform, catalogues);
        config.inventory = inventory.map(PathBuf::from);
        let engine = ChartEngine::new(config)?;
        out = Box::into_raw(Box::new(FerriteEngine {
            engine,
            opened: OpenReport::default(),
            scene: None,
            renderer: None,
        }));
        Ok(())
    });
    out
}

/// Destroy an engine (null is ignored).
#[no_mangle]
pub unsafe extern "C" fn ferrite_engine_destroy(handle: *mut FerriteEngine) {
    if !handle.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| drop(Box::from_raw(handle))));
    }
}

fn open(handle: &mut FerriteEngine, source: DatasetSource) -> std::result::Result<(), Failure> {
    let report = handle.engine.open_blocking(source)?;
    handle.opened.datasets.extend(report.datasets);
    handle.opened.issues.extend(report.issues);
    handle.opened.notices.extend(report.notices);
    // A new dataset set needs a new scene.
    handle.scene = None;
    Ok(())
}

/// Open every dataset under a folder (exchange sets included). Rejected
/// datasets are reported by [`ferrite_engine_report_json`], not as failure.
#[no_mangle]
pub unsafe extern "C" fn ferrite_engine_open_folder(
    handle: *mut FerriteEngine,
    path: *const c_char,
) -> i32 {
    status(|| {
        let handle = engine(handle)?;
        let path = text(path, "path")?;
        open(handle, DatasetSource::Folder(path.into()))
    })
}

/// Open dataset files (an S-101 base with its updates belongs in one call).
#[no_mangle]
pub unsafe extern "C" fn ferrite_engine_open_files(
    handle: *mut FerriteEngine,
    paths: *const *const c_char,
    count: usize,
) -> i32 {
    status(|| {
        let handle = engine(handle)?;
        if paths.is_null() && count > 0 {
            return Err(Failure::Invalid("paths is null".into()));
        }
        let mut files = Vec::with_capacity(count);
        for index in 0..count {
            files.push(PathBuf::from(text(*paths.add(index), "path")?));
        }
        open(handle, DatasetSource::Files(files))
    })
}

/// Open datasets held in memory. `names[i]` identifies `data[i]` (`lengths[i]`
/// bytes): its file name decides the product and the update chain (e.g.
/// `101FR00369400.000`). The bytes are copied. No signature verification.
#[no_mangle]
pub unsafe extern "C" fn ferrite_engine_open_bytes(
    handle: *mut FerriteEngine,
    names: *const *const c_char,
    data: *const *const u8,
    lengths: *const usize,
    count: usize,
) -> i32 {
    status(|| {
        let handle = engine(handle)?;
        if count > 0 && (names.is_null() || data.is_null() || lengths.is_null()) {
            return Err(Failure::Invalid("names, data or lengths is null".into()));
        }
        let mut files = Vec::with_capacity(count);
        for index in 0..count {
            let name = text(*names.add(index), "name")?;
            let (pointer, length) = (*data.add(index), *lengths.add(index));
            if pointer.is_null() && length > 0 {
                return Err(Failure::Invalid(format!("data of {name} is null")));
            }
            let bytes: Arc<[u8]> = if length == 0 {
                Arc::from(&[][..])
            } else {
                Arc::from(std::slice::from_raw_parts(pointer, length))
            };
            files.push((PathBuf::from(name), bytes));
        }
        open(handle, DatasetSource::Bytes(files))
    })
}

/// Colour profile ("Day", "Dusk", "Night") of the next prepared scene.
#[no_mangle]
pub unsafe extern "C" fn ferrite_engine_set_palette(
    handle: *mut FerriteEngine,
    palette: *const c_char,
) -> i32 {
    status(|| {
        let handle = engine(handle)?;
        let palette = text(palette, "palette")?;
        let display = DisplayOptions {
            profile: palette.into(),
            ..handle.engine.display().clone()
        };
        handle.engine.set_display(display)?;
        Ok(())
    })
}

/// Portray S-101 cells from precomputed payloads in the cache directory
/// (non-zero) or with the Lua rules (zero; the default when built).
#[no_mangle]
pub unsafe extern "C" fn ferrite_engine_set_precomputed(
    handle: *mut FerriteEngine,
    precomputed: i32,
) -> i32 {
    status(|| {
        let handle = engine(handle)?;
        handle.engine.set_portrayal_source(if precomputed != 0 {
            PortrayalSource::Precomputed
        } else {
            PortrayalSource::Lua
        });
        Ok(())
    })
}

/// Portray the opened datasets for a `width` x `height` pixel view at
/// `density` physical pixels per logical pixel. `camera` (nullable): null
/// shows all data.
#[no_mangle]
pub unsafe extern "C" fn ferrite_engine_prepare(
    handle: *mut FerriteEngine,
    width: u32,
    height: u32,
    density: f64,
    camera: *const FerriteCamera,
) -> i32 {
    status(|| {
        let handle = engine(handle)?;
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return Err(Failure::Invalid("Size must be within 1..16384".into()));
        }
        if !(density.is_finite() && density > 0.) {
            return Err(Failure::Invalid("Density must be positive".into()));
        }
        let viewport = ViewportPixels {
            width,
            height,
            scale_factor: density,
        };
        let camera = match camera.as_ref() {
            None => Camera {
                center: None,
                zoom: Zoom::Factor(1.0),
            },
            Some(c) => Camera {
                center: (c.has_center != 0).then_some([c.latitude, c.longitude]),
                zoom: if c.zoom > 0. {
                    Zoom::Factor(c.zoom)
                } else if c.scale_denominator > 0. {
                    Zoom::Scale(c.scale_denominator)
                } else {
                    Zoom::Factor(1.0)
                },
            },
        };
        handle.scene = None;
        let (_, zoom) = handle.engine.camera_view(camera, viewport)?;
        let scene = handle.engine.prepare(&View {
            fit: None,
            viewport,
            camera: Some(camera),
        })?;
        let scene =
            Arc::try_unwrap(scene).map_err(|_| anyhow::anyhow!("Prepared scene is shared"))?;
        handle.scene = Some(Scene {
            scene,
            viewport,
            zoom,
        });
        Ok(())
    })
}

/// Render the prepared scene offscreen (GPU) into `rgba`: `width * height * 4`
/// bytes, rows top to bottom, straight RGBA8.
#[no_mangle]
pub unsafe extern "C" fn ferrite_engine_render_rgba(
    handle: *mut FerriteEngine,
    rgba: *mut u8,
    length: usize,
) -> i32 {
    status(|| {
        let handle = engine(handle)?;
        let FerriteEngine {
            engine,
            scene,
            renderer,
            ..
        } = handle;
        let scene = scene.as_mut().ok_or(Failure::NoScene)?;
        let ViewportPixels {
            width,
            height,
            scale_factor,
        } = scene.viewport;
        let needed = width as usize * height as usize * 4;
        if rgba.is_null() {
            return Err(Failure::Invalid("rgba is null".into()));
        }
        if length < needed {
            return Err(Failure::BufferTooSmall(needed));
        }
        // One renderer per size and density.
        if renderer
            .as_ref()
            .is_some_and(|r| r.size() != (width, height) || r.scale_factor() != scale_factor)
        {
            *renderer = None;
        }
        if renderer.is_none() {
            *renderer = Some(OffscreenChartRenderer::new(width, height, scale_factor)?);
        }
        let image = renderer.as_mut().expect("created above").render(
            engine,
            &mut scene.scene,
            scene.zoom,
        )?;
        let pixels = image.as_raw();
        ensure_len(pixels.len(), needed)?;
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), rgba, needed);
        Ok(())
    })
}

fn ensure_len(actual: usize, expected: usize) -> Result<()> {
    ensure!(actual == expected, "Rendered image has an unexpected size");
    Ok(())
}

/// Features under pixel (`x`, `y`) of the prepared scene within `radius`
/// pixels, best first, as a JSON array of feature reports (free with
/// [`ferrite_string_free`]); null on failure.
#[no_mangle]
pub unsafe extern "C" fn ferrite_engine_pick_json(
    handle: *mut FerriteEngine,
    x: f64,
    y: f64,
    radius: f32,
) -> *mut c_char {
    string_result(|| {
        let handle = engine(handle)?;
        if !(x.is_finite() && y.is_finite() && radius.is_finite() && radius >= 0.) {
            return Err(Failure::Invalid("Invalid pick position or radius".into()));
        }
        let scene = handle.scene.as_ref().ok_or(Failure::NoScene)?;
        let hits = handle.engine.pick(&scene.scene, [x, y], radius, None);
        let mut seen = std::collections::HashSet::new();
        let mut reports = Vec::new();
        for hit in &hits {
            let id: ferrite_engine::chart_engine::FeatureId = hit.into();
            if !seen.insert((id.cell_index, id.feature_id)) {
                continue;
            }
            if let Some(report) = handle.engine.feature(id) {
                reports.push(feature_json(&report));
            }
        }
        Ok(serde_json::Value::Array(reports).to_string())
    })
}

/// Opened datasets (with feature counts by type), the issues and notices of
/// every open so far and the exchange-set checks, as JSON (free with
/// [`ferrite_string_free`]); null on failure.
#[no_mangle]
pub unsafe extern "C" fn ferrite_engine_report_json(handle: *mut FerriteEngine) -> *mut c_char {
    string_result(|| {
        let handle = engine(handle)?;
        let mut report = inspection_json(&handle.engine, &handle.opened);
        let (issues, notices) = handle.engine.exchange_set_checks();
        report["exchange_set"] = serde_json::json!({
            "issues": issues.iter().map(issue_json).collect::<Vec<_>>(),
            "notices": notices,
        });
        Ok(report.to_string())
    })
}

/// Number of opened datasets, or a negative status.
#[no_mangle]
pub unsafe extern "C" fn ferrite_engine_dataset_count(handle: *mut FerriteEngine) -> i64 {
    let mut count = 0i64;
    let code = status(|| {
        count = engine(handle)?.engine.datasets().len() as i64;
        Ok(())
    });
    if code == FERRITE_OK {
        count
    } else {
        i64::from(code)
    }
}
