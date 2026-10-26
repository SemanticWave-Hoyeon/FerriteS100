//! S-98 Level 2 suppression of raster feature layers. A suppressed raster
//! stays visible only outside the area where the replacing vector product's
//! data coverage is displayed (S-98 Part B B-5.1): the device mask is the
//! chart viewport minus the union of the committed coverage frame's dataset
//! masks, applied with the shared fragment clip.
use crate::coverage_clip::{
    create_clip_layout, fragment_clipped_shader, ClipTransform, CoverageClip, FragmentEntry,
};
use crate::{RasterVertex, Result, WgpuError};
use ferrite_kernel::coverage_raster::{rasterize, PixelMask};
use ferrite_kernel::coverage_selection::Region;

/// Device-mask byte budget (one R8 texel per pixel of an 8K surface).
const MASK_BYTE_BUDGET: usize = 8192 * 8192;

/// Coverage owner identity, surface extent and chart viewport bits.
type ClipKey = (usize, [u32; 2], [u32; 4]);

#[derive(Default)]
pub(crate) struct RasterSuppression {
    enabled: bool,
    pipeline: Option<wgpu::RenderPipeline>,
    clip_layout: Option<wgpu::BindGroupLayout>,
    /// (coverage owner identity, extent, viewport bits) -> uploaded clip.
    clip: Option<(ClipKey, CoverageClip)>,
}

/// Visible area of a suppressed raster: viewport pixels not covered by any
/// replacing dataset's mask.
pub(crate) fn visible_outside(
    viewport: &Region,
    extent: [u32; 2],
    covered: &[&PixelMask],
) -> Result<PixelMask> {
    fn error(e: impl std::fmt::Display) -> WgpuError {
        WgpuError::Render(e.to_string())
    }
    let mut visible = rasterize(viewport, extent, MASK_BYTE_BUDGET).map_err(error)?;
    let owned: Vec<PixelMask> = covered.iter().map(|m| (*m).clone()).collect();
    let union = PixelMask::union_masks(&owned, MASK_BYTE_BUDGET).map_err(error)?;
    visible.subtract(&union);
    Ok(visible)
}

impl RasterSuppression {
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.clip = None;
        }
    }
    /// Draw unclipped until the next successful mask.
    pub(crate) fn reset_clip(&mut self) {
        self.clip = None;
    }
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }
    /// Pipeline and clip for this frame; None draws rasters unclipped.
    pub(crate) fn binding(&self) -> Option<(&wgpu::RenderPipeline, &wgpu::BindGroup)> {
        Some((self.pipeline.as_ref()?, &self.clip.as_ref()?.1.bind_group))
    }
    fn pipeline(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        samples: u32,
        view_layout: &wgpu::BindGroupLayout,
        texture_layout: &wgpu::BindGroupLayout,
        clip_layout: &wgpu::BindGroupLayout,
    ) -> Result<wgpu::RenderPipeline> {
        let source = fragment_clipped_shader(
            crate::pipeline::RASTER_SHADER,
            2,
            &[FragmentEntry {
                name: "fs_main",
                input_type: "Output",
                position_field: "position",
            }],
        )?;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("suppressed-raster"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("suppressed-raster-layout"),
            bind_group_layouts: &[view_layout, texture_layout, clip_layout],
            push_constant_ranges: &[],
        });
        let blend = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        };
        Ok(
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("suppressed-raster"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[RasterVertex::desc()],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState {
                            color: blend,
                            alpha: blend,
                        }),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: samples,
                    ..Default::default()
                },
                multiview: None,
                cache: None,
            }),
        )
    }
    /// Rebuild the device mask when the committed coverage owner, surface or
    /// chart viewport changes; otherwise only refresh the affine transform.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare(
        &mut self,
        state: &crate::GpuState,
        pipelines: &crate::RenderPipelines,
        coverage: Option<&std::sync::Arc<ferrite_render::PreparedCoverage>>,
        viewport: Option<ferrite_render::Viewport>,
        extent: [u32; 2],
        transform: ClipTransform,
    ) -> Result<()> {
        let (true, Some(coverage), Some(v)) = (self.enabled, coverage, viewport) else {
            self.clip = None;
            return Ok(());
        };
        if self.clip_layout.is_none() {
            let layout = create_clip_layout(&state.device);
            self.pipeline = Some(Self::pipeline(
                &state.device,
                state.format(),
                crate::state::MSAA_SAMPLE_COUNT,
                &pipelines.view_bind_group_layout,
                &pipelines.texture_bind_group_layout,
                &layout,
            )?);
            self.clip_layout = Some(layout);
        }
        let key = (
            std::sync::Arc::as_ptr(coverage) as usize,
            extent,
            [v.x, v.y, v.width, v.height].map(f32::to_bits),
        );
        if let Some((cached, clip)) = &mut self.clip {
            if *cached == key {
                clip.set_transform(&state.queue, transform);
                return Ok(());
            }
        }
        let mut covered = Vec::new();
        for pass in 0..coverage.pass_count() {
            let pass = coverage
                .pass(pass)
                .map_err(|e| WgpuError::Render(e.to_string()))?;
            covered.extend(pass.frame().visible_masks());
        }
        let [x, y, right, bottom] = [v.x, v.y, v.x + v.width, v.y + v.height].map(f64::from);
        let region = Region::from_rings(
            &[[x, y], [right, y], [right, bottom], [x, bottom], [x, y]],
            &[],
        )
        .map_err(|e| WgpuError::Render(e.to_string()))?;
        let visible = visible_outside(&region, extent, &covered)?;
        let clip = CoverageClip::new(
            &state.device,
            &state.queue,
            self.clip_layout.as_ref().expect("created above"),
            Some(&visible),
            transform,
            MASK_BYTE_BUDGET,
        )?;
        self.clip = Some((key, clip));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Region {
        Region::from_rings(&[[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]], &[]).unwrap()
    }
    #[test]
    fn suppressed_raster_is_visible_only_outside_replacing_coverage() {
        let extent = [100, 50];
        let covered = rasterize(&rect(0., 0., 40., 50.), extent, MASK_BYTE_BUDGET).unwrap();
        let visible = visible_outside(&rect(0., 0., 100., 50.), extent, &[&covered]).unwrap();
        assert!(!visible.contains_pixel(10, 10));
        assert!(!visible.contains_pixel(39, 49));
        assert!(visible.contains_pixel(60, 10));
        // Nothing covered: the whole viewport stays visible.
        let all = visible_outside(&rect(0., 0., 100., 50.), extent, &[]).unwrap();
        assert!(all.contains_pixel(0, 0) && all.contains_pixel(99, 49));
    }
}
