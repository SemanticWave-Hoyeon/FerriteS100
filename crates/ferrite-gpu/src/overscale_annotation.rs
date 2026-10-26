//! Non-pickable S-98 annotation: no invented ENC source, instruction or ID.
use crate::coverage_clip::{
    create_clip_layout, fragment_clipped_shader, ClipTransform, CoverageClip, FragmentEntry,
};
use crate::{PatternVertex, Result, WgpuError};
use ferrite_kernel::coverage_frame::FrameScaleAnnotations;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use wgpu::util::DeviceExt;
pub(crate) struct OverscaleAnnotation {
    pipeline: AnnotationPipeline,
    _texture: wgpu::Texture,
    asset: wgpu::BindGroup,
    clip: CoverageClip,
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    extent: [u32; 2],
}
// Only shader/pipeline/layout objects are shared. R8, source bitmap, bindings,
// phase, geometry and pick/coverage rights remain privately prepared each time.
struct Program {
    pipeline: wgpu::RenderPipeline,
    clip_layout: wgpu::BindGroupLayout,
}
enum AnnotationPipeline {
    Owned(wgpu::RenderPipeline),
    Shared(Arc<Program>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProgramKey {
    device: wgpu::Device,
    view: wgpu::BindGroupLayout,
    pattern: wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
    samples: u32,
}
impl ProgramKey {
    fn capture(state: &crate::GpuState, pipelines: &crate::RenderPipelines) -> Self {
        Self {
            device: state.device.clone(),
            view: pipelines.view_bind_group_layout.clone(),
            pattern: pipelines.pattern_bind_group_layout.clone(),
            format: state.format(),
            samples: crate::state::MSAA_SAMPLE_COUNT,
        }
    }
    fn matches(&self, state: &crate::GpuState, pipelines: &crate::RenderPipelines) -> bool {
        self.device == state.device
            && self.view == pipelines.view_bind_group_layout
            && self.pattern == pipelines.pattern_bind_group_layout
            && self.format == state.format()
            && self.samples == crate::state::MSAA_SAMPLE_COUNT
    }
}
fn program_reuse_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}
#[derive(Default)]
struct ImmutableSlot<K, V>(Mutex<Option<(K, V)>>);
impl<K: PartialEq, V: Clone> ImmutableSlot<K, V> {
    fn get_matching(&self, matches: impl FnOnce(&K) -> bool) -> Option<V> {
        let held = self.0.lock().ok()?;
        held.as_ref()
            .filter(|(key, _)| matches(key))
            .map(|(_, value)| value.clone())
    }
    fn publish(&self, key: K, value: V) {
        if let Ok(mut held) = self.0.lock() {
            *held = Some((key, value));
        }
    }
}
pub(crate) struct ProgramReuse {
    enabled: bool,
    slot: ImmutableSlot<ProgramKey, Arc<Program>>,
    lookups: AtomicU64,
    hits: AtomicU64,
    creations: AtomicU64,
}
impl ProgramReuse {
    pub(crate) fn new(value: Option<&std::ffi::OsStr>) -> Self {
        Self {
            enabled: program_reuse_enabled(value),
            slot: ImmutableSlot(Mutex::new(None)),
            lookups: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            creations: AtomicU64::new(0),
        }
    }
    fn lookup(
        &self,
        state: &crate::GpuState,
        pipelines: &crate::RenderPipelines,
    ) -> Option<Arc<Program>> {
        if !self.enabled {
            return None;
        }
        self.lookups.fetch_add(1, Ordering::Relaxed);
        let program = self.slot.get_matching(|key| key.matches(state, pipelines));
        if program.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
        program
    }
    fn publish(
        &self,
        state: &crate::GpuState,
        pipelines: &crate::RenderPipelines,
        program: Arc<Program>,
    ) {
        if !self.enabled {
            return;
        }
        self.creations.fetch_add(1, Ordering::Relaxed);
        self.slot
            .publish(ProgramKey::capture(state, pipelines), program);
    }
    pub(crate) fn statistics(&self) -> serde_json::Value {
        serde_json::json!({"enabled":self.enabled,"lookups":self.lookups.load(Ordering::Relaxed),"hits":self.hits.load(Ordering::Relaxed),"pipeline_creations":self.creations.load(Ordering::Relaxed),"scope":"renderer lifetime HOST calls, immutable pipeline only; no GPU duration"})
    }
}
fn failure(s: impl Into<String>) -> WgpuError {
    WgpuError::Render(s.into())
}
fn shared_shader() -> Result<String> {
    let source = crate::pipeline::PATTERN_FILL_SHADER;
    let x = "(pos.x - in.tile_params.z * pos.y) * in.tile_params.x";
    let y = "pos.y * in.tile_params.y";
    if source.matches(x).count() != 1 || source.matches(y).count() != 1 {
        return Err(failure("OVERSC01 shared shader contract changed"));
    }
    // OVERSC01 has a rectangular 9x4mm lattice. zw stores the common
    // geometry anchor reduced modulo the physical period in f64, not shear.
    let source = source
        .replace(x, "(pos.x - in.tile_params.z) * in.tile_params.x")
        .replace(y, "(pos.y - in.tile_params.w) * in.tile_params.y");
    fragment_clipped_shader(
        &source,
        2,
        &[FragmentEntry {
            name: "fs_main",
            input_type: "PatternVertexOutput",
            position_field: "clip_position",
        }],
    )
}
fn phase(anchor: [f64; 2], period: [f64; 2]) -> Result<[f32; 2]> {
    if !anchor.iter().all(|v| v.is_finite())
        || !period.iter().all(|v| v.is_finite() && *v > 0.)
        || anchor
            .iter()
            .zip(period)
            .any(|(a, p)| (a / p).abs() > 2_f64.powi(40))
    {
        return Err(failure("Invalid OVERSC01 geometric phase"));
    }
    Ok([
        anchor[0].rem_euclid(period[0]) as f32,
        anchor[1].rem_euclid(period[1]) as f32,
    ])
}
impl OverscaleAnnotation {
    pub(crate) fn prepare(
        annotations: &[&FrameScaleAnnotations],
        scaler: &ferrite_render::Scaler,
        symbols: &mut crate::SymbolCache,
        profile: &ferrite_portrayal_catalog::ColorProfile,
        state: &crate::GpuState,
        pipelines: &crate::RenderPipelines,
        program_reuse: Option<&ProgramReuse>,
    ) -> Result<Option<Self>> {
        const MASK_BUDGET: usize = 128 * 1024 * 1024;
        let mut masks = Vec::new();
        let mut bytes = 0usize;
        let extent = [state.size.width, state.size.height];
        for annotation in annotations {
            if annotation.physical_extent() != extent {
                return Err(failure("OVERSC01 stale physical coverage extent"));
            }
            if let Some(mask) = annotation
                .overscale_pattern_mask(MASK_BUDGET.saturating_sub(bytes))
                .map_err(|e| failure(e.to_string()))?
            {
                bytes = bytes
                    .checked_add(mask.pixels().len())
                    .ok_or_else(|| failure("OVERSC01 pass-mask overflow"))?;
                masks.push(mask);
            }
        }
        if masks.is_empty() {
            return Ok(None);
        }
        let mask = ferrite_kernel::coverage_raster::PixelMask::union_masks(
            &masks,
            MASK_BUDGET.saturating_sub(bytes),
        )
        .map_err(|e| failure(e.to_string()))?;
        Self::prepare_mask(
            mask,
            extent,
            scaler,
            symbols,
            profile,
            state,
            pipelines,
            program_reuse,
        )
    }
    pub(crate) fn prepare_owned(
        annotations: &[&FrameScaleAnnotations],
        scaler: &ferrite_render::Scaler,
        resources: &mut crate::CellPortrayalResources,
        state: &crate::GpuState,
        pipelines: &crate::RenderPipelines,
        program_reuse: Option<&ProgramReuse>,
    ) -> Result<Vec<Self>> {
        const MASK_BUDGET: usize = 128 * 1024 * 1024;
        const TEXTURE_BUDGET: usize = 64 * 1024 * 1024;
        let extent = [state.size.width, state.size.height];
        let mut groups: std::collections::BTreeMap<
            u64,
            (usize, Vec<ferrite_kernel::coverage_raster::PixelMask>),
        > = std::collections::BTreeMap::new();
        let mut retained = 0usize;
        for annotation in annotations {
            if annotation.physical_extent() != extent {
                return Err(failure("OVERSC01 stale mixed-owner extent"));
            }
            let partitions = annotation
                .overscale_pattern_masks_by_dataset(MASK_BUDGET.saturating_sub(retained))
                .map_err(|e| failure(e.to_string()))?;
            for (dataset, mut mask) in partitions {
                // Across longitude passes, one existing physical fragment can
                // receive annotation once. Source rights remain original global OR.
                for (_, masks) in groups.values() {
                    for prior in masks {
                        mask.subtract(prior);
                    }
                }
                if !mask.pixels().iter().any(|p| *p != 0) {
                    continue;
                }
                retained = retained
                    .checked_add(mask.pixels().len())
                    .ok_or_else(|| failure("Mixed annotation mask bytes overflow"))?;
                if retained > MASK_BUDGET {
                    return Err(failure("Mixed annotation mask budget exceeded"));
                }
                let resolved = resources.resolve_mut(Some(dataset))?;
                let owner = resolved.cache.resource_revision();
                groups
                    .entry(owner)
                    .or_insert_with(|| (dataset, Vec::new()))
                    .1
                    .push(mask);
            }
        }
        let mut plans = Vec::new();
        let mut texture_bytes = 0usize;
        // Complete bitmap/device/aggregate admission before the first GPU
        // allocation. A later owner's error cannot publish a retained prefix.
        for (_, (dataset, masks)) in groups {
            let previous_bytes = masks
                .iter()
                .try_fold(0usize, |n, m| n.checked_add(m.pixels().len()))
                .ok_or_else(|| failure("Mixed annotation owner mask overflow"))?;
            let mask = ferrite_kernel::coverage_raster::PixelMask::union_masks(
                &masks,
                MASK_BUDGET.saturating_sub(retained),
            )
            .map_err(|e| failure(e.to_string()))?;
            let joined_bytes = mask.pixels().len();
            drop(masks);
            retained = retained
                .checked_sub(previous_bytes)
                .and_then(|n| n.checked_add(joined_bytes))
                .ok_or_else(|| failure("Mixed annotation mask ledger overflow"))?;
            let resolved = resources.resolve_mut(Some(dataset))?;
            let definition = resolved
                .cache
                .overscale_pattern_definition()
                .map_err(failure)?;
            let ppm = (96. / 25.4) * state.scale_factor();
            let lattice = ferrite_render::PatternLattice::from_mm((9., 0.), (0., 4.), ppm)
                .map_err(|e| failure(e.to_string()))?;
            let bitmap = resolved
                .cache
                .get_symbol_for_lattice(definition.symbol_ref(), resolved.profile, lattice, ppm)
                .map_err(failure)?;
            let expected = (bitmap.width as usize)
                .checked_mul(bitmap.height as usize)
                .and_then(|n| n.checked_mul(4))
                .ok_or_else(|| failure("Mixed annotation bitmap dimensions overflow"))?;
            if expected != bitmap.pixels.len()
                || expected > 16 * 1024 * 1024
                || bitmap.width > state.device.limits().max_texture_dimension_2d
                || bitmap.height > state.device.limits().max_texture_dimension_2d
            {
                return Err(failure("Mixed annotation bitmap admission failed"));
            }
            texture_bytes = texture_bytes
                .checked_add(expected)
                .ok_or_else(|| failure("Mixed annotation texture overflow"))?;
            if texture_bytes > TEXTURE_BUDGET {
                return Err(failure(
                    "Mixed annotation aggregate texture budget exceeded",
                ));
            }
            plans.push((dataset, mask));
        }
        let mut output = Vec::new();
        for (dataset, mask) in plans {
            let resolved = resources.resolve_mut(Some(dataset))?;
            if let Some(annotation) = Self::prepare_mask(
                mask,
                extent,
                scaler,
                resolved.cache,
                resolved.profile,
                state,
                pipelines,
                program_reuse,
            )? {
                output.push(annotation);
            }
        }
        Ok(output)
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Keep original mask/owner/scaler/device inputs independent and optional immutable program reuse"
    )]
    fn prepare_mask(
        mask: ferrite_kernel::coverage_raster::PixelMask,
        extent: [u32; 2],
        scaler: &ferrite_render::Scaler,
        symbols: &mut crate::SymbolCache,
        profile: &ferrite_portrayal_catalog::ColorProfile,
        state: &crate::GpuState,
        pipelines: &crate::RenderPipelines,
        program_reuse: Option<&ProgramReuse>,
    ) -> Result<Option<Self>> {
        const MASK_BUDGET: usize = 128 * 1024 * 1024;
        if !mask.pixels().iter().any(|v| *v != 0) {
            return Ok(None);
        }
        let definition = symbols.overscale_pattern_definition().map_err(failure)?;
        let ppm = (96. / 25.4) * state.scale_factor();
        let lattice = ferrite_render::PatternLattice::from_mm((9., 0.), (0., 4.), ppm)
            .map_err(|e| failure(e.to_string()))?;
        let bitmap = symbols
            .get_symbol_for_lattice(definition.symbol_ref(), profile, lattice, ppm)
            .map_err(failure)?;
        let bytes = (bitmap.width as usize)
            .checked_mul(bitmap.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| failure("OVERSC01 texture size overflow"))?;
        if bitmap.width > state.device.limits().max_texture_dimension_2d
            || bitmap.height > state.device.limits().max_texture_dimension_2d
            || bytes > 16 * 1024 * 1024
            || bytes != bitmap.pixels.len()
        {
            return Err(failure("OVERSC01 texture payload budget exceeded"));
        }
        let period = [9. * ppm, 4. * ppm];
        let t = scaler.flat_transform();
        // Common WGS84 (longitude0, latitude0) geometry anchor, shared across
        // every coverage owner. Keep physical mm lattice through navigation.
        let anchor = [
            (0. - t.geographic_origin[0]) * t.scale[0] + t.offset[0],
            (t.projection.project_y(t.geographic_origin[1]) - t.projection.project_y(0.))
                * t.scale[1]
                + t.offset[1],
        ];
        let origin = phase(anchor, period)?;
        let inv = [(1. / period[0]) as f32, (-1. / period[1]) as f32];
        let vertex = |x, y| PatternVertex {
            position: [x, y],
            tile_params: [inv[0], inv[1], origin[0], origin[1]],
        };
        let vertices = [
            vertex(0., 0.),
            vertex(extent[0] as f32, 0.),
            vertex(extent[0] as f32, extent[1] as f32),
            vertex(0., extent[1] as f32),
        ];
        let indices = [0u32, 1, 2, 0, 2, 3];
        let cached_program = program_reuse.and_then(|reuse| reuse.lookup(state, pipelines));
        let cold_clip_layout = cached_program
            .is_none()
            .then(|| create_clip_layout(&state.device));
        let clip_layout = cached_program
            .as_ref()
            .map(|program| &program.clip_layout)
            .or(cold_clip_layout.as_ref())
            .expect("cached or fresh clip layout");
        let clip = CoverageClip::new(
            &state.device,
            &state.queue,
            clip_layout,
            Some(&mask),
            ClipTransform::IDENTITY,
            MASK_BUDGET,
        )?;
        let (_texture, view) = state.create_texture_from_rgba(
            &bitmap.pixels,
            bitmap.width,
            bitmap.height,
            "OVERSC01-active-PC",
        );
        let asset = pipelines.create_pattern_bind_group(&state.device, &view);
        let mut fresh_program = None;
        let pipeline = if let Some(program) = cached_program.as_ref() {
            AnnotationPipeline::Shared(Arc::clone(program))
        } else {
            let pipeline = create_program_pipeline(state, pipelines, clip_layout)?;
            if program_reuse.is_some_and(|reuse| reuse.enabled) {
                let program = Arc::new(Program {
                    pipeline,
                    clip_layout: cold_clip_layout.expect("fresh program clip layout"),
                });
                fresh_program = Some(Arc::clone(&program));
                AnnotationPipeline::Shared(program)
            } else {
                AnnotationPipeline::Owned(pipeline)
            }
        };
        let vertices = state
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("OVERSC01-quad"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
        let indices = state
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("OVERSC01-indices"),
                contents: bytemuck::cast_slice(&indices),
                usage: wgpu::BufferUsages::INDEX,
            });
        let output = Self {
            pipeline,
            _texture,
            asset,
            clip,
            vertices,
            indices,
            extent,
        };
        // Publish only after the full independent annotation construction returns.
        if let (Some(reuse), Some(program)) = (program_reuse, fresh_program) {
            reuse.publish(state, pipelines, program);
        }
        Ok(Some(output))
    }
    pub(crate) fn encode<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        view: &'a wgpu::BindGroup,
        current_extent: [u32; 2],
    ) {
        if self.extent != current_extent {
            return;
        }
        let pipeline = match &self.pipeline {
            AnnotationPipeline::Owned(pipeline) => pipeline,
            AnnotationPipeline::Shared(program) => &program.pipeline,
        };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, view, &[]);
        pass.set_bind_group(1, &self.asset, &[]);
        pass.set_bind_group(2, &self.clip.bind_group, &[]);
        pass.set_vertex_buffer(0, self.vertices.slice(..));
        pass.set_index_buffer(self.indices.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..6, 0, 0..1);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn phase_wraps_common_anchor_without_changing_physical_period() {
        assert_eq!(phase([-9., 8.], [9., 4.]).unwrap(), [0., 0.]);
        assert_eq!(phase([9.5, -0.5], [9., 4.]).unwrap(), [0.5, 3.5]);
        assert!(phase([f64::INFINITY, 0.], [9., 4.]).is_err());
    }
    #[test]
    fn shader_reuses_sampler_derivatives_and_exact_r8_wrapper() {
        let s = shared_shader().unwrap();
        assert!(s.contains("textureSample(t_pattern, s_pattern, uv)"));
        assert!(s.contains("(pos.y - in.tile_params.w)"));
        assert!(s.contains("s100_clip_visible"));
        assert!(!s.contains("in.tile_params.z * pos.y"));
        let module = wgpu::naga::front::wgsl::parse_str(&s).unwrap();
        wgpu::naga::valid::Validator::new(
            wgpu::naga::valid::ValidationFlags::all(),
            wgpu::naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap();
    }
}

fn create_program_pipeline(
    state: &crate::GpuState,
    pipelines: &crate::RenderPipelines,
    clip_layout: &wgpu::BindGroupLayout,
) -> Result<wgpu::RenderPipeline> {
    let shader = state
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("OVERSC01-shared-pattern"),
            source: wgpu::ShaderSource::Wgsl(shared_shader()?.into()),
        });
    let layout = state
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("OVERSC01-layout"),
            bind_group_layouts: &[
                &pipelines.view_bind_group_layout,
                &pipelines.pattern_bind_group_layout,
                clip_layout,
            ],
            push_constant_ranges: &[],
        });
    let pipeline = state
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("OVERSC01-non-pickable"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[PatternVertex::desc()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: state.format(),
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: crate::state::MSAA_SAMPLE_COUNT,
                ..Default::default()
            },
            multiview: None,
            cache: None,
        });

    Ok(pipeline)
}

#[cfg(test)]
mod program_reuse_tests {
    use super::*;
    #[test]
    fn policy_exact_one_and_empty_slot_decline() {
        assert!(program_reuse_enabled(Some(std::ffi::OsStr::new("1"))));
        for v in [
            None,
            Some(std::ffi::OsStr::new("0")),
            Some(std::ffi::OsStr::new("true")),
        ] {
            assert!(!program_reuse_enabled(v));
        }
        let slot: ImmutableSlot<u32, Arc<Vec<u8>>> = ImmutableSlot(Mutex::new(None));
        assert!(slot.get_matching(|key| *key == 1).is_none());
    }
    #[test]
    fn slot_is_bounded_and_retained_previous_immutable_value_survives_replace() {
        let slot: ImmutableSlot<u32, Arc<Vec<u8>>> = ImmutableSlot(Mutex::new(None));
        slot.publish(1, Arc::new(vec![1, 2]));
        let old = slot.get_matching(|key| *key == 1).unwrap();
        assert!(slot.get_matching(|key| *key == 2).is_none());
        slot.publish(2, Arc::new(vec![3, 4]));
        assert_eq!(&*old, &[1, 2]);
        assert_eq!(&*slot.get_matching(|key| *key == 2).unwrap(), &[3, 4]);
        assert!(slot.get_matching(|key| *key == 1).is_none());
    }
    #[test]
    fn poisoned_slot_declines_without_bypassing_cold_path() {
        let slot: ImmutableSlot<u32, Arc<Vec<u8>>> = ImmutableSlot(Mutex::new(None));
        let _ = std::panic::catch_unwind(|| {
            let _held = slot.0.lock().unwrap();
            panic!("benign test poison");
        });
        assert!(slot.get_matching(|_| true).is_none());
        slot.publish(1, Arc::new(vec![1]));
        assert!(slot.get_matching(|_| true).is_none());
    }
}
