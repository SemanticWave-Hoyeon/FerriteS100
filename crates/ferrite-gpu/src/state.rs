//! GPU State Management
//!
//! Manages wgpu device, surface, and render state.

use std::sync::Arc;

use crate::{PhysicalSize, Result, SurfaceHost, ViewUniforms, WgpuError};

/// MSAA sample count for antialiasing
pub const MSAA_SAMPLE_COUNT: u32 = 4;

/// GPU state containing device, queue, and surface
pub struct GpuState {
    /// Shared only so a worker shadow can exist; only the owner presents.
    /// None for an offscreen (headless) state.
    pub surface: Option<Arc<wgpu::Surface<'static>>>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    pub size: PhysicalSize<u32>,
    pub window: Arc<dyn SurfaceHost>,
    /// Window density sampled on the UI thread (creation and resize). Scene
    /// emission reads this; a worker must never call into the macOS window,
    /// which synchronously dispatches to the main thread.
    scale_factor: f64,
    /// MSAA render target texture
    pub msaa_texture: Option<wgpu::Texture>,
    pub msaa_view: Option<wgpu::TextureView>,
    /// GPU adapter name
    pub gpu_name: String,
    /// Actual adapter capability metadata; device request remains unchanged.
    pub adapter_features: wgpu::Features,
    pub adapter_backend: wgpu::Backend,
    surface_pacing: Option<std::cell::RefCell<crate::surface_pacing::Capture>>,
    /// Buffers created off the UI thread for exact CPU slices of a scene that
    /// is about to be uploaded; consumed by `create_*_buffer`, else dropped.
    prebuilt: std::cell::RefCell<Vec<PrebuiltBuffer>>,
}

/// A GPU buffer already initialised from the slice at `address`/`len`.
/// Identity is the live allocation, valid only until that slice can change.
pub(crate) struct PrebuiltBuffer {
    label: &'static str,
    usage: wgpu::BufferUsages,
    address: usize,
    len: usize,
    buffer: wgpu::Buffer,
}

impl GpuState {
    /// Create new GPU state presenting to `target`, which belongs to `window`.
    pub async fn new(
        target: wgpu::SurfaceTarget<'static>,
        window: Arc<dyn SurfaceHost>,
    ) -> Result<Self> {
        let size = window.inner_size();

        // Create instance
        let instance = Self::instance().await;

        // Create surface
        let surface = instance
            .create_surface(target)
            .map_err(|e| WgpuError::SurfaceCreation(e.to_string()))?;

        // Request adapter
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or(WgpuError::AdapterNotFound)?;

        let adapter_info = adapter.get_info();
        let gpu_name = adapter_info.name.clone();
        tracing::info!("Using GPU adapter: {:?}", gpu_name);

        let (device, queue, adapter_features) = Self::request_device(&adapter).await?;

        // Configure surface
        // Use non-sRGB format to avoid automatic gamma correction
        // PC color profiles define colors in sRGB space (for direct display)
        // Using sRGB surface would apply gamma correction twice
        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| !f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        // Use Fifo (VSync) to match monitor refresh rate — no wasted frames.
        // Default latency hint remains 1. Exact opt-in value 2 is experimental.
        // Depending on backend, hint 1 may serialize CPU/GPU at acquisition;
        // hint 2 may reduce starvation while adding latency/in-flight resources.
        // Neither the hint nor a hidden surface proves physical display cadence.
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: crate::surface_pacing::requested_latency(
                std::env::var_os("FERRITE_SURFACE_FRAME_LATENCY").as_deref(),
            ),
        };
        surface.configure(&device, &config);

        // Create MSAA texture
        let (msaa_texture, msaa_view) = Self::create_msaa_texture(&device, &config);

        tracing::info!("MSAA enabled with {} samples", MSAA_SAMPLE_COUNT);

        Ok(GpuState {
            surface: Some(Arc::new(surface)),
            device,
            queue,
            config,
            size,
            window: Arc::clone(&window),
            scale_factor: window.scale_factor(),
            msaa_texture: Some(msaa_texture),
            msaa_view: Some(msaa_view),
            gpu_name,
            adapter_features,
            adapter_backend: adapter_info.backend,
            surface_pacing: crate::surface_pacing::Capture::new(
                std::env::var_os("FERRITE_SURFACE_PACING_DIAGNOSTICS").as_deref(),
                crate::background_test::enabled()
                    && window.is_visible() == Some(false)
                    && !window.has_focus(),
            )
            .map(std::cell::RefCell::new),
            prebuilt: Default::default(),
        })
    }

    /// A wgpu instance on every backend. On the web WebGPU is used when the
    /// browser offers it, otherwise WebGL2.
    async fn instance() -> wgpu::Instance {
        let descriptor = wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        };
        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        {
            wgpu::util::new_instance_with_webgpu_detection(&descriptor).await
        }
        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        {
            wgpu::Instance::new(&descriptor)
        }
    }

    /// Device limits: the wgpu defaults; WebGL2 (web fallback) offers only
    /// its downlevel limits at the adapter's resolution.
    fn required_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        if adapter.get_info().backend == wgpu::Backend::Gl {
            return wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits());
        }
        let _ = adapter;
        wgpu::Limits::default()
    }

    /// The renderer's device on `adapter`, with the timestamp queries the
    /// adapter supports; also returns the adapter's features.
    async fn request_device(
        adapter: &wgpu::Adapter,
    ) -> Result<(wgpu::Device, wgpu::Queue, wgpu::Features)> {
        // Check which timestamp query features the adapter supports
        let adapter_features = adapter.features();
        let mut requested_features = wgpu::Features::empty();
        if adapter_features.contains(wgpu::Features::TIMESTAMP_QUERY) {
            requested_features |= wgpu::Features::TIMESTAMP_QUERY;
            tracing::info!("GPU supports TIMESTAMP_QUERY");
        }
        if adapter_features.contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS) {
            requested_features |= wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
            tracing::info!("GPU supports TIMESTAMP_QUERY_INSIDE_ENCODERS");
        }
        if adapter_features.contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_PASSES) {
            requested_features |= wgpu::Features::TIMESTAMP_QUERY_INSIDE_PASSES;
            tracing::info!("GPU supports TIMESTAMP_QUERY_INSIDE_PASSES");
        }

        // Request device
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("ferrite-device"),
                    required_features: requested_features,
                    required_limits: Self::required_limits(adapter),
                    memory_hints: Default::default(),
                },
                None,
            )
            .await?;
        Ok((device, queue, adapter_features))
    }

    /// GPU state without a presentation surface: frames are only rendered
    /// into offscreen textures (image export). `window` reports the size and
    /// density of the offscreen target.
    pub async fn new_headless(window: Arc<dyn SurfaceHost>) -> Result<Self> {
        let size = window.inner_size();
        let instance = Self::instance().await;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .ok_or(WgpuError::AdapterNotFound)?;
        let adapter_info = adapter.get_info();
        let gpu_name = adapter_info.name.clone();
        tracing::info!("Using GPU adapter (offscreen): {:?}", gpu_name);
        let (device, queue, adapter_features) = Self::request_device(&adapter).await?;
        // The non-sRGB format a window surface uses on the desktop platforms;
        // PC colours are already sRGB values (see `new`).
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: wgpu::TextureFormat::Bgra8Unorm,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        let (msaa_texture, msaa_view) = Self::create_msaa_texture(&device, &config);
        Ok(GpuState {
            surface: None,
            device,
            queue,
            config,
            size,
            scale_factor: window.scale_factor(),
            window,
            msaa_texture: Some(msaa_texture),
            msaa_view: Some(msaa_view),
            gpu_name,
            adapter_features,
            adapter_backend: adapter_info.backend,
            surface_pacing: None,
            prebuilt: Default::default(),
        })
    }

    /// Whether frames are presented to a window surface (false: offscreen).
    pub fn has_surface(&self) -> bool {
        self.surface.is_some()
    }

    /// Create MSAA texture and view
    fn create_msaa_texture(
        device: &wgpu::Device,
        config: &wgpu::SurfaceConfiguration,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("msaa-texture"),
            size: wgpu::Extent3d {
                width: config.width,
                height: config.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: MSAA_SAMPLE_COUNT,
            dimension: wgpu::TextureDimension::D2,
            format: config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }

    /// Read-only device/size copy for preparing a scene on a worker thread.
    /// It never acquires or presents, and carries no pacing collector.
    pub(crate) fn worker_shadow(&self) -> Self {
        Self {
            surface: self.surface.clone(),
            device: self.device.clone(),
            queue: self.queue.clone(),
            config: self.config.clone(),
            size: self.size,
            window: Arc::clone(&self.window),
            scale_factor: self.scale_factor,
            msaa_texture: None,
            msaa_view: None,
            gpu_name: self.gpu_name.clone(),
            adapter_features: self.adapter_features,
            adapter_backend: self.adapter_backend,
            surface_pacing: None,
            prebuilt: Default::default(),
        }
    }

    /// Resize the surface
    pub fn resize(&mut self, new_size: PhysicalSize<u32>) {
        self.scale_factor = self.window.scale_factor();
        if new_size.width > 0 && new_size.height > 0 {
            self.size = new_size;
            self.config.width = new_size.width;
            self.config.height = new_size.height;
            if let Some(surface) = &self.surface {
                surface.configure(&self.device, &self.config);
            }

            // Recreate MSAA texture with new size
            let (msaa_texture, msaa_view) = Self::create_msaa_texture(&self.device, &self.config);
            self.msaa_texture = Some(msaa_texture);
            self.msaa_view = Some(msaa_view);
        }
    }

    /// Get current surface texture for rendering
    pub fn get_current_texture(&self) -> Result<wgpu::SurfaceTexture> {
        let timer = self.surface_pacing_clock();
        let result = match &self.surface {
            Some(surface) => surface
                .get_current_texture()
                .map_err(|e| WgpuError::Render(e.to_string())),
            None => Err(WgpuError::Render(
                "Offscreen renderer has no window surface".into(),
            )),
        };
        self.surface_pacing_record(crate::surface_pacing::Stage::Acquire, timer);
        if let Some(capture) = &self.surface_pacing {
            capture.borrow_mut().acquire(result.is_err());
        }
        result
    }

    pub(crate) fn surface_pacing_arm(&self, frame: u64, source: u64, view: u64) {
        if let Some(capture) = &self.surface_pacing {
            capture.borrow_mut().arm(frame, source, view);
        }
    }
    pub(crate) fn surface_pacing_clock(&self) -> Option<ferrite_kernel::time::Instant> {
        self.surface_pacing
            .as_ref()
            .and_then(|capture| capture.borrow().clock())
    }
    pub(crate) fn surface_pacing_record(
        &self,
        stage: crate::surface_pacing::Stage,
        timer: Option<ferrite_kernel::time::Instant>,
    ) {
        if let (Some(capture), Some(timer)) = (&self.surface_pacing, timer) {
            let elapsed = timer.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            capture.borrow_mut().record(stage, elapsed);
        }
    }
    pub(crate) fn surface_pacing_presented(&self) {
        if let Some(capture) = &self.surface_pacing {
            capture.borrow_mut().presented();
        }
    }
    pub(crate) fn surface_pacing_finish(&self) {
        if let Some(capture) = &self.surface_pacing {
            capture.borrow_mut().finish();
        }
    }
    pub(crate) fn surface_pacing_snapshot(&self) -> Option<serde_json::Value> {
        self.surface_pacing.as_ref().map(|capture| {
            capture.borrow().snapshot(
                self.config.desired_maximum_frame_latency,
                [self.config.width, self.config.height],
            )
        })
    }

    /// Create a uniform buffer
    pub fn create_uniform_buffer<T: bytemuck::Pod>(&self, data: &T, label: &str) -> wgpu::Buffer {
        use wgpu::util::DeviceExt;
        self.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: bytemuck::cast_slice(&[*data]),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            })
    }

    /// Create a vertex buffer
    pub fn create_vertex_buffer<T: bytemuck::Pod>(
        &self,
        vertices: &[T],
        label: &str,
    ) -> wgpu::Buffer {
        use wgpu::util::DeviceExt;
        if let Some(buffer) = self.take_prebuilt(
            label,
            wgpu::BufferUsages::VERTEX,
            bytemuck::cast_slice(vertices),
        ) {
            return buffer;
        }
        self.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: bytemuck::cast_slice(vertices),
                usage: wgpu::BufferUsages::VERTEX,
            })
    }

    /// Create an index buffer
    pub fn create_index_buffer(&self, indices: &[u32], label: &str) -> wgpu::Buffer {
        use wgpu::util::DeviceExt;
        if let Some(buffer) = self.take_prebuilt(
            label,
            wgpu::BufferUsages::INDEX,
            bytemuck::cast_slice(indices),
        ) {
            return buffer;
        }
        self.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: bytemuck::cast_slice(indices),
                usage: wgpu::BufferUsages::INDEX,
            })
    }

    /// Create (on any thread) the buffer `create_*_buffer` would create for
    /// exactly this slice, and hold it for that later call.
    pub(crate) fn prebuild_buffer(
        &self,
        label: &'static str,
        usage: wgpu::BufferUsages,
        contents: &[u8],
    ) {
        use wgpu::util::DeviceExt;
        if contents.is_empty() {
            return;
        }
        let buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents,
                usage,
            });
        self.prebuilt.borrow_mut().push(PrebuiltBuffer {
            label,
            usage,
            address: contents.as_ptr() as usize,
            len: contents.len(),
            buffer,
        });
    }
    pub(crate) fn take_prebuilt_buffers(&self) -> Vec<PrebuiltBuffer> {
        self.prebuilt.take()
    }
    /// Replace held buffers; their slices must be unchanged until consumed.
    pub(crate) fn hold_prebuilt_buffers(&self, buffers: Vec<PrebuiltBuffer>) {
        *self.prebuilt.borrow_mut() = buffers;
    }
    /// Drop held buffers once their slices may change or were uploaded.
    pub(crate) fn clear_prebuilt_buffers(&self) {
        self.prebuilt.borrow_mut().clear();
    }
    fn take_prebuilt(
        &self,
        label: &str,
        usage: wgpu::BufferUsages,
        contents: &[u8],
    ) -> Option<wgpu::Buffer> {
        let mut held = self.prebuilt.borrow_mut();
        let index = held.iter().position(|b| {
            b.label == label
                && b.usage == usage
                && b.address == contents.as_ptr() as usize
                && b.len == contents.len()
        })?;
        Some(held.swap_remove(index).buffer)
    }

    /// Update view uniforms
    pub fn update_view_uniforms(&self, buffer: &wgpu::Buffer, uniforms: &ViewUniforms) {
        self.queue
            .write_buffer(buffer, 0, bytemuck::cast_slice(&[*uniforms]));
    }

    /// Get surface format
    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// Re-sample density on the UI thread before a scene emission.
    pub(crate) fn sync_scale_factor(&mut self) {
        self.scale_factor = self.window.scale_factor();
    }

    /// Window density as of creation, the last resize or the last
    /// `sync_scale_factor`.
    pub fn scale_factor(&self) -> f64 {
        self.scale_factor
    }

    /// Get viewport dimensions
    pub fn viewport_size(&self) -> (f32, f32) {
        (self.size.width as f32, self.size.height as f32)
    }

    /// Create a texture from RGBA pixel data
    pub fn create_texture_from_rgba(
        &self,
        pixels: &[u8],
        width: u32,
        height: u32,
        label: &str,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        use wgpu::util::DeviceExt;

        let texture = self.device.create_texture_with_data(
            &self.queue,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            pixels,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }
}
