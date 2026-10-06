//! Product-neutral fragment clipping from a cropped device-pixel coverage mask.
//! Vertex formats and source geometry remain unchanged. The caller assigns each
//! dataset's mask to non-point-origin draw ranges, including symbols and text
//! originating from curves or surfaces. Point-origin instructions follow S-98
//! E-1.5: occluded origins hide the entire instruction; other points are unclipped.
use crate::{Result, WgpuError};
use ferrite_kernel::coverage_raster::PixelMask;
use wgpu::util::DeviceExt;

#[derive(Debug, Clone, Copy)]
pub struct ClipTransform {
    scale: [f32; 2],
    bias: [f32; 2],
}
impl ClipTransform {
    pub const IDENTITY: Self = Self {
        scale: [1., 1.],
        bias: [0., 0.],
    };
    /// Prepared-frame pixel = current fragment pixel * scale + bias.
    /// The resource subtracts its cropped image origin after this transform.
    pub fn new(scale: [f32; 2], bias: [f32; 2]) -> Result<Self> {
        if !scale.iter().all(|v| v.is_finite() && *v > 0.) || !bias.iter().all(|v| v.is_finite()) {
            return Err(WgpuError::Render("Invalid coverage clip affine".into()));
        }
        Ok(Self { scale, bias })
    }
    pub fn prepared_pixel(&self, fragment: [f32; 2]) -> [f32; 2] {
        [
            fragment[0] * self.scale[0] + self.bias[0],
            fragment[1] * self.scale[1] + self.bias[1],
        ]
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ClipUniforms {
    mode: u32,
    padding: [u32; 3],
    scale: [f32; 2],
    bias: [f32; 2],
    size: [u32; 2],
    padding2: [u32; 2],
}

pub fn create_clip_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("dataset-coverage-clip-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(48),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Uint,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
        ],
    })
}

/// Shader source is appended to a primitive shader. Call visible() after
/// derivative-dependent texture sampling, then discard masked fragments.
pub fn clip_wgsl(group: u32) -> String {
    r#"
struct S100Clip {
 mode:u32, p0:u32, p1:u32, p2:u32,
 scale:vec2<f32>, bias:vec2<f32>, size:vec2<u32>, p3:vec2<u32>,
}

@group(GROUP) @binding(0) var<uniform> s100_clip:S100Clip;
@group(GROUP) @binding(1) var s100_mask:texture_2d<u32>;
fn s100_clip_visible(fragment:vec2<f32>)->bool {
 if s100_clip.mode==0u { return true; }
 if s100_clip.mode==2u { return false; }
 let p=floor(fragment*s100_clip.scale+s100_clip.bias);
 if any(p<vec2<f32>(0.)) || any(p>=vec2<f32>(s100_clip.size)) { return false; }
 return textureLoad(s100_mask,vec2<i32>(p),0).r!=0u;
}
"#
    .replace("GROUP", &group.to_string())
}

pub struct FragmentEntry {
    pub name: &'static str,
    pub input_type: &'static str,
    pub position_field: &'static str,
}
/// Preserve an existing fragment's complete color/alpha calculation and sample
/// derivatives, then clip its result. The source must contain exactly the given
/// entries, each returning a location-zero vec4. Validation fails explicitly if
/// a renderer shader changes its entry interface.
pub fn fragment_clipped_shader(
    source: &str,
    group: u32,
    entries: &[FragmentEntry],
) -> Result<String> {
    if entries.is_empty() || source.matches("@fragment").count() != entries.len() {
        return Err(WgpuError::Shader(
            "Coverage clip fragment entry mismatch".into(),
        ));
    }
    let mut output = source
        .replace("@fragment", "")
        .replace("-> @location(0)", "->")
        .replace("->@location(0)", "->");
    for entry in entries {
        for id in [entry.name, entry.input_type, entry.position_field] {
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(WgpuError::Shader(
                    "Invalid coverage clip entry identifier".into(),
                ));
            }
        }
        let signature = format!("fn {}(", entry.name);
        if output.matches(&signature).count() != 1 {
            return Err(WgpuError::Shader(
                "Coverage clip source entry mismatch".into(),
            ));
        }
        output = output.replace(&signature, &format!("fn s100_original_{}(", entry.name));
    }
    output.push_str(&clip_wgsl(group));
    for entry in entries {
        output.push_str(&format!("\n@fragment fn {name}(v:{ty})->@location(0) vec4<f32> {{\n let c=s100_original_{name}(v);\n if !s100_clip_visible(v.{pos}.xy) {{discard;}}\n return c;\n}}\n",name=entry.name,ty=entry.input_type,pos=entry.position_field));
    }
    Ok(output)
}

pub struct CoverageClip {
    pub bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    _texture: wgpu::Texture,
    origin: [u32; 2],
    uniforms: ClipUniforms,
    pixel_bytes: usize,
}
impl CoverageClip {
    /// None permits all fragments. An empty mask clips every fragment. Nonempty
    /// masks clip outside their cropped rectangle and every zero-valued pixel.
    /// byte_budget accounts for logical R8 texels, including the 1-byte default.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        mask: Option<&PixelMask>,
        transform: ClipTransform,
        byte_budget: usize,
    ) -> Result<Self> {
        let size = mask.map_or([0, 0], PixelMask::size);
        let origin = mask.map_or([0, 0], PixelMask::origin);
        if origin.iter().any(|v| *v > 1 << 24) {
            return Err(WgpuError::Texture(
                "Coverage origin exceeds exact device-pixel range".into(),
            ));
        }
        let mode = if mask.is_none() {
            0
        } else if size.contains(&0) {
            2
        } else {
            1
        };
        let image_size = if mode == 1 { size } else { [1, 1] };
        let limit = device.limits().max_texture_dimension_2d;
        if image_size.iter().any(|v| *v > limit) {
            return Err(WgpuError::Texture(
                "Coverage clip exceeds texture dimension limit".into(),
            ));
        }
        let pixel_bytes = (image_size[0] as usize)
            .checked_mul(image_size[1] as usize)
            .ok_or_else(|| WgpuError::Texture("Coverage texture size overflow".into()))?;
        if pixel_bytes > byte_budget {
            return Err(WgpuError::Texture(format!(
                "Coverage texture exceeds byte budget: {pixel_bytes} > {byte_budget}"
            )));
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dataset-coverage-mask"),
            size: wgpu::Extent3d {
                width: image_size[0],
                height: image_size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let data = if mode == 1 {
            mask.unwrap().pixels()
        } else {
            &[0]
        };
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(image_size[0]),
                rows_per_image: Some(image_size[1]),
            },
            wgpu::Extent3d {
                width: image_size[0],
                height: image_size[1],
                depth_or_array_layers: 1,
            },
        );
        let uniforms = ClipUniforms {
            mode,
            padding: [0; 3],
            scale: transform.scale,
            bias: [
                transform.bias[0] - origin[0] as f32,
                transform.bias[1] - origin[1] as f32,
            ],
            size,
            padding2: [0; 2],
        };
        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("dataset-coverage-clip-uniforms"),
            contents: bytemuck::bytes_of(&uniforms),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let view = texture.create_view(&Default::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("dataset-coverage-clip"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
            ],
        });
        Ok(Self {
            bind_group,
            uniform_buffer,
            _texture: texture,
            origin,
            uniforms,
            pixel_bytes,
        })
    }
    /// Reuse immutable image texels during a fast camera affine. The caller must
    /// regenerate masks for changes in selection, projection or nonaffine camera.
    pub fn set_transform(&mut self, queue: &wgpu::Queue, transform: ClipTransform) {
        self.uniforms.scale = transform.scale;
        self.uniforms.bias = [
            transform.bias[0] - self.origin[0] as f32,
            transform.bias[1] - self.origin[1] as f32,
        ];
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&self.uniforms));
    }
    pub fn pixel_bytes(&self) -> usize {
        self.pixel_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transform_is_checked_and_uniform_layout_matches_shader_offsets() {
        assert_eq!(std::mem::size_of::<ClipUniforms>(), 48);
        assert_eq!(std::mem::offset_of!(ClipUniforms, scale), 16);
        assert_eq!(std::mem::offset_of!(ClipUniforms, bias), 24);
        assert_eq!(std::mem::offset_of!(ClipUniforms, size), 32);
        for (s, b) in [
            ([0., 1.], [0., 0.]),
            ([-1., 1.], [0., 0.]),
            ([f32::NAN, 1.], [0., 0.]),
            ([1., 1.], [f32::INFINITY, 0.]),
        ] {
            assert!(ClipTransform::new(s, b).is_err());
        }
        let t = ClipTransform::new([0.5, 2.], [-3., 1.]).unwrap();
        assert_eq!(t.prepared_pixel([10., 7.]), [2., 15.]);
        assert!(clip_wgsl(2).contains("@group(2) @binding(1)"));
    }
    #[test]
    fn wrapper_preserves_source_sampling_and_rejects_entry_mismatches() {
        let source =
            "@fragment fn fs(v:Out)->@location(0) vec4<f32> {return textureSample(t,s,v.uv);}";
        let entries = [FragmentEntry {
            name: "fs",
            input_type: "Out",
            position_field: "position",
        }];
        let output = fragment_clipped_shader(source, 2, &entries).unwrap();
        assert!(output
            .contains("fn s100_original_fs(v:Out)-> vec4<f32> {return textureSample(t,s,v.uv);}"));
        assert!(output.contains("let c=s100_original_fs(v);"));
        assert_eq!(output.matches("@fragment").count(), 1);
        assert!(fragment_clipped_shader(source, 2, &[]).is_err());
        assert!(fragment_clipped_shader(
            "fn fs(v:Out)->vec4<f32>{return vec4<f32>(0.);}",
            2,
            &entries
        )
        .is_err());
        assert!(fragment_clipped_shader(
            source,
            2,
            &[FragmentEntry {
                name: "absent",
                input_type: "Out",
                position_field: "position"
            }]
        )
        .is_err());
    }
}
