//! GPU State Management
//!
//! Manages wgpu device, surface, and render state.

use std::sync::Arc;
use winit::window::Window;

use crate::{Result, ViewUniforms, WgpuError};

/// MSAA sample count for antialiasing
pub const MSAA_SAMPLE_COUNT: u32 = 4;

/// GPU state containing device, queue, and surface
pub struct GpuState {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    pub size: winit::dpi::PhysicalSize<u32>,
    pub window: Arc<Window>,
    /// MSAA render target texture
    pub msaa_texture: Option<wgpu::Texture>,
    pub msaa_view: Option<wgpu::TextureView>,
    /// GPU adapter name
    pub gpu_name: String,
}

impl GpuState {
    /// Create new GPU state for the given window
    pub async fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();

        // Create instance
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        // Create surface
        let surface = instance
            .create_surface(window.clone())
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
                    required_limits: wgpu::Limits::default(),
                    memory_hints: Default::default(),
                },
                None,
            )
            .await?;

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
        // desired_maximum_frame_latency = 1 minimizes input-to-display latency:
        // the CPU starts the next frame only after the GPU finishes the previous one,
        // keeping the GPU queue shallow for the freshest possible input state.
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);

        // Create MSAA texture
        let (msaa_texture, msaa_view) = Self::create_msaa_texture(&device, &config);

        tracing::info!("MSAA enabled with {} samples", MSAA_SAMPLE_COUNT);

        Ok(GpuState {
            surface,
            device,
            queue,
            config,
            size,
            window,
            msaa_texture: Some(msaa_texture),
            msaa_view: Some(msaa_view),
            gpu_name,
        })
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

    /// Resize the surface
    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width > 0 && new_size.height > 0 {
            self.size = new_size;
            self.config.width = new_size.width;
            self.config.height = new_size.height;
            self.surface.configure(&self.device, &self.config);

            // Recreate MSAA texture with new size
            let (msaa_texture, msaa_view) = Self::create_msaa_texture(&self.device, &self.config);
            self.msaa_texture = Some(msaa_texture);
            self.msaa_view = Some(msaa_view);
        }
    }

    /// Get current surface texture for rendering
    pub fn get_current_texture(&self) -> Result<wgpu::SurfaceTexture> {
        self.surface
            .get_current_texture()
            .map_err(|e| WgpuError::Render(e.to_string()))
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
        self.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: bytemuck::cast_slice(indices),
                usage: wgpu::BufferUsages::INDEX,
            })
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
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            pixels,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }
}
