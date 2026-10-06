//! Natural upright motif GPU material for the actual surface Scene pipeline.
//! This does not decide geographic site containment or authorize a Pane command.
use crate::{
    globe_pattern::{AreaMaterialKind, PatternMaterial, PatternParams},
    whole_motif::NaturalMotifResource,
};
use wgpu::util::DeviceExt;

pub struct NaturalMotifTexture {
    view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    pub resource_key: String,
    pub rgba_bytes: usize,
    pub has_coverage: bool,
    dimensions: [u32; 2],
    bitmap_origin: [f64; 2],
}
/// Distinct from a Periodic material. Shared Scene shader/layout/blend/coverage
/// and cropped-ID sampling are intentional; the sampler and phase are NOT Repeat.
pub struct NaturalMotifMaterial {
    scene: PatternMaterial,
}
impl NaturalMotifMaterial {
    pub fn scene_material(&self) -> &PatternMaterial {
        &self.scene
    }
}

fn natural_params(
    dimensions: [u32; 2],
    bitmap_origin: [f64; 2],
    site: [f64; 2],
    extent: [f64; 2],
) -> Result<PatternParams, String> {
    if dimensions.contains(&0)
        || !bitmap_origin
            .iter()
            .chain(site.iter())
            .all(|v| v.is_finite())
        || !extent.iter().all(|v| v.is_finite() && *v > 0.)
    {
        return Err("Invalid natural motif physical mapping".into());
    }
    let mut rows = [[0.; 4]; 2];
    for i in 0..2 {
        let size = f64::from(dimensions[i]);
        let origin = site[i] + bitmap_origin[i];
        let vb = origin - site[i];
        let addition_error = ((site[i] - (origin - vb)) + (bitmap_origin[i] - vb)).abs();
        let inverse = 1. / size;
        let phase = -origin / size;
        rows[i][i] = inverse as f32;
        rows[i][2] = phase as f32;
        let magnitude = f64::from(rows[i][i]).abs() * extent[i] + f64::from(rows[i][2]).abs();
        let error = ((f64::from(rows[i][i]) - inverse).abs() * extent[i]
            + (f64::from(rows[i][2]) - phase).abs()
            + 4. * f64::from(f32::EPSILON) * magnitude)
            * size
            + addition_error;
        if !origin.is_finite()
            || !rows[i].iter().all(|v| v.is_finite())
            || !error.is_finite()
            || error > 0.125
        {
            return Err("Natural motif GPU affine precision budget exceeded".into());
        }
    }
    Ok(PatternParams {
        row0: rows[0],
        row1: rows[1],
    })
}
impl NaturalMotifTexture {
    pub fn upload(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        resource: &NaturalMotifResource,
    ) -> Result<Self, String> {
        let edge = device.limits().max_texture_dimension_2d;
        let [width, height] = [resource.width, resource.height];
        if width == 0 || height == 0 || width > edge || height > edge {
            return Err("Natural motif texture exceeds device dimension budget".into());
        }
        let rgba_bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .filter(|n| *n == resource.pixels.len() && *n <= 64 * 1024 * 1024)
            .ok_or("Invalid natural motif RGBA payload")?;
        if !resource.bitmap_origin_px.iter().all(|v| v.is_finite()) {
            return Err("Invalid natural motif bitmap origin".into());
        }
        // Resource fields are publicly inspectable; verify alpha and edge contract
        // at the upload boundary rather than trusting a caller-mutated has_coverage.
        let w = width as usize;
        let h = height as usize;
        let alpha = |x: usize, y: usize| resource.pixels[(y * w + x) * 4 + 3];
        if (0..w).any(|x| alpha(x, 0) != 0 || alpha(x, h - 1) != 0)
            || (0..h).any(|y| alpha(0, y) != 0 || alpha(w - 1, y) != 0)
        {
            return Err("Natural motif GPU transparent guard violated".into());
        }
        if !resource
            .pixels
            .chunks_exact(4)
            .all(|p| p[..3].iter().all(|c| *c <= p[3]))
        {
            return Err("Natural motif GPU requires premultiplied RGBA".into());
        }
        let has_coverage = resource.pixels.chunks_exact(4).any(|p| p[3] != 0);
        if has_coverage != resource.has_coverage {
            return Err("Natural motif GPU alpha availability mismatch".into());
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("S100 independent natural motif"),
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
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &resource.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            texture.size(),
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("S100 natural motif clamp sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Ok(Self {
            view: texture.create_view(&Default::default()),
            sampler,
            resource_key: resource.resource_key.clone(),
            rgba_bytes,
            has_coverage,
            dimensions: [width, height],
            bitmap_origin: resource.bitmap_origin_px,
        })
    }
    /// Site is in local physical pane coordinates, as in the Scene shared sample.
    /// Pick query-origin correction is applied by the existing actual ID shader.
    pub fn material(
        &self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        site: [f64; 2],
        extent: [f64; 2],
    ) -> Result<NaturalMotifMaterial, String> {
        let params = natural_params(self.dimensions, self.bitmap_origin, site, extent)?;
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("S100 independent motif physical origin"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("S100 natural whole motif material"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&self.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: buffer.as_entire_binding(),
                },
            ],
        });
        Ok(NaturalMotifMaterial {
            scene: PatternMaterial {
                bind_group,
                params,
                kind: AreaMaterialKind::WholeMotif,
            },
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_mapping_retains_origin_without_period_reduction_or_shear() {
        let p = natural_params([12, 8], [-2., -2.], [10., 20.], [64., 64.]).unwrap();
        assert_eq!(p.row0[1], 0.);
        assert_eq!(p.row1[0], 0.);
        let uv = |xy: [f64; 2]| {
            [
                f64::from(p.row0[0]) * xy[0] + f64::from(p.row0[2]),
                f64::from(p.row1[1]) * xy[1] + f64::from(p.row1[2]),
            ]
        };
        let near = |a: f64, b: f64| assert!((a - b).abs() < 1e-6);
        for (a, b) in uv([8., 18.]).into_iter().zip([0., 0.]) {
            near(a, b);
        }
        for (a, b) in uv([20., 26.]).into_iter().zip([1., 1.]) {
            near(a, b);
        }
        assert_ne!(
            p,
            natural_params([12, 8], [-2., -2.], [22., 20.], [64., 64.]).unwrap()
        );
        assert!(natural_params([0, 8], [0., 0.], [0., 0.], [64., 64.]).is_err());
        assert!(natural_params([12, 8], [0., 0.], [1e16, 0.], [64., 64.]).is_err());
        assert!(natural_params([12, 8], [0., 0.], [0., 0.], [f64::NAN, 64.]).is_err());
    }
}
