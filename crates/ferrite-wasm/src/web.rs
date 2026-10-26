//! wasm-bindgen surface: [`WebEngine`] and [`WebCanvas`].
use crate::core::{parse_bundle, WebEngineCore};
use ferrite_cli::SceneEmitter;
use ferrite_engine::platform::Clock;
use ferrite_gpu::headless::{HeadlessSurface, HeadlessUi};
use ferrite_gpu::{PhysicalSize, SurfaceHost, WgpuRenderer};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

fn js_error(error: anyhow::Error) -> JsError {
    JsError::new(&format!("{error:#}"))
}

/// Wall clock of the page (`Date.now()`).
struct JsClock;
impl Clock for JsClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(js_sys::Date::now().max(0.) as u64)
    }
}

/// Logs Rust panics to the browser console before the module traps.
#[wasm_bindgen(start)]
pub fn start() {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&format!("ferrite-wasm panic: {info}").into());
    }));
}

/// An S-100 chart engine in the page.
#[wasm_bindgen]
pub struct WebEngine {
    core: WebEngineCore,
}

#[wasm_bindgen]
impl WebEngine {
    /// An engine over catalogue files: an array of `[name, Uint8Array]`
    /// pairs named relative to the catalogue folder (`FC/S-101/...`,
    /// `PC/S-101/...`, optionally `FC/S-124`, `PC/S-124`, `FC/S-129`,
    /// `PC/S-129`).
    #[wasm_bindgen(constructor)]
    pub fn new(catalogue_files: js_sys::Array) -> Result<WebEngine, JsError> {
        let mut files = Vec::with_capacity(catalogue_files.length() as usize);
        for entry in catalogue_files.iter() {
            let pair: js_sys::Array = entry
                .dyn_into()
                .map_err(|_| JsError::new("Catalogue entries must be [name, bytes] pairs"))?;
            let name = pair
                .get(0)
                .as_string()
                .ok_or_else(|| JsError::new("Catalogue file name must be a string"))?;
            let bytes = js_sys::Uint8Array::new(&pair.get(1)).to_vec();
            files.push((name, bytes));
        }
        Self::with_files(files)
    }

    /// An engine over a catalogue bundle (`catalogues.bin` written by
    /// `tools/build_wasm.py`).
    pub fn from_bundle(bundle: &[u8]) -> Result<WebEngine, JsError> {
        Self::with_files(parse_bundle(bundle).map_err(js_error)?)
    }

    fn with_files(files: Vec<(String, Vec<u8>)>) -> Result<WebEngine, JsError> {
        Ok(Self {
            core: WebEngineCore::new(files, Arc::new(JsClock)).map_err(js_error)?,
        })
    }

    /// Catalogue notes of the optional overlay products (JSON array).
    pub fn catalogue_notes(&self) -> String {
        serde_json::json!(self.core.engine().catalogue_errors()).to_string()
    }

    /// Queue a dataset file (S-101 cell or update, S-124/S-129 GML,
    /// exchange catalogue) for the next [`WebEngine::open`].
    pub fn add_file(&mut self, path: &str, bytes: Vec<u8>) -> Result<(), JsError> {
        self.core.add_file(path, bytes).map_err(js_error)
    }

    pub fn pending_files(&self) -> usize {
        self.core.pending_files()
    }

    /// Store a precomputed S-101 cell payload (`.ferrite_pre_*`).
    pub fn load_precomputed(&mut self, name: &str, bytes: &[u8]) -> Result<(), JsError> {
        self.core.load_precomputed(name, bytes).map_err(js_error)
    }

    /// Names of the cache entries the next portrayal reads that the page's
    /// cache does not hold (JSON array): the S-102 rule instructions, the
    /// precomputed payloads of the opened cells and, when the preparation
    /// portrays with Lua, the per-cell Lua portrayal caches for the current
    /// display. When payload names (`.ferrite_pre_*`) are listed, call again
    /// after loading them (whether Lua runs depends on what was found). A
    /// page with a persistent cache (IndexedDB) loads them with
    /// [`WebEngine::load_cache_entry`] before [`WebEngine::set_view`] /
    /// [`WebEngine::prepare`] and saves [`WebEngine::take_cache_writes`]
    /// (and the payloads it was given) afterwards.
    pub fn missing_cache_entries(&mut self) -> String {
        serde_json::json!(self.core.missing_cache_entries()).to_string()
    }

    /// Estimated instruction count from which only the cells near the view
    /// are portrayed (residency; 0 always); negative or NaN restores the
    /// engine default.
    pub fn set_residency_threshold(&mut self, threshold: f64) {
        self.core
            .set_residency_threshold((threshold >= 0.).then_some(threshold as usize));
    }

    /// Store a cache entry saved by an earlier page (`.ferrite_*` name; a
    /// `.ferrite_pre_*` payload counts as loaded).
    pub fn load_cache_entry(&mut self, name: &str, bytes: &[u8]) -> Result<(), JsError> {
        self.core.load_cache_entry(name, bytes).map_err(js_error)
    }

    /// [`WebEngine::load_cache_entry`] with the SHA-256 of the entry's
    /// payload (`bytes.subarray(40)`, e.g. from `crypto.subtle.digest`).
    /// When it matches the hash stored in the entry, the engine does not
    /// hash the payload again in WebAssembly (the bulk of a cache-hit
    /// preparation); otherwise the entry is checked as usual.
    pub fn load_cache_entry_digest(
        &mut self,
        name: &str,
        bytes: &[u8],
        payload_sha256: &[u8],
    ) -> Result<(), JsError> {
        self.core
            .load_cache_entry_checked(name, bytes, Some(payload_sha256))
            .map_err(js_error)
    }

    /// Cache entries the engine wrote since the last call, as an array of
    /// `[name, Uint8Array]` pairs to persist.
    pub fn take_cache_writes(&self) -> js_sys::Array {
        self.core
            .take_cache_writes()
            .into_iter()
            .map(|(name, bytes)| {
                let pair = js_sys::Array::new();
                pair.push(&JsValue::from_str(&name));
                pair.push(&js_sys::Uint8Array::from(bytes.as_slice()));
                JsValue::from(pair)
            })
            .collect()
    }

    /// Whether a view change can re-project the current scene (no portrayal).
    pub fn scene_current(&self) -> bool {
        self.core.scene_current()
    }

    /// Open the queued files; this open's report as JSON.
    pub fn open(&mut self) -> Result<String, JsError> {
        self.core.open().map_err(js_error)
    }

    /// All opened datasets, issues, notices and exchange-set checks (JSON).
    pub fn report(&self) -> String {
        self.core.report_json()
    }

    /// Colour profiles of the portrayal catalogue (JSON array).
    pub fn palettes(&self) -> String {
        self.core.palettes_json()
    }

    pub fn set_palette(&mut self, name: &str) -> Result<(), JsError> {
        self.core.set_palette(name).map_err(js_error)
    }

    /// Portray for a `width` x `height` physical-pixel view at `density`
    /// device pixels per CSS pixel, centred on (`center_lat`, `center_lon`)
    /// (NaN: data centre). `scale_or_zoom` is the zoom factor over the
    /// all-data view, or with `is_scale` the scale denominator. Scene
    /// summary as JSON (instruction count, zoom, view and data bounds).
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &mut self,
        width: u32,
        height: u32,
        density: f64,
        center_lat: f64,
        center_lon: f64,
        scale_or_zoom: f64,
        is_scale: bool,
    ) -> Result<String, JsError> {
        self.core
            .prepare(
                width,
                height,
                density,
                center_lat,
                center_lon,
                scale_or_zoom,
                is_scale,
            )
            .map_err(js_error)
    }

    /// Move to a view (arguments as [`WebEngine::prepare`]): re-projects the
    /// current scene when only the camera or size changed and the resident
    /// cells stay the same, otherwise portrays again. The summary's
    /// `reprojected` says which.
    #[allow(clippy::too_many_arguments)]
    pub fn set_view(
        &mut self,
        width: u32,
        height: u32,
        density: f64,
        center_lat: f64,
        center_lon: f64,
        scale_or_zoom: f64,
        is_scale: bool,
    ) -> Result<String, JsError> {
        self.core
            .set_view(
                width,
                height,
                density,
                center_lat,
                center_lon,
                scale_or_zoom,
                is_scale,
            )
            .map_err(js_error)
    }

    /// S-101 portrayal: `"auto"` (default: Lua when built and no payloads
    /// were loaded, else precomputed payloads with Lua for missing ones),
    /// `"lua"` or `"precomputed"`.
    pub fn set_portrayal(&mut self, choice: &str) -> Result<(), JsError> {
        self.core.set_portrayal(choice).map_err(js_error)
    }

    /// `{choice, lua_available, payloads, used}` (JSON); `used` is the
    /// source of the current scene's S-101 instructions.
    pub fn portrayal(&self) -> String {
        self.core.portrayal_json()
    }

    /// Whether this build portrays S-101 with Lua (feature `lua-wasm`).
    pub fn lua_available() -> bool {
        crate::core::LUA_AVAILABLE
    }

    /// Raw Part 9a output of the Lua rules for every opened S-101 cell
    /// (see `WebEngineCore::part9a_dump`; parity with native builds).
    #[cfg(feature = "lua-wasm")]
    pub fn part9a_dump(&self) -> Result<String, JsError> {
        self.core.part9a_dump().map_err(js_error)
    }

    /// Features at physical pixel (`x`, `y`) within `radius` (JSON array).
    pub fn pick_json(&mut self, x: f64, y: f64, radius: f32) -> Result<String, JsError> {
        self.core.pick_json(x, y, radius).map_err(js_error)
    }

    /// One feature report; `cell_index` undefined for S-124/S-129 objects.
    pub fn feature_json(
        &mut self,
        cell_index: Option<u32>,
        feature_id: f64,
    ) -> Result<String, JsError> {
        self.core
            .feature_json(cell_index, feature_id as i64)
            .map_err(js_error)
    }

    /// Draw the prepared scene into `canvas` (sized to the prepared view).
    pub fn render_to_canvas(&mut self, canvas: &mut WebCanvas) -> Result<(), JsError> {
        let (engine, scene, zoom, [width, height]) = self
            .core
            .scene_mut()
            .ok_or_else(|| JsError::new("No prepared scene"))?;
        canvas.resize(width, height);
        canvas
            .emitter
            .emit(&mut canvas.renderer, engine, scene, zoom)
            .map_err(js_error)?;
        canvas
            .renderer
            .render()
            .map_err(|e| JsError::new(&e.to_string()))
    }
}

/// ferrite-gpu presenting to an HTML canvas, or to an `OffscreenCanvas`
/// (e.g. in a Web Worker).
#[wasm_bindgen]
pub struct WebCanvas {
    renderer: WgpuRenderer<HeadlessUi>,
    surface: Arc<HeadlessSurface>,
    emitter: SceneEmitter,
    /// An offscreen canvas has no page layout; [`WebCanvas::resize`] sizes it.
    offscreen: Option<web_sys::OffscreenCanvas>,
}

fn backends(backend: Option<&str>) -> Result<wgpu::Backends, JsError> {
    Ok(match backend.unwrap_or("auto") {
        "auto" => wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL,
        "webgpu" => wgpu::Backends::BROWSER_WEBGPU,
        "webgl2" => wgpu::Backends::GL,
        other => {
            return Err(JsError::new(&format!(
                "Unknown backend {other:?} (auto, webgpu, webgl2)"
            )))
        }
    })
}

#[wasm_bindgen]
impl WebCanvas {
    /// The renderer of canvas element `canvas_id` at `density` device pixels
    /// per CSS pixel (the canvas's `width`/`height` are its pixel size).
    /// `backend` is `"auto"` (default: WebGPU when the browser offers it,
    /// else WebGL2), `"webgpu"` or `"webgl2"`.
    pub async fn create(
        canvas_id: String,
        density: f64,
        backend: Option<String>,
    ) -> Result<WebCanvas, JsError> {
        let backends = backends(backend.as_deref())?;
        let canvas: web_sys::HtmlCanvasElement = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id(&canvas_id))
            .ok_or_else(|| JsError::new(&format!("No element #{canvas_id}")))?
            .dyn_into()
            .map_err(|_| JsError::new(&format!("#{canvas_id} is not a canvas")))?;
        let size = PhysicalSize::new(canvas.width().max(1), canvas.height().max(1));
        Self::with_target(
            wgpu::SurfaceTarget::Canvas(canvas),
            size,
            density,
            backends,
            None,
        )
        .await
    }

    /// The renderer of an `OffscreenCanvas` (one transferred to a Web Worker
    /// with `transferControlToOffscreen()`), at `density` device pixels per
    /// CSS pixel; `backend` as in [`WebCanvas::create`]. WebGPU needs
    /// `navigator.gpu` in the worker; WebGL2 works on offscreen canvases.
    pub async fn from_offscreen(
        canvas: web_sys::OffscreenCanvas,
        density: f64,
        backend: Option<String>,
    ) -> Result<WebCanvas, JsError> {
        let backends = backends(backend.as_deref())?;
        let size = PhysicalSize::new(canvas.width().max(1), canvas.height().max(1));
        Self::with_target(
            wgpu::SurfaceTarget::OffscreenCanvas(canvas.clone()),
            size,
            density,
            backends,
            Some(canvas),
        )
        .await
    }

    async fn with_target(
        target: wgpu::SurfaceTarget<'static>,
        size: PhysicalSize<u32>,
        density: f64,
        backends: wgpu::Backends,
        offscreen: Option<web_sys::OffscreenCanvas>,
    ) -> Result<WebCanvas, JsError> {
        let surface = Arc::new(HeadlessSurface::new(size, density));
        let host = Arc::clone(&surface);
        let renderer = WgpuRenderer::new_with_backends(
            target,
            Arc::clone(&surface) as Arc<dyn SurfaceHost>,
            backends,
            |device, _| HeadlessUi::new(device, host),
        )
        .await
        .map_err(|e| JsError::new(&format!("No GPU for the canvas: {e}")))?;
        Ok(Self {
            renderer,
            surface,
            emitter: SceneEmitter::new(),
            offscreen,
        })
    }

    /// GPU adapter and backend ("WebGPU" or "WebGL2"), e.g. "ANGLE (...) (WebGL2)".
    pub fn adapter(&self) -> String {
        let state = &self.renderer.state;
        let backend = match state.adapter_backend {
            wgpu::Backend::BrowserWebGpu => "WebGPU",
            wgpu::Backend::Gl => "WebGL2",
            _ => "other",
        };
        // Browsers may hide the WebGPU adapter name.
        if state.gpu_name.is_empty() {
            backend.to_owned()
        } else {
            format!("{} ({backend})", state.gpu_name)
        }
    }

    /// Stage timings of the last frame's emission (JSON: `view_only`,
    /// `setup_ms`, `prefill_ms`, `bathymetry_ms`, `instructions_ms`).
    pub fn emit_timings(&self) -> String {
        self.emitter.timings().to_json().to_string()
    }

    /// Whether frames of a re-projected scene reuse the scene's emission
    /// set-up (default true; false sets every frame up again, to compare).
    pub fn set_view_only_reuse(&mut self, reuse: bool) {
        self.emitter.set_view_only_reuse(reuse);
    }

    /// Match the drawing buffer to `width` x `height` physical pixels.
    pub fn resize(&mut self, width: u32, height: u32) {
        let size = PhysicalSize::new(width.max(1), height.max(1));
        if let Some(canvas) = &self.offscreen {
            if canvas.width() != size.width || canvas.height() != size.height {
                canvas.set_width(size.width);
                canvas.set_height(size.height);
            }
        }
        if self.surface.inner_size() != size {
            self.surface.set_size(size);
            self.renderer.resize(size);
        }
    }
}
