//! Vertex definitions for GPU rendering

use bytemuck::{Pod, Zeroable};

/// Basic 2D vertex with position and color
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Vertex2D {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

impl Vertex2D {
    #[inline]
    pub fn new(x: f32, y: f32, color: [f32; 4]) -> Self {
        Vertex2D {
            position: [x, y],
            color,
        }
    }

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex2D>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                // position
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // color
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        }
    }
}

/// Textured vertex for symbols
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct TexturedVertex {
    pub position: [f32; 2],
    pub tex_coords: [f32; 2],
    pub color: [f32; 4],
}

impl TexturedVertex {
    #[inline]
    pub fn new(x: f32, y: f32, u: f32, v: f32, color: [f32; 4]) -> Self {
        TexturedVertex {
            position: [x, y],
            tex_coords: [u, v],
            color,
        }
    }

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TexturedVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                // position
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // tex_coords
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // color
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 4]>() as wgpu::BufferAddress,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        }
    }
}

/// Stroke vertex: geographic center and an unscaled screen-pixel offset.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct LineVertex {
    pub position: [f32; 2],
    pub offset: [f32; 2],
    pub color: [f32; 4],
}

impl LineVertex {
    #[inline]
    pub fn new(x: f32, y: f32, ox: f32, oy: f32, color: [f32; 4]) -> Self {
        Self {
            position: [x, y],
            offset: [ox, oy],
            color,
        }
    }

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        const ATTRIBUTES: [wgpu::VertexAttribute; 3] =
            wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4];
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &ATTRIBUTES,
        }
    }
}

/// Uniform buffer for view transform
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct ViewUniforms {
    /// View-projection matrix (4x4)
    pub view_proj: [[f32; 4]; 4],
    /// Viewport size
    pub viewport_size: [f32; 2],
    /// Scale factor
    pub scale: f32,
    /// Padding for alignment
    pub _padding: f32,
    /// Pan offset in screen coordinates (pixels)
    pub pan_offset: [f32; 2],
    /// GPU zoom scale (1.0 = no zoom, >1 = zoomed in)
    pub zoom_scale: f32,
    /// Independent vertical zoom; occupies the former padding slot.
    pub zoom_scale_y: f32,
    /// Zoom pivot point in screen coordinates
    pub zoom_pivot: [f32; 2],
    /// Padding for 16-byte alignment
    pub _padding3: [f32; 2],
}

impl ViewUniforms {
    #[inline]
    pub fn new(width: f32, height: f32, scale: f32) -> Self {
        Self::with_pan_zoom(width, height, scale, 0.0, 0.0, 1.0, 0.0, 0.0)
    }

    #[inline]
    pub fn with_pan(width: f32, height: f32, scale: f32, pan_x: f32, pan_y: f32) -> Self {
        Self::with_pan_zoom(width, height, scale, pan_x, pan_y, 1.0, 0.0, 0.0)
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn with_pan_zoom(
        width: f32,
        height: f32,
        scale: f32,
        pan_x: f32,
        pan_y: f32,
        zoom_scale: f32,
        zoom_pivot_x: f32,
        zoom_pivot_y: f32,
    ) -> Self {
        // Create orthographic projection for 2D rendering
        // Maps pixel coordinates to NDC (-1 to 1)
        let view_proj = Self::orthographic(0.0, width, height, 0.0, -1.0, 1.0);

        ViewUniforms {
            view_proj,
            viewport_size: [width, height],
            scale,
            _padding: 0.0,
            pan_offset: [pan_x, pan_y],
            zoom_scale,
            zoom_scale_y: zoom_scale,
            zoom_pivot: [zoom_pivot_x, zoom_pivot_y],
            _padding3: [0.0, 0.0],
        }
    }

    /// Create orthographic projection matrix
    fn orthographic(
        left: f32,
        right: f32,
        bottom: f32,
        top: f32,
        near: f32,
        far: f32,
    ) -> [[f32; 4]; 4] {
        let width = right - left;
        let height = top - bottom;
        let depth = far - near;

        [
            [2.0 / width, 0.0, 0.0, 0.0],
            [0.0, 2.0 / height, 0.0, 0.0],
            [0.0, 0.0, -2.0 / depth, 0.0],
            [
                -(right + left) / width,
                -(top + bottom) / height,
                -(far + near) / depth,
                1.0,
            ],
        ]
    }
}
