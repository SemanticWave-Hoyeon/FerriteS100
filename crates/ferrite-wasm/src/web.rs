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

/// ferrite-gpu presenting to an HTML canvas.
#[wasm_bindgen]
pub struct WebCanvas {
    renderer: WgpuRenderer<HeadlessUi>,
    surface: Arc<HeadlessSurface>,
    emitter: SceneEmitter,
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
        let backends = match backend.as_deref().unwrap_or("auto") {
            "auto" => wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL,
            "webgpu" => wgpu::Backends::BROWSER_WEBGPU,
            "webgl2" => wgpu::Backends::GL,
            other => {
                return Err(JsError::new(&format!(
                    "Unknown backend {other:?} (auto, webgpu, webgl2)"
                )))
            }
        };
        let canvas: web_sys::HtmlCanvasElement = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id(&canvas_id))
            .ok_or_else(|| JsError::new(&format!("No element #{canvas_id}")))?
            .dyn_into()
            .map_err(|_| JsError::new(&format!("#{canvas_id} is not a canvas")))?;
        let size = PhysicalSize::new(canvas.width().max(1), canvas.height().max(1));
        let surface = Arc::new(HeadlessSurface::new(size, density));
        let host = Arc::clone(&surface);
        let renderer = WgpuRenderer::new_with_backends(
            wgpu::SurfaceTarget::Canvas(canvas),
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

    /// Match the drawing buffer to `width` x `height` physical pixels.
    pub fn resize(&mut self, width: u32, height: u32) {
        let size = PhysicalSize::new(width.max(1), height.max(1));
        if self.surface.inner_size() != size {
            self.surface.set_size(size);
            self.renderer.resize(size);
        }
    }
}
