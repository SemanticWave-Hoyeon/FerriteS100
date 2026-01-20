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

/// Line vertex with width attribute
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct LineVertex {
    pub position: [f32; 2],
    pub normal: [f32; 2],
    pub color: [f32; 4],
    pub width: f32,
    pub _padding: f32,
}

impl LineVertex {
    #[inline]
    pub fn new(x: f32, y: f32, nx: f32, ny: f32, color: [f32; 4], width: f32) -> Self {
        LineVertex {
            position: [x, y],
            normal: [nx, ny],
            color,
            width,
            _padding: 0.0,
        }
    }

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<LineVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                // position
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // normal
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
                // width
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 8]>() as wgpu::BufferAddress,
                    shader_location: 3,
                    format: wgpu::VertexFormat::Float32,
                },
            ],
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
    /// Padding for 16-byte alignment
    pub _padding2: [f32; 2],
}

impl ViewUniforms {
    #[inline]
    pub fn new(width: f32, height: f32, scale: f32) -> Self {
        Self::with_pan(width, height, scale, 0.0, 0.0)
    }

    #[inline]
    pub fn with_pan(width: f32, height: f32, scale: f32, pan_x: f32, pan_y: f32) -> Self {
        // Create orthographic projection for 2D rendering
        // Maps pixel coordinates to NDC (-1 to 1)
        let view_proj = Self::orthographic(0.0, width, height, 0.0, -1.0, 1.0);

        ViewUniforms {
            view_proj,
            viewport_size: [width, height],
            scale,
            _padding: 0.0,
            pan_offset: [pan_x, pan_y],
            _padding2: [0.0, 0.0],
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
