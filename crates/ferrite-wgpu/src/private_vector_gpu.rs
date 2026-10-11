//! Private vector GPU staging/current-camera activation. Not an App/raster/journal transaction.
//! No live UI font atlas, glyph pool, view buffer or geometry buffer is imported.
use super::*;

const MAX_BUFFER_PAYLOAD: u64 = 128 * 1024 * 1024;
const MAX_DRAW_OBJECTS: usize = 32_768;
fn bad(message: impl Into<String>) -> WgpuError {
    WgpuError::Render(message.into())
}
#[derive(Default)]
struct BufferCharge {
    bytes: u64,
    objects: usize,
}
impl BufferCharge {
    fn admit(&mut self, bytes: u64, limit: u64) -> Result<()> {
        if bytes > limit {
            return Err(bad("Private vector buffer exceeds device limit"));
        }
        let next = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| bad("Private vector GPU payload overflow"))?;
        let objects = self
            .objects
            .checked_add(usize::from(bytes != 0))
            .ok_or_else(|| bad("Private vector GPU object overflow"))?;
        if next > MAX_BUFFER_PAYLOAD || objects > MAX_DRAW_OBJECTS {
            return Err(bad("Private vector GPU receiver budget exceeded"));
        }
        self.bytes = next;
        self.objects = objects;
        Ok(())
    }
}
/// Exclusively new buffer pair, counts are logical draw prefixes, not capacities.
struct GeometryPair {
    vertices: Option<wgpu::Buffer>,
    indices: Option<wgpu::Buffer>,
    index_count: u32,
}
fn byte_len<T>(count: usize) -> Result<u64> {
    count
        .checked_mul(std::mem::size_of::<T>())
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(|| bad("Private vector payload size overflow"))
}
fn validate_indices(vertices: usize, indices: &[u32]) -> Result<u32> {
    let count = u32::try_from(indices.len())
        .map_err(|_| bad("Private vector index count exceeds draw range"))?;
    if indices.iter().any(|&i| i as usize >= vertices) {
        return Err(bad("Private vector index outside vertex payload"));
    }
    Ok(count)
}
fn range(start: usize, end: usize, count: usize) -> Result<()> {
    if start > end || end > count || end > u32::MAX as usize {
        return Err(bad("Private vector draw range outside payload"));
    }
    Ok(())
}
fn pair<T: bytemuck::Pod>(
    state: &GpuState,
    vertices: &[T],
    indices: &[u32],
    label: &str,
    charge: &mut BufferCharge,
) -> Result<GeometryPair> {
    let index_count = validate_indices(vertices.len(), indices)?;
    charge.admit(
        byte_len::<T>(vertices.len())?,
        state.device.limits().max_buffer_size,
    )?;
    charge.admit(
        byte_len::<u32>(indices.len())?,
        state.device.limits().max_buffer_size,
    )?;
    Ok(GeometryPair {
        vertices: (!vertices.is_empty()).then(|| state.create_vertex_buffer(vertices, label)),
        indices: (!indices.is_empty()).then(|| state.create_index_buffer(indices, label)),
        index_count,
    })
}
struct PrivateVectorView {
    buffers: [wgpu::Buffer; 3],
    groups: [wgpu::BindGroup; 3],
    uniforms: [Option<ViewUniforms>; 3],
}
struct PrivateVectorGpu {
    area: GeometryPair,
    line: GeometryPair,
    pattern: GeometryPair,
    world_lines: GeometryPair,
    world_masks: GeometryPair,
    line_compact: bool,
    compact: Option<crate::exact_line_quad::Pipelines>,
    symbols: EmittedSymbolBuffers,
    quad_indices: Option<wgpu::Buffer>,
    text: Vec<GpuChartText>,
    glyphs: ChartTextBufferPool,
    view: PrivateVectorView,
    buffer_payload: u64,
}
/// Candidate-private CPU/material/font/coverage and GPU outputs; checked Renderer-only activation.
/// Ready means this vector preparation finished; App/raster expected-old checks remain separate.
pub struct ReadyVectorGpuFrame {
    prepared: PreparedVectorEmission,
    gpu: PrivateVectorGpu,
}
/// Only SinglePc preparation can construct this wrapper; no OwnedCells getters leak.
pub struct ReadySinglePcVectorGpuFrame(ReadyVectorGpuFrame);
impl ReadySinglePcVectorGpuFrame {
    pub fn buffer_payload_bytes(&self) -> u64 {
        self.0.buffer_payload_bytes()
    }
    pub fn geometry_index_counts(&self) -> [u32; 5] {
        self.0.geometry_index_counts()
    }
    pub fn text_draw_count(&self) -> usize {
        self.0.text_draw_count()
    }
    pub fn context(&self) -> &RenderContext {
        self.0.context()
    }
    pub fn settings(&self) -> VectorEmissionSettings {
        self.0.settings()
    }
    pub fn target_continuous_transform_key(&self) -> [u32; 9] {
        self.0.target_continuous_transform_key()
    }
}
pub(super) struct ExpectedPublishedVector {
    policy: [u32; 8],
    ui_settings: SettingsState,
    ui_profile: String,
    window_extent: [u32; 2],
    cpu_owner: Arc<()>,
    scene_epoch: VectorSceneEpoch,
    raster_epoch: Arc<()>,
    continuous_owner: Arc<()>,
    old_transform: [u32; 9],
    source_revision: Option<u64>,
    coverage_owner: Option<Arc<ferrite_render::PreparedCoverage>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    view_layout: wgpu::BindGroupLayout,
    texture_layout: wgpu::BindGroupLayout,
    pipelines: [wgpu::RenderPipeline; 5],
    instance: Option<wgpu::RenderPipeline>,
}
fn policy_bits(settings: VectorEmissionSettings) -> [u32; 8] {
    let c = settings.background_color.to_array();
    [
        settings.symbol_scale.to_bits(),
        u32::from(settings.show_soundings),
        u32::from(settings.animation_mode),
        u32::from(settings.show_shallow_pattern),
        c[0].to_bits(),
        c[1].to_bits(),
        c[2].to_bits(),
        c[3].to_bits(),
    ]
}
impl ExpectedPublishedVector {
    pub(super) fn capture(renderer: &WgpuRenderer) -> Self {
        let size = renderer.state.window.inner_size();
        Self {
            policy: policy_bits(renderer.vector_emission_settings()),
            ui_settings: renderer.ui_state.settings.clone(),
            ui_profile: renderer.ui_state.color_profile.clone(),
            window_extent: [size.width, size.height],
            cpu_owner: Arc::clone(&renderer.vector_emission.vector_frame.frame_cpu.owner),
            scene_epoch: renderer.vector_scene_epoch.clone(),
            raster_epoch: Arc::clone(&renderer.raster_epoch),
            continuous_owner: Arc::clone(&renderer.continuous_owner),
            old_transform: renderer.continuous_transform_key(),
            source_revision: renderer.vector_emission.triangulation_revision,
            coverage_owner: renderer
                .vector_emission
                .vector_frame
                .prepared_coverage
                .clone(),
            device: renderer.state.device.clone(),
            queue: renderer.state.queue.clone(),
            view_layout: renderer.pipelines.view_bind_group_layout.clone(),
            texture_layout: renderer.pipelines.texture_bind_group_layout.clone(),
            pipelines: [
                renderer.pipelines.area_pipeline.clone(),
                renderer.pipelines.line_pipeline.clone(),
                renderer.pipelines.pattern_fill_pipeline.clone(),
                renderer.pipelines.chart_text_pipeline.clone(),
                renderer.pipelines.texture_pipeline.clone(),
            ],
            instance: renderer.pipelines.symbol_instance_pipeline.clone(),
        }
    }
    pub(super) fn matches(&self, renderer: &WgpuRenderer) -> bool {
        let size = renderer.state.window.inner_size();
        self.scene_epoch.matches(&renderer.vector_scene_epoch)
            && self.policy == policy_bits(renderer.vector_emission_settings())
            && self.ui_settings == renderer.ui_state.settings
            && self.ui_profile == renderer.ui_state.color_profile
            && self.window_extent == [size.width, size.height]
            && Arc::ptr_eq(
                &self.cpu_owner,
                &renderer.vector_emission.vector_frame.frame_cpu.owner,
            )
            && Arc::ptr_eq(&self.raster_epoch, &renderer.raster_epoch)
            && Arc::ptr_eq(&self.continuous_owner, &renderer.continuous_owner)
            && self.old_transform == renderer.continuous_transform_key()
            && self.source_revision == renderer.vector_emission.triangulation_revision
            && same_coverage_owner(
                self.coverage_owner.as_ref(),
                renderer
                    .vector_emission
                    .vector_frame
                    .prepared_coverage
                    .as_ref(),
            )
            && self.device == renderer.state.device
            && self.queue == renderer.state.queue
            && self.view_layout == renderer.pipelines.view_bind_group_layout
            && self.texture_layout == renderer.pipelines.texture_bind_group_layout
            && self.pipelines
                == [
                    renderer.pipelines.area_pipeline.clone(),
                    renderer.pipelines.line_pipeline.clone(),
                    renderer.pipelines.pattern_fill_pipeline.clone(),
                    renderer.pipelines.chart_text_pipeline.clone(),
                    renderer.pipelines.texture_pipeline.clone(),
                ]
            && self.instance == renderer.pipelines.symbol_instance_pipeline
    }
}
impl ReadyVectorGpuFrame {
    pub fn context(&self) -> &RenderContext {
        self.prepared.context()
    }
    pub fn resource_owners(&self) -> &crate::CellPortrayalResources {
        self.prepared.resource_owners()
    }
    pub fn settings(&self) -> VectorEmissionSettings {
        self.prepared.settings()
    }
    pub fn target_continuous_transform_key(&self) -> [u32; 9] {
        self.prepared.target_continuous_transform_key()
    }
    pub fn buffer_payload_bytes(&self) -> u64 {
        self.gpu.buffer_payload
    }
    pub fn geometry_index_counts(&self) -> [u32; 5] {
        [
            self.gpu.area.index_count,
            self.gpu.line.index_count,
            self.gpu.pattern.index_count,
            self.gpu.world_lines.index_count,
            self.gpu.world_masks.index_count,
        ]
    }
    pub fn text_draw_count(&self) -> usize {
        self.gpu.text.len()
    }
    pub fn symbol_batch_count(&self) -> usize {
        self.gpu.symbols.len()
    }
}
impl WgpuRenderer {
    pub fn prepare_private_single_pc_vector_gpu(
        &self,
        prepared: PreparedSinglePcVectorEmission,
    ) -> Result<ReadySinglePcVectorGpuFrame> {
        self.prepare_private_vector_gpu(prepared.0)
            .map(ReadySinglePcVectorGpuFrame)
    }
    pub fn validate_private_single_pc_vector_gpu(
        &self,
        candidate: &ReadySinglePcVectorGpuFrame,
    ) -> Result<()> {
        self.validate_private_vector_gpu(&candidate.0)
    }
    /// Consumes a private CPU emission; shared queue writes target ONLY its new resources.
    /// No acquire, render, GPU wait/readback, live UI pass, raster setter, scene activation.
    pub fn prepare_private_vector_gpu(
        &self,
        mut prepared: PreparedVectorEmission,
    ) -> Result<ReadyVectorGpuFrame> {
        if !prepared.old_scene_epoch.matches(&self.vector_scene_epoch)
            || prepared.environment != PrivateEmissionEnvironment::capture(self)
            || !prepared.expected_old.matches(self)
        {
            return Err(bad("Private vector environment is stale"));
        }
        let window_size = self.state.window.inner_size();
        if window_size != self.state.size {
            return Err(bad("Private vector requires completed surface resize"));
        }
        let (width, height) = self.state.viewport_size();
        if !width.is_finite() || !height.is_finite() || width <= 0. || height <= 0. {
            return Err(bad("Private vector viewport is unavailable"));
        }
        validate_frame(&prepared)?;
        let metrics = prepared.environment.private_metric_source(&self.egui.ctx)?;
        let mut services = EmissionServices {
            state: &self.state,
            pipelines: &self.pipelines,
            fonts: EmissionFonts::Private(&metrics),
            cpu_profiler: None,
            overscale_program_reuse: &self.overscale_program_reuse,
            symbol_scale: prepared.settings.symbol_scale,
            show_soundings: prepared.settings.show_soundings,
            animation_mode: prepared.settings.animation_mode,
            show_shallow_pattern: prepared.settings.show_shallow_pattern,
            world_map_coastlines: &self.world_map_coastlines,
            world_map_detailed: &self.world_map_detailed,
            background_color: prepared.settings.background_color,
        };
        let mut charge = BufferCharge::default();
        // Existing private coverage uniforms are updated ONLY after affine validation.
        let transform = prepared.emission.coverage_clip_transform(&mut services)?;
        if let Some(frame) = prepared.emission.vector_frame.coverage_frame.as_mut() {
            frame.set_transform(&self.state.queue, transform);
        }
        let uniforms = prepared
            .emission
            .vector_frame
            .view_uniforms(self.state.viewport_size());
        charge.admit(
            3 * std::mem::size_of::<ViewUniforms>() as u64,
            self.state.device.limits().max_buffer_size,
        )?;
        let buffers = std::array::from_fn(|i| {
            self.state.create_uniform_buffer(
                &uniforms[i].unwrap_or(uniforms[0].expect("validated central view")),
                "private-vector-view",
            )
        });
        let groups = std::array::from_fn(|i| {
            self.pipelines
                .create_view_bind_group(&self.state.device, &buffers[i])
        });
        let view = PrivateVectorView {
            buffers,
            groups,
            uniforms,
        };
        let cpu = &prepared.emission.vector_frame.frame_cpu;
        let area_count = validate_indices(cpu.area_vertices.len(), &cpu.area_indices)?;
        charge.admit(
            byte_len::<Vertex2D>(cpu.area_vertices.len())?,
            self.state.device.limits().max_buffer_size,
        )?;
        charge.admit(
            byte_len::<u32>(cpu.area_indices.len())?,
            self.state.device.limits().max_buffer_size,
        )?;
        let area_indices = (!cpu.area_indices.is_empty()).then(|| {
            self.state
                .create_index_buffer(&cpu.area_indices, "private-area-indices")
        });
        // Fresh retained cache resources only; never borrow an old compute input/output/uniform.
        let retained = prepared.emission.retained_world_areas.prepare(
            &self.state.device,
            &self.state.queue,
            &cpu.area_vertices,
            prepared.emission.vector_frame.screen_pan_offset == (0., 0.)
                && prepared.emission.vector_frame.screen_zoom_scale == 1.
                && prepared.emission.vector_frame.screen_zoom_scale_y == 1.,
        );
        let area_vertices = if prepared.emission.retained_world_areas.active() {
            retained
        } else {
            (!cpu.area_vertices.is_empty()).then(|| {
                self.state
                    .create_vertex_buffer(&cpu.area_vertices, "private-area")
            })
        };
        let area = GeometryPair {
            vertices: area_vertices,
            indices: area_indices,
            index_count: area_count,
        };
        if !cpu.area_vertices.is_empty() && area.vertices.is_none() {
            return Err(bad("Private retained area output missing"));
        }
        let (line, line_compact, compact) =
            private_line(self, &mut prepared.emission, &mut charge)?;
        let cpu = &prepared.emission.vector_frame.frame_cpu;
        let pattern = pair(
            &self.state,
            &cpu.pattern_vertices,
            &cpu.pattern_indices,
            "private-pattern",
            &mut charge,
        )?;
        let world_lines = pair(
            &self.state,
            &cpu.world_map_line_vertices,
            &cpu.world_map_line_indices,
            "private-world-lines",
            &mut charge,
        )?;
        let world_masks = pair(
            &self.state,
            &cpu.world_map_mask_vertices,
            &cpu.world_map_mask_indices,
            "private-world-masks",
            &mut charge,
        )?;
        let (symbols, quad_indices) =
            private_symbols(self, &prepared.emission, &mut services, &mut charge)?;
        let (text, glyphs) =
            private_text(self, &mut prepared.emission, &mut services, &mut charge)?;
        prepared.emission.gpu_buffers_dirty = false;
        let gpu = PrivateVectorGpu {
            area,
            line,
            pattern,
            world_lines,
            world_masks,
            line_compact,
            compact,
            symbols,
            quad_indices,
            text,
            glyphs,
            view,
            buffer_payload: charge.bytes,
        };
        let ready = ReadyVectorGpuFrame { prepared, gpu };
        self.validate_private_vector_gpu(&ready)?;
        Ok(ready)
    }
    /// Read-only prepublication guard. It does not authorize App/raster commit.
    pub fn validate_private_vector_gpu(&self, candidate: &ReadyVectorGpuFrame) -> Result<()> {
        if candidate.prepared.environment != PrivateEmissionEnvironment::capture(self)
            || !candidate.prepared.expected_old.matches(self)
        {
            return Err(bad("Private vector publication binding changed"));
        }
        validate_frame(&candidate.prepared)?;
        if candidate.gpu.buffer_payload > MAX_BUFFER_PAYLOAD {
            return Err(bad("Private GPU payload exceeds admitted budget"));
        }
        let view = &candidate.gpu.view;
        for (i, buffer) in view.buffers.iter().enumerate() {
            if buffer.size() < std::mem::size_of::<ViewUniforms>() as u64
                || !buffer.usage().contains(wgpu::BufferUsages::UNIFORM)
            {
                return Err(bad("Private vector view buffer invalid"));
            }
            // Bind groups were constructed using these exact buffers/device/layout, never accepted as input.
            let _group = &view.groups[i];
        }
        if view.uniforms[0].is_none() {
            return Err(bad("Private vector central view absent"));
        }
        if candidate.gpu.line_compact
            && candidate.gpu.compact.as_ref().is_none_or(|c| {
                !c.matches_coverage(candidate.prepared.emission.coverage_pipelines.as_ref())
            })
        {
            return Err(bad("Private compact line layout owner changed"));
        }
        let cpu = &candidate.prepared.emission.vector_frame.frame_cpu;
        validate_pair(
            &candidate.gpu.area,
            byte_len::<Vertex2D>(cpu.area_vertices.len())?,
            byte_len::<u32>(cpu.area_indices.len())?,
        )?;
        let line_vertices = if candidate.gpu.line_compact {
            byte_len::<crate::exact_line_quad::ExactLineQuad>(cpu.line_geometry.index_len() / 6)?
        } else {
            byte_len::<LineVertex>(cpu.line_geometry.vertex_len())?
        };
        let line_indices = if candidate.gpu.line_compact {
            24
        } else {
            byte_len::<u32>(cpu.line_geometry.index_len())?
        };
        if candidate.gpu.line.index_count as usize != cpu.line_geometry.index_len() {
            return Err(bad("Private line logical draw count changed"));
        }
        validate_pair(&candidate.gpu.line, line_vertices, line_indices)?;
        validate_pair(
            &candidate.gpu.pattern,
            byte_len::<TextureVertex>(cpu.pattern_vertices.len())?,
            byte_len::<u32>(cpu.pattern_indices.len())?,
        )?;
        validate_pair(
            &candidate.gpu.world_lines,
            byte_len::<LineVertex>(cpu.world_map_line_vertices.len())?,
            byte_len::<u32>(cpu.world_map_line_indices.len())?,
        )?;
        validate_pair(
            &candidate.gpu.world_masks,
            byte_len::<Vertex2D>(cpu.world_map_mask_vertices.len())?,
            byte_len::<u32>(cpu.world_map_mask_indices.len())?,
        )?;
        for (_, _, start, end, vb, ib, ranges) in &candidate.gpu.symbols {
            range(*start, *end, cpu.symbol_instances.len())?;
            if !vb.usage().contains(wgpu::BufferUsages::VERTEX)
                || !ib.usage().contains(wgpu::BufferUsages::INDEX)
            {
                return Err(bad("Private symbol buffer usages invalid"));
            }
            let count = if self.pipelines.symbol_instance_pipeline.is_some() {
                vb.size() / 40 * 6
            } else {
                ib.size() / 4
            };
            for &(key, a, b) in ranges {
                if !candidate
                    .prepared
                    .emission
                    .symbol_textures
                    .contains_key(&key)
                    || u64::from(a) + u64::from(b) > count
                {
                    return Err(bad("Private symbol texture/range invalid"));
                }
            }
        }
        for text in &candidate.gpu.text {
            if !text.vertices.usage().contains(wgpu::BufferUsages::VERTEX)
                || !text.indices.usage().contains(wgpu::BufferUsages::INDEX)
                || u64::from(text.index_count) * 4 > text.indices.size()
            {
                return Err(bad("Private glyph draw binding invalid"));
            }
        }
        // Hold glyph pool and index handles with this scene, not a cache in the current renderer.
        let _held = (&candidate.gpu.glyphs, &candidate.gpu.quad_indices);
        Ok(())
    }
}
fn validate_pair(pair: &GeometryPair, vertices: u64, indices: u64) -> Result<()> {
    if pair.index_count != 0 && (pair.vertices.is_none() || pair.indices.is_none()) {
        return Err(bad("Private geometry draw buffers absent"));
    }
    if pair
        .vertices
        .as_ref()
        .is_some_and(|b| b.size() < vertices || !b.usage().contains(wgpu::BufferUsages::VERTEX))
        || pair
            .indices
            .as_ref()
            .is_some_and(|b| b.size() < indices || !b.usage().contains(wgpu::BufferUsages::INDEX))
    {
        return Err(bad("Private geometry GPU payload/usage invalid"));
    }
    Ok(())
}

fn validate_frame(prepared: &PreparedVectorEmission) -> Result<()> {
    let emission = &prepared.emission;
    let frame = &emission.vector_frame;
    let cpu = &frame.frame_cpu;
    if let Some(coverage) = &frame.prepared_coverage {
        coverage
            .validate(
                prepared.context.geometry_revision(),
                prepared.context.coverage_view_revision(),
                prepared.context.instruction_count(),
            )
            .map_err(|e| bad(e.to_string()))?;
    }
    if frame.coverage_failed || !frame.dependency_status.converged {
        return Err(bad("Private vector frame is incomplete"));
    }
    validate_indices(cpu.area_vertices.len(), &cpu.area_indices)?;
    if let Some((vertices, indices)) = cpu.line_geometry.legacy() {
        validate_indices(vertices.len(), indices)?;
    } else if cpu.line_geometry.packed().is_none_or(|quads| {
        quads.len().checked_mul(4) != Some(cpu.line_geometry.vertex_len())
            || quads.len().checked_mul(6) != Some(cpu.line_geometry.index_len())
    }) {
        return Err(bad("Private primary line topology is invalid"));
    }
    validate_indices(cpu.pattern_vertices.len(), &cpu.pattern_indices)?;
    validate_indices(
        cpu.world_map_line_vertices.len(),
        &cpu.world_map_line_indices,
    )?;
    validate_indices(
        cpu.world_map_mask_vertices.len(),
        &cpu.world_map_mask_indices,
    )?;
    let passes = if frame.lon_wrap_screen_px > 0. { 3 } else { 1 };
    let source = |index: Option<usize>| -> Result<()> {
        if let Some(index) = index {
            if index >= prepared.context.instruction_count() {
                return Err(bad("Private draw source outside context"));
            }
            for pass in 0..passes {
                let fixed = frame.static_source_classification.as_ref().map_or_else(
                    || frame.device_fixed_sources.contains(&index),
                    |c| c.is_device_fixed(index),
                );
                if pass != 0 && fixed {
                    continue;
                }
                let binding =
                    super::vector_scene_draw::resolve_frame_binding(frame, Some(index), pass)?;
                if matches!(
                    binding,
                    crate::coverage_gpu_frame::CoverageGpuBinding::Masked(_)
                ) && emission.coverage_pipelines.is_none()
                {
                    return Err(bad("Private masked draw pipeline missing"));
                }
            }
        }
        Ok(())
    };
    for &(_, _, a, b, s) in &cpu.area_priority_ranges {
        range(a, b, cpu.area_indices.len())?;
        source(s)?;
    }
    for &(_, _, a, b, s) in &cpu.line_priority_ranges {
        range(a, b, cpu.line_geometry.index_len())?;
        source(s)?;
    }
    for &(_, _, a, b, s) in &cpu.symbol_priority_ranges {
        range(a, b, cpu.symbol_instances.len())?;
        source(s)?;
    }
    for (_, _, a, b, key, wrap, s) in &cpu.pattern_ranges {
        range(*a, *b, cpu.pattern_indices.len())?;
        source(*s)?;
        if !matches!(wrap, 0 | 1 | 2 | 255)
            || (*b > *a && !emission.pattern_textures.contains_key(key))
        {
            return Err(bad("Private pattern owner/binding missing"));
        }
    }
    for label in &cpu.text_labels {
        source(label.source)?;
        if label.font_style.reference.is_some() != label.referenced_font.is_some() {
            return Err(bad("Private text explicit FontReference proof differs"));
        }
    }
    for instance in &cpu.symbol_instances {
        source(instance.source)?;
        if !emission
            .symbol_textures
            .contains_key(&(instance.resource_owner, instance.symbol_id))
        {
            return Err(bad("Private symbol material owner missing"));
        }
    }
    if !cpu.text_labels.is_empty() && emission.referenced_chart_owner.is_none() {
        return Err(bad("Private chart text lacks independently owned fonts"));
    }
    Ok(())
}
fn private_line(
    renderer: &WgpuRenderer,
    emission: &mut VectorEmissionOwned,
    charge: &mut BufferCharge,
) -> Result<(
    GeometryPair,
    bool,
    Option<crate::exact_line_quad::Pipelines>,
)> {
    let cpu = &mut emission.vector_frame.frame_cpu;
    if renderer.exact_line_quad_enabled {
        let owned;
        let packed = if let Some(primary) = cpu.line_geometry.packed() {
            crate::exact_line_quad::layout_admitted(
                cpu.line_geometry.vertex_len(),
                cpu.line_geometry.index_len(),
                cpu.line_priority_ranges.iter().map(|r| (r.2, r.3)),
            )
            .then_some(primary)
        } else {
            let (vertices, indices) = cpu
                .line_geometry
                .legacy()
                .ok_or_else(|| bad("Missing private line backend"))?;
            owned = crate::exact_line_quad::pack_legacy(
                vertices,
                indices,
                cpu.line_priority_ranges.iter().map(|r| (r.2, r.3)),
            );
            owned.as_deref()
        };
        if let Some(packed) = packed.filter(|p| !p.is_empty()) {
            let attempt = crate::exact_line_quad::Pipelines::new(
                &renderer.state,
                &renderer.pipelines.view_bind_group_layout,
            )
            .and_then(|mut p| {
                p.prepare_masked(
                    &renderer.state,
                    &renderer.pipelines.view_bind_group_layout,
                    emission.coverage_pipelines.as_ref(),
                )?;
                Ok(p)
            });
            if let Ok(pipelines) = attempt {
                charge.admit(
                    byte_len::<crate::exact_line_quad::ExactLineQuad>(packed.len())?,
                    renderer.state.device.limits().max_buffer_size,
                )?;
                charge.admit(24, renderer.state.device.limits().max_buffer_size)?;
                let vertices = renderer
                    .state
                    .create_vertex_buffer(packed, "private-exact-line");
                return Ok((
                    GeometryPair {
                        vertices: Some(vertices),
                        indices: Some(pipelines.indices.clone()),
                        index_count: u32::try_from(cpu.line_geometry.index_len())
                            .map_err(|_| bad("Private line count overflow"))?,
                    },
                    true,
                    Some(pipelines),
                ));
            }
        }
    }
    // Optional pipeline/representation decline materializes the WHOLE private stream.
    // Allocation failure propagates; the original packed authority is never replaced by empty data.
    cpu.line_geometry.materialize().map_err(bad)?;
    let (vertices, indices) = cpu
        .line_geometry
        .legacy()
        .ok_or_else(|| bad("Private line materialization unavailable"))?;
    Ok((
        pair(&renderer.state, vertices, indices, "private-lines", charge)?,
        false,
        None,
    ))
}
fn private_symbols(
    renderer: &WgpuRenderer,
    emission: &VectorEmissionOwned,
    services: &mut EmissionServices<'_>,
    charge: &mut BufferCharge,
) -> Result<(EmittedSymbolBuffers, Option<wgpu::Buffer>)> {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut ranges = Vec::new();
    let mut batches = Vec::new();
    let mut quad = None;
    for &(plane, priority, start, end, _) in &emission.vector_frame.frame_cpu.symbol_priority_ranges
    {
        if end <= start {
            continue;
        }
        emission.pack_symbols(
            services,
            start,
            end,
            &mut vertices,
            &mut indices,
            &mut ranges,
        );
        if indices.is_empty() {
            continue;
        }
        let (vb, ib) = if renderer.pipelines.symbol_instance_pipeline.is_some() {
            let count = vertices.len() / 4;
            let size = byte_len::<crate::symbol_instance::SymbolQuadInstance>(count)?;
            charge.admit(size, renderer.state.device.limits().max_buffer_size)?;
            let buffer = renderer
                .state
                .device
                .create_buffer(&wgpu::BufferDescriptor {
                    label: Some("private-symbol-instances"),
                    size,
                    usage: wgpu::BufferUsages::VERTEX,
                    mapped_at_creation: true,
                });
            {
                let mut mapped = buffer.slice(..).get_mapped_range_mut();
                for (out, points) in mapped
                    .as_chunks_mut::<40>()
                    .0
                    .iter_mut()
                    .zip(vertices.as_chunks::<4>().0.iter())
                {
                    out.copy_from_slice(bytemuck::bytes_of(
                        &crate::symbol_instance::SymbolQuadInstance::from_quad(points),
                    ));
                }
            }
            buffer.unmap();
            if quad.is_none() {
                charge.admit(24, renderer.state.device.limits().max_buffer_size)?;
                quad = Some(renderer.state.create_index_buffer(
                    &crate::symbol_instance::QUAD_INDICES,
                    "private-symbol-indices",
                ));
            }
            (
                buffer,
                quad.as_ref().expect("private topology initialized").clone(),
            )
        } else {
            charge.admit(
                byte_len::<TextureVertex>(vertices.len())?,
                renderer.state.device.limits().max_buffer_size,
            )?;
            charge.admit(
                byte_len::<u32>(indices.len())?,
                renderer.state.device.limits().max_buffer_size,
            )?;
            (
                renderer
                    .state
                    .create_vertex_buffer(&vertices, "private-symbols"),
                renderer
                    .state
                    .create_index_buffer(&indices, "private-symbol-indices"),
            )
        };
        if batches.len() == MAX_DRAW_OBJECTS {
            return Err(bad("Private symbol draw budget exceeded"));
        }
        batches.push((plane, priority, start, end, vb, ib, ranges.clone()));
    }
    Ok((batches, quad))
}
fn private_text(
    renderer: &WgpuRenderer,
    emission: &mut VectorEmissionOwned,
    services: &mut EmissionServices<'_>,
    charge: &mut BufferCharge,
) -> Result<(Vec<GpuChartText>, ChartTextBufferPool)> {
    if emission.vector_frame.frame_cpu.text_labels.is_empty() {
        return Ok((Vec::new(), ChartTextBufferPool::default()));
    }
    let owner = emission
        .referenced_chart_owner
        .as_ref()
        .ok_or_else(|| bad("Private font owner absent"))?;
    owner.begin_display(
        [renderer.state.size.width, renderer.state.size.height],
        renderer.state.window.scale_factor() as f32,
        f32::from_bits(PrivateEmissionEnvironment::capture(renderer).ppp),
    )?;
    let shapes = emission
        .layout_chart_text_with_owner(services, Some(owner), Vec::new(), true)
        .0;
    let owner = emission
        .referenced_chart_owner
        .as_mut()
        .ok_or_else(|| bad("Private font owner absent"))?;
    owner.end_display()?;
    // This atlas is exclusively new, even for zero-reference built-in chart families.
    owner.upload(&renderer.state.device, &renderer.state.queue)?;
    let mut groups = std::collections::BTreeMap::<
        (CompositionPlane, i32, Option<usize>, u8),
        Vec<egui::epaint::ClippedShape>,
    >::new();
    for (plane, priority, source, wrap, shape) in shapes {
        groups
            .entry((plane, priority, source, wrap))
            .or_default()
            .push(shape);
    }
    let ppp = owner.context.pixels_per_point();
    let mut glyphs = ChartTextBufferPool::default();
    let mut output = Vec::new();
    glyphs.begin();
    for ((plane, priority, source, wrap_pass), shapes) in groups {
        for job in owner.context.tessellate(shapes, ppp) {
            match job.primitive {
                egui::epaint::Primitive::Mesh(mesh) => {
                    if mesh.indices.is_empty() {
                        continue;
                    }
                    validate_indices(mesh.vertices.len(), &mesh.indices)?;
                    let Some(scissor) = chart_text_scissor(
                        job.clip_rect,
                        ppp,
                        [renderer.state.size.width, renderer.state.size.height],
                    ) else {
                        continue;
                    };
                    let vertices = chart_text_vertices(&mesh, ppp);
                    let vb = byte_len::<crate::ChartTextVertex>(vertices.len())?
                        .max(4)
                        .checked_next_power_of_two()
                        .ok_or_else(|| bad("Private glyph capacity overflow"))?;
                    let ib = byte_len::<u32>(mesh.indices.len())?
                        .max(4)
                        .checked_next_power_of_two()
                        .ok_or_else(|| bad("Private glyph capacity overflow"))?;
                    charge.admit(vb, renderer.state.device.limits().max_buffer_size)?;
                    charge.admit(ib, renderer.state.device.limits().max_buffer_size)?;
                    let bind_group = owner.bind_group(mesh.texture_id)?;
                    let (vertices, indices) = glyphs.upload(
                        &renderer.state.device,
                        &renderer.state.queue,
                        &vertices,
                        &mesh.indices,
                    );
                    if output.len() == MAX_DRAW_OBJECTS {
                        return Err(bad("Private text draw budget exceeded"));
                    }
                    output.push(GpuChartText {
                        texture_id: mesh.texture_id,
                        plane,
                        priority,
                        source,
                        wrap_pass,
                        scissor,
                        bind_group,
                        vertices,
                        indices,
                        index_count: mesh.indices.len() as u32,
                    });
                }
                egui::epaint::Primitive::Callback(_) => {
                    return Err(bad("Private chart text callback unsupported"))
                }
            }
        }
    }
    Ok((output, glyphs))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn primary_private_upload_counts_and_whole_fallback_preserve_original_bits() {
        let mut primary = crate::primary_line_geometry::Geometry::new_frame(0, 0, true);
        let mut legacy = crate::primary_line_geometry::Geometry::new_frame(0, 0, false);
        for x in [-0.0_f32, 1.25, -2000.] {
            for g in [&mut primary, &mut legacy] {
                g.append_emitted(
                    [x, -0.0],
                    [x + 4., 7.],
                    [-0.25, -2.],
                    [0.25, 2.],
                    [0., 0.3, 0.7, 0.5],
                )
                .unwrap();
            }
        }
        let packed = primary.packed().unwrap();
        assert_eq!(
            std::mem::size_of::<crate::exact_line_quad::ExactLineQuad>(),
            64
        );
        assert_eq!(
            byte_len::<crate::exact_line_quad::ExactLineQuad>(packed.len()).unwrap(),
            192
        );
        assert_eq!((primary.vertex_len(), primary.index_len()), (12, 18));
        assert!(crate::exact_line_quad::layout_admitted(
            12,
            18,
            [(0, 6), (6, 18)].into_iter()
        ));
        // Exact original fallback: not an empty stream or per-quad partial success.
        primary.materialize().unwrap();
        let (pv, pi) = primary.legacy().unwrap();
        let (lv, li) = legacy.legacy().unwrap();
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(pv),
            bytemuck::cast_slice::<_, u8>(lv)
        );
        assert_eq!(pi, li);
    }
    #[test]
    fn primary_empty_and_bad_ranges_never_become_fake_draws() {
        let mut primary = crate::primary_line_geometry::Geometry::new_frame(0, 0, true);
        assert_eq!(primary.packed().unwrap().len(), 0);
        assert!(crate::exact_line_quad::layout_admitted(
            0,
            0,
            [(0, 0)].into_iter()
        ));
        primary.materialize().unwrap();
        let (v, i) = primary.legacy().unwrap();
        assert!(v.is_empty() && i.is_empty());
        assert!(!crate::exact_line_quad::layout_admitted(
            4,
            6,
            [(1, 6)].into_iter()
        ));
        assert!(!crate::exact_line_quad::layout_admitted(
            4,
            6,
            [(0, 7)].into_iter()
        ));
    }
    #[test]
    fn ranges_and_indices_reject_without_any_device_or_scene_write() {
        assert_eq!(validate_indices(0, &[]).unwrap(), 0);
        assert!(validate_indices(1, &[1]).is_err());
        assert!(range(2, 1, 3).is_err());
        assert!(range(0, 4, 3).is_err());
        assert!(range(2, 2, 2).is_ok());
    }
    #[test]
    fn buffer_budget_is_fail_closed_without_advancing_previous_charge() {
        let mut c = BufferCharge::default();
        c.admit(MAX_BUFFER_PAYLOAD, MAX_BUFFER_PAYLOAD).unwrap();
        assert!(c.admit(1, MAX_BUFFER_PAYLOAD).is_err());
        assert_eq!(c.bytes, MAX_BUFFER_PAYLOAD);
        let mut c = BufferCharge::default();
        assert!(c.admit(9, 8).is_err());
        assert_eq!(c.objects, 0);
    }
    #[test]
    fn zero_buffers_and_overflow_keep_receiver_contract() {
        let mut c = BufferCharge::default();
        c.admit(0, 0).unwrap();
        assert_eq!(c.objects, 0);
        assert!(byte_len::<Vertex2D>(usize::MAX).is_err());
    }
    #[test]
    fn target_palette_background_is_not_an_alias_of_expected_live_policy() {
        let a = VectorEmissionSettings {
            symbol_scale: 1.,
            show_soundings: true,
            animation_mode: false,
            show_shallow_pattern: true,
            background_color: Color::BLACK,
        };
        let mut b = a;
        b.background_color = Color::WHITE;
        assert_ne!(policy_bits(a), policy_bits(b));
        b = a;
        b.show_shallow_pattern = false;
        assert_ne!(policy_bits(a), policy_bits(b));
        b = a;
        b.symbol_scale = f32::from_bits(a.symbol_scale.to_bits() + 1);
        assert_ne!(policy_bits(a), policy_bits(b));
    }
}

/// Audit-only owned pixels. Linear/encoded channel semantics follow the actual pipeline target format.
/// No application overlays, raster/native layers, GPU mask texture proof or atomic publication claim.
pub struct PrivateVectorReadback {
    pub extent: [u32; 2],
    pub rgba8: Vec<u8>,
    pub texture_format: wgpu::TextureFormat,
    pub samples: u32,
}
impl ReadyVectorGpuFrame {
    fn draw_scene<'a>(
        &'a self,
        renderer: &'a WgpuRenderer,
    ) -> super::vector_scene_draw::VectorDrawScene<'a> {
        use super::vector_scene_draw::{PairRef, VectorDrawScene};
        let g = &self.gpu;
        VectorDrawScene {
            emission: &self.prepared.emission,
            pipelines: &renderer.pipelines,
            area: PairRef {
                vertices: &g.area.vertices,
                indices: &g.area.indices,
            },
            line: PairRef {
                vertices: &g.line.vertices,
                indices: &g.line.indices,
            },
            pattern: PairRef {
                vertices: &g.pattern.vertices,
                indices: &g.pattern.indices,
            },
            world_lines: PairRef {
                vertices: &g.world_lines.vertices,
                indices: &g.world_lines.indices,
            },
            world_masks: PairRef {
                vertices: &g.world_masks.vertices,
                indices: &g.world_masks.indices,
            },
            views: [&g.view.groups[0], &g.view.groups[1], &g.view.groups[2]],
            line_compact: g.line_compact,
            compact: g.compact.as_ref(),
            symbols: &g.symbols,
            text: &g.text,
            extent: self.prepared.environment.extent,
            background: self.prepared.settings.background_color,
            draw_range_index_enabled: renderer.draw_range_index_enabled,
            raster_renderer: None,
        }
    }
}
const OFFSCREEN_BUDGET: u64 = 64 * 1024 * 1024;
struct ReadbackLayout {
    row: u32,
    padded_row: u32,
    buffer_bytes: u64,
    rgba_bytes: usize,
}
fn readback_layout(
    extent: [u32; 2],
    samples: u32,
    max_side: u32,
    max_buffer: u64,
) -> Result<ReadbackLayout> {
    let [width, height] = extent;
    if width == 0
        || height == 0
        || width > max_side
        || height > max_side
        || samples != crate::state::MSAA_SAMPLE_COUNT
    {
        return Err(bad("Private offscreen extent/sampling unsupported"));
    }
    let row = width
        .checked_mul(4)
        .ok_or_else(|| bad("Private offscreen row overflow"))?;
    let padded_row = row
        .checked_add(255)
        .map(|n| n / 256 * 256)
        .ok_or_else(|| bad("Private offscreen alignment overflow"))?;
    let buffer_bytes = u64::from(padded_row)
        .checked_mul(u64::from(height))
        .ok_or_else(|| bad("Private offscreen bytes overflow"))?;
    let image_bytes = u64::from(row)
        .checked_mul(u64::from(height))
        .ok_or_else(|| bad("Private offscreen image overflow"))?;
    // New MSAA + resolve + readback GPU payload + returned CPU copy. No old/new scene or RSS claim.
    let total = image_bytes
        .checked_mul(u64::from(samples) + 2)
        .and_then(|n| n.checked_add(buffer_bytes))
        .ok_or_else(|| bad("Private offscreen aggregate overflow"))?;
    if total > OFFSCREEN_BUDGET || buffer_bytes > max_buffer {
        return Err(bad("Private offscreen receiver budget exceeded"));
    }
    Ok(ReadbackLayout {
        row,
        padded_row,
        buffer_bytes,
        rgba_bytes: usize::try_from(image_bytes)
            .map_err(|_| bad("Private offscreen host capacity overflow"))?,
    })
}
impl WgpuRenderer {
    /// Explicit blocking GPU audit; never called by normal render/prepare. Uses wholly private attachments.
    /// The borrowed Ready owner survives all encoding/readback. No publication or old-scene repair occurs.
    /// Explicit blocking audit of the sealed SinglePc wrapper; never normal rendering.
    pub fn audit_private_single_pc_vector_frame(
        &self,
        candidate: &ReadySinglePcVectorGpuFrame,
    ) -> Result<PrivateVectorReadback> {
        self.audit_private_vector_frame(&candidate.0)
    }
    pub fn audit_private_vector_frame(
        &self,
        candidate: &ReadyVectorGpuFrame,
    ) -> Result<PrivateVectorReadback> {
        self.validate_private_vector_gpu(candidate)?;
        let frame = &candidate.prepared.emission.vector_frame;
        if let Some(coverage) = &frame.prepared_coverage {
            coverage
                .validate(
                    candidate.prepared.context.geometry_revision(),
                    candidate.prepared.context.coverage_view_revision(),
                    candidate.prepared.context.instruction_count(),
                )
                .map_err(|e| bad(e.to_string()))?;
        }
        let image = self.audit_vector_scene(
            candidate.draw_scene(self),
            Some(&candidate.prepared.emission.retained_world_areas),
        )?;
        self.validate_private_vector_gpu(candidate)?;
        Ok(image)
    }
    /// Qualified original vector-only reference; requires already uploaded displayed geometry.
    /// Mixed raster/native overlays cannot silently disappear from this comparison.
    pub fn audit_displayed_vector_frame(&self) -> Result<PrivateVectorReadback> {
        if self.vector_emission.gpu_buffers_dirty
            || !self.raster_layers.is_empty()
            || self.native_route_gpu.is_some()
        {
            return Err(bad(
                "Displayed vector audit requires ready vector-only scene",
            ));
        }
        self.audit_vector_scene(self.displayed_vector_draw_scene(), None)
    }
    /// Hidden direct regular mixed pass; NO overlay preparation, glyph rebuild or geometry upload.
    /// The vector-only component control changes a local encoder borrow, never live product visibility.
    pub fn audit_displayed_regular_mixed_frame(
        &self,
        include_rasters: bool,
    ) -> Result<PrivateVectorReadback> {
        if self.vector_emission.gpu_buffers_dirty
            || self.raster_layers.is_empty()
            || self
                .raster_layers
                .iter()
                .any(|layer| layer.continuous.is_some())
            || self.native_route_gpu.is_some()
        {
            return Err(bad(
                "Direct mixed audit requires uploaded vector and actual regular raster owners",
            ));
        }
        let mut scene = self.displayed_vector_draw_scene();
        if !include_rasters {
            scene.raster_renderer = None;
        }
        self.audit_vector_scene(scene, None)
    }
    /// Hidden direct native pass using already uploaded chart and route owners.
    /// The component control changes only a local scene borrow.
    pub fn audit_displayed_native_frame(
        &self,
        include_native: bool,
    ) -> Result<PrivateVectorReadback> {
        if self.vector_emission.gpu_buffers_dirty
            || !self.raster_layers.is_empty()
            || self.native_route_gpu.is_none()
        {
            return Err(bad(
                "Native direct audit requires uploaded chart and route owners without rasters",
            ));
        }
        let mut scene = self.displayed_vector_draw_scene();
        if !include_native {
            scene.raster_renderer = None;
        }
        self.audit_vector_scene(scene, None)
    }

    fn audit_vector_scene(
        &self,
        scene: super::vector_scene_draw::VectorDrawScene<'_>,
        retained: Option<&crate::retained_world_area::RetainedWorldAreas>,
    ) -> Result<PrivateVectorReadback> {
        if !crate::background_test::enabled()
            || self.state.window.is_visible() != Some(false)
            || self.state.window.has_focus()
        {
            return Err(bad(
                "Private vector readback requires an unfocused hidden background fixture",
            ));
        }
        let viewport = scene
            .emission
            .vector_frame
            .chart_geometry_viewport
            .unwrap_or_else(|| {
                ferrite_render::Viewport::new(scene.extent[0] as f32, scene.extent[1] as f32)
            });
        if chart_pass_scissor(viewport, scene.extent).is_none() {
            return Err(bad("Private vector target has no chart pane intersection"));
        }
        let format = self.state.format();
        if !matches!(
            format,
            wgpu::TextureFormat::Rgba8Unorm
                | wgpu::TextureFormat::Rgba8UnormSrgb
                | wgpu::TextureFormat::Bgra8Unorm
                | wgpu::TextureFormat::Bgra8UnormSrgb
        ) {
            return Err(bad("Private audit target format unsupported"));
        }
        let samples = crate::state::MSAA_SAMPLE_COUNT;
        let limits = self.state.device.limits();
        let layout = readback_layout(
            scene.extent,
            samples,
            limits.max_texture_dimension_2d,
            limits.max_buffer_size,
        )?;
        let [width, height] = scene.extent;
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = |sample_count, usage, label| {
            self.state.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        self.state
            .device
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let result = (|| -> Result<PrivateVectorReadback> {
            let output = texture(
                1,
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                "private-vector-resolve",
            );
            let resolve = output.create_view(&wgpu::TextureViewDescriptor::default());
            let msaa = texture(
                samples,
                wgpu::TextureUsages::RENDER_ATTACHMENT,
                "private-vector-msaa",
            );
            let view = msaa.create_view(&wgpu::TextureViewDescriptor::default());
            let buffer = self.state.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("private-vector-readback"),
                size: layout.buffer_bytes,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder =
                self.state
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("private-vector-audit"),
                    });
            if let Some(retained) = retained {
                retained.encode_owned_resources(&mut encoder, None);
            }
            scene.encode(&mut encoder, &view, Some(&resolve), None, None);
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &output,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(layout.padded_row),
                        rows_per_image: Some(height),
                    },
                },
                size,
            );
            self.state.queue.submit(std::iter::once(encoder.finish()));
            let slice = buffer.slice(..);
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            slice.map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
            // Audit-only, outside measured or production callbacks. No screenshot/raw-file dump.
            self.state.device.poll(wgpu::Maintain::Wait);
            rx.recv_timeout(std::time::Duration::from_secs(30))
                .map_err(|e| bad(format!("Private readback receive: {e}")))?
                .map_err(|e| bad(format!("Private readback map: {e}")))?;
            let data = slice.get_mapped_range();
            let mut rgba8 = Vec::with_capacity(layout.rgba_bytes);
            for row in 0..height {
                let start = (u64::from(row) * u64::from(layout.padded_row)) as usize;
                rgba8.extend_from_slice(&data[start..start + layout.row as usize]);
            }
            drop(data);
            buffer.unmap();
            if matches!(
                format,
                wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
            ) {
                for pixel in rgba8.as_chunks_mut::<4>().0 {
                    pixel.swap(0, 2);
                }
            }
            Ok(PrivateVectorReadback {
                extent: scene.extent,
                rgba8,
                texture_format: format,
                samples,
            })
        })();
        self.state.device.poll(wgpu::Maintain::Wait);
        let error = pollster::block_on(self.state.device.pop_error_scope());
        if let Some(error) = error {
            return Err(bad(format!("Private vector GPU validation: {error}")));
        }
        result
    }
}

fn same_coverage_owner(
    a: Option<&Arc<ferrite_render::PreparedCoverage>>,
    b: Option<&Arc<ferrite_render::PreparedCoverage>>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

#[cfg(test)]
mod private_draw_tests {
    use super::*;
    #[test]
    fn offscreen_checked_receiver_bounds_decline_before_gpu_allocations() {
        let l =
            readback_layout([17, 3], crate::state::MSAA_SAMPLE_COUNT, 4096, 1024 * 1024).unwrap();
        assert_eq!(
            (l.row, l.padded_row, l.buffer_bytes, l.rgba_bytes),
            (68, 256, 768, 204)
        );
        assert!(readback_layout([0, 3], crate::state::MSAA_SAMPLE_COUNT, 4096, u64::MAX).is_err());
        assert!(readback_layout([17, 3], crate::state::MSAA_SAMPLE_COUNT, 4096, 767).is_err());
        assert!(readback_layout(
            [u32::MAX, 3],
            crate::state::MSAA_SAMPLE_COUNT,
            u32::MAX,
            u64::MAX
        )
        .is_err());
        assert!(readback_layout(
            [4096, 4096],
            crate::state::MSAA_SAMPLE_COUNT,
            4096,
            u64::MAX
        )
        .is_err());
    }
    #[test]
    fn captured_reference_missing_glyph_rejects_before_gpu_owner_allocation() {
        // Uses the SAME captured-bound-font validator invoked by ReferencedChartOwner::prepare.
        let font = include_bytes!("../../../Catalogues/PC/S-421/Fonts/OpenSans-Regular.ttf");
        struct OwnedFixture(std::path::PathBuf);
        impl Drop for OwnedFixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "ferrite-private-draw-font-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let owned = OwnedFixture(root);
        std::fs::create_dir(owned.0.join("Fonts")).unwrap();
        std::fs::write(owned.0.join("portrayal_catalogue.xml"),"<portrayalCatalog><fonts><font id='same'><fileName>font.ttf</fileName><fileType>Font</fileType><fileFormat>TTF</fileFormat></font></fonts></portrayalCatalog>").unwrap();
        std::fs::write(owned.0.join("Fonts/font.ttf"), font).unwrap();
        let sources = ferrite_portrayal_catalog::CatalogueSources::capture(&owned.0).unwrap();
        let bound = ferrite_portrayal_catalog::BoundFontDeclarations::from_sources(sources)
            .unwrap()
            .resolve("same")
            .unwrap();
        let parsed =
            crate::referenced_chart_font::PreparedReferencedChartFont::prepare(&bound).unwrap();
        assert!(parsed.validate_text("Map 12.3 AB").is_ok());
        assert!(parsed.validate_text("\u{10FFFF}").is_err());
        // Same-PC capture exactness: no global UI fallback may turn the unknown glyph into success.
        assert!(parsed.validate_text("Map \u{10FFFF}").is_err());
    }
}

/// Explicit displayed vector mutation domain; does not replace App/raster/model transaction epochs.
/// Advancing is allocation-free except u64 rollover, where a new owner prevents ABA.
#[derive(Clone)]
pub(super) struct VectorSceneEpoch {
    owner: Arc<()>,
    generation: u64,
}
impl Default for VectorSceneEpoch {
    fn default() -> Self {
        Self {
            owner: Arc::new(()),
            generation: 0,
        }
    }
}
impl VectorSceneEpoch {
    pub(super) fn advance(&mut self) {
        if let Some(next) = self.generation.checked_add(1) {
            self.generation = next;
        } else {
            *self = Self::default();
        }
    }
    pub(super) fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner) && self.generation == other.generation
    }
}
#[cfg(test)]
mod epoch_tests {
    use super::*;
    #[test]
    fn display_mutation_and_rollover_reject_even_when_cpu_owner_is_unchanged() {
        let cpu = Arc::new(());
        let retained_cpu = cpu.clone();
        let mut epoch = VectorSceneEpoch::default();
        let old = epoch.clone();
        assert!(old.matches(&epoch));
        epoch.advance();
        assert!(Arc::ptr_eq(&cpu, &retained_cpu));
        assert!(!old.matches(&epoch));
        epoch.generation = u64::MAX;
        let before_rollover = epoch.clone();
        epoch.advance();
        assert!(!before_rollover.matches(&epoch));
        assert_eq!(epoch.generation, 0);
        assert!(!old.matches(&epoch));
    }
}

/// App must adopt both returned source-index/PC owners before exposing picks.
/// This is not an App/raster/journal commit token or source-authentication grant.
pub struct ActivatedVectorBindings {
    context: RenderContext,
    resources: crate::CellPortrayalResources,
}
impl ActivatedVectorBindings {
    pub fn into_parts(self) -> (RenderContext, crate::CellPortrayalResources) {
        (self.context, self.resources)
    }
}
pub struct ActivatedSinglePcVectorBindings {
    context: RenderContext,
    resources: SinglePcVectorResources,
}
impl ActivatedSinglePcVectorBindings {
    pub fn into_parts(self) -> (RenderContext, SinglePcVectorResources) {
        (self.context, self.resources)
    }
}
fn require_current_vector_palettes<'a>(
    current: &str,
    palettes: impl Iterator<Item = &'a str>,
) -> Result<()> {
    if palettes.into_iter().any(|palette| palette != current) {
        return Err(bad(
            "Vector target palette differs from current display policy",
        ));
    }
    Ok(())
}
fn require_joint_camera_targets(
    target: Option<[u64; 16]>,
    cameras: impl Iterator<Item = Option<[u64; 16]>>,
) -> Result<()> {
    let target = target.ok_or_else(|| bad("Joint raster target requires an actual flat camera"))?;
    if cameras.into_iter().any(|camera| camera != Some(target)) {
        return Err(bad(
            "Raster geometry was not prepared for the vector target camera",
        ));
    }
    Ok(())
}
struct VectorInstallation {
    candidate: ReadyVectorGpuFrame,
    next_epoch: VectorSceneEpoch,
    line_owner: Option<Arc<()>>,
}
/// Complete Renderer staging only; App/HDF/pick/journal publication is external.
/// No public constructor or unchecked installation method.
pub struct PreparedJointVectorRaster {
    installation: VectorInstallation,
    raster: PreparedRasterScenePublication,
    next_raster_epoch: Arc<()>,
    continuous_count: usize,
    #[cfg(feature = "s102-portrayal")]
    s102_target: Option<BoundS102Target>,
}
#[cfg(feature = "s102-portrayal")]
struct BoundS102Target {
    pc: Arc<ferrite_portrayal_catalog::BoundPortrayalCatalogue>,
    profile: String,
    settings: ferrite_s102::DepthSettings,
}
struct ActivatedResourceModeBindings {
    context: RenderContext,
    resources: PrivateVectorResources,
}
fn require_vector_activation_scope(
    has_raster: bool,
    has_native: bool,
    has_selection: bool,
    measuring: bool,
) -> Result<()> {
    if has_raster || has_native || has_selection || measuring {
        return Err(bad(
            "Vector activation requires raster/native/selection/measurement-free scope",
        ));
    }
    Ok(())
}
impl WgpuRenderer {
    /// Renderer-only, current-camera/current-policy activation boundary.
    /// All fallible preparation and expected-old checks precede any displayed owner move.
    /// No public unchecked commit, UI font setter, GPU upload, scene repair or raster activation.
    /// Caller must perform its independent App source/registry transaction before using this API.
    pub fn activate_ready_vector_frame(
        &mut self,
        candidate: ReadyVectorGpuFrame,
    ) -> Result<ActivatedVectorBindings> {
        if !candidate.prepared.resources.is_owned_cells() {
            return Err(bad("OwnedCells activation requires OwnedCells preparation"));
        }
        let installed = self.activate_ready_resource_mode(candidate)?;
        let PrivateVectorResources::OwnedCells(resources) = installed.resources else {
            unreachable!("Sealed resource mode checked before activation")
        };
        Ok(ActivatedVectorBindings {
            context: installed.context,
            resources,
        })
    }
    /// Same existing restricted activation scope; not a joint App/raster commit.
    pub fn activate_ready_single_pc_vector_frame(
        &mut self,
        candidate: ReadySinglePcVectorGpuFrame,
    ) -> Result<ActivatedSinglePcVectorBindings> {
        if candidate.0.prepared.resources.is_owned_cells() {
            return Err(bad("SinglePc activation requires SinglePc preparation"));
        }
        let installed = self.activate_ready_resource_mode(candidate.0)?;
        let PrivateVectorResources::SinglePc(resources) = installed.resources else {
            unreachable!("Sealed resource mode checked before activation")
        };
        Ok(ActivatedSinglePcVectorBindings {
            context: installed.context,
            resources: *resources,
        })
    }
    fn activate_ready_resource_mode(
        &mut self,
        candidate: ReadyVectorGpuFrame,
    ) -> Result<ActivatedResourceModeBindings> {
        let installation = self.prepare_vector_installation(candidate);
        self.validate_vector_installation_scope(&installation.candidate, false)?;
        self.validate_private_vector_gpu(&installation.candidate)?;
        Ok(self.install_validated_vector(installation))
    }
    fn validate_vector_installation_scope(
        &self,
        candidate: &ReadyVectorGpuFrame,
        allow_raster: bool,
    ) -> Result<()> {
        require_vector_activation_scope(
            !allow_raster
                && (!self.raster_layers.is_empty()
                    || self.continuous_layer_count != 0
                    || self.continuous_frame.is_some()),
            self.native_route_gpu.is_some(),
            self.ui_state.selected_feature.is_some()
                || !self.ui_state.selection_candidates.is_empty()
                || self.vector_emission.vector_frame.selection_anchor.is_some()
                || !self
                    .vector_emission
                    .vector_frame
                    .selection_world_geometry
                    .is_empty()
                || !self
                    .vector_emission
                    .vector_frame
                    .selection_screen_geometry
                    .is_empty(),
            self.vector_emission.flat_diagnostic.is_some() || self.gpu_timestamp_batch.is_some(),
        )?;
        match &candidate.prepared.resources {
            PrivateVectorResources::OwnedCells(resources) => require_current_vector_palettes(
                &self.ui_state.color_profile,
                resources
                    .cell_profiles()
                    .map(|(_, profile)| profile.id.as_str()),
            )?,
            PrivateVectorResources::SinglePc(resources) => require_current_vector_palettes(
                &self.ui_state.color_profile,
                std::iter::once(resources.profile().id.as_str()),
            )?,
        }
        // First boundary intentionally does not install a new App camera/palette/settings.
        // Source changes at the exact current camera are supported. Distinct target-camera
        // publication needs an explicit joint App camera/UI/picking capsule, not post-swap setters.
        if policy_bits(candidate.prepared.settings) != policy_bits(self.vector_emission_settings())
            || candidate.prepared.emission.vector_frame.geometry_transform
                != self.vector_emission.vector_frame.geometry_transform
            || candidate
                .prepared
                .emission
                .vector_frame
                .chart_geometry_viewport
                .map(|v| {
                    [
                        v.x.to_bits(),
                        v.y.to_bits(),
                        v.width.to_bits(),
                        v.height.to_bits(),
                    ]
                })
                != self
                    .vector_emission
                    .vector_frame
                    .chart_geometry_viewport
                    .map(|v| {
                        [
                            v.x.to_bits(),
                            v.y.to_bits(),
                            v.width.to_bits(),
                            v.height.to_bits(),
                        ]
                    })
            || candidate.target_continuous_transform_key() != self.continuous_transform_key()
        {
            return Err(bad(
                "Vector activation target differs from current camera/policy",
            ));
        }
        if !candidate
            .prepared
            .emission
            .vector_frame
            .selection_world_geometry
            .is_empty()
            || !candidate
                .prepared
                .emission
                .vector_frame
                .selection_screen_geometry
                .is_empty()
            || candidate
                .prepared
                .emission
                .vector_frame
                .selection_anchor
                .is_some()
        {
            return Err(bad(
                "Private candidate contains unsupported selection overlay",
            ));
        }
        Ok(())
    }
    fn prepare_vector_installation(&self, candidate: ReadyVectorGpuFrame) -> VectorInstallation {
        let mut next_epoch = self.vector_scene_epoch.clone();
        next_epoch.advance();
        let line_owner = candidate
            .gpu
            .line_compact
            .then(|| Arc::clone(&candidate.prepared.emission.vector_frame.frame_cpu.owner));
        VectorInstallation {
            candidate,
            next_epoch,
            line_owner,
        }
    }
    // Module-private, called only immediately after the complete publication guard.
    fn install_validated_vector(
        &mut self,
        installation: VectorInstallation,
    ) -> ActivatedResourceModeBindings {
        let VectorInstallation {
            candidate,
            next_epoch,
            line_owner,
        } = installation;
        let ReadyVectorGpuFrame { prepared, gpu } = candidate;
        let PreparedVectorEmission {
            mut emission,
            context,
            resources,
            environment: _,
            old_scene_epoch: _,
            expected_old: _,
            settings: _,
        } = prepared;
        let PrivateVectorGpu {
            area,
            line,
            pattern,
            world_lines,
            world_masks,
            line_compact,
            compact,
            symbols,
            quad_indices,
            text,
            glyphs,
            view,
            buffer_payload: _,
        } = gpu;
        let PrivateVectorView {
            buffers: [center_buffer, left_buffer, right_buffer],
            groups: [center_group, left_group, right_group],
            uniforms,
        } = view;
        emission.cached_symbol_buffers = symbols;
        emission.gpu_buffers_dirty = false;
        // Whole owned CPU outputs, materials, coverage/masks, fonts, dependency/pick state and caches.
        self.vector_emission = emission;
        self.cached_area_vb = area.vertices;
        // Private Ready owns a different GPU tuple: discard normal upload witness.
        self.area_index_upload_reuse.invalidate();
        self.cached_area_ib = area.indices;
        self.cached_area_index_count = area.index_count;
        self.cached_line_vb = line.vertices;
        self.cached_line_ib = line.indices;
        self.cached_line_index_count = line.index_count; // Original logical 4/6 counts; packed IBO is six indices.
        self.cached_line_compact = line_compact;
        self.exact_line_quad_pipelines = compact;
        self.exact_line_cpu_owner = line_owner;
        self.cached_pattern_vb = pattern.vertices;
        self.cached_pattern_ib = pattern.indices;
        self.cached_pattern_index_count = pattern.index_count;
        self.cached_wm_line_vb = world_lines.vertices;
        self.cached_wm_line_ib = world_lines.indices;
        self.cached_wm_mask_vb = world_masks.vertices;
        self.cached_wm_mask_ib = world_masks.indices;
        self.shared_symbol_quad_index_buffer = quad_indices;
        // Whole Ready activation replaces producer/font/pool ownership, even same bytes.
        self.immutable_text_preparation.clear();
        self.chart_text_meshes = text;
        self.chart_text_buffers = glyphs;
        self.chart_text_shapes = Vec::new();
        self.view_buffer = center_buffer;
        self.view_buffer_left = left_buffer;
        self.view_buffer_right = right_buffer;
        self.view_bind_group = center_group;
        self.view_bind_group_left = left_group;
        self.view_bind_group_right = right_group;
        self.continuous_uniforms = uniforms;
        // Stale packed upload scratch is not authoritative; no old ranges may leak into a future upload.
        self.packed_symbol_vertices = Vec::new();
        self.packed_symbol_indices = Vec::new();
        self.packed_symbol_ranges = Vec::new();
        self.vector_scene_epoch = next_epoch;
        ActivatedResourceModeBindings { context, resources }
    }

    /// First joint Renderer boundary: exact CURRENT camera/policy only.
    /// GPU uploads must already be complete; no App/journal authorization is granted.
    pub fn prepare_ready_vector_raster_publication(
        &self,
        vector: ReadyVectorGpuFrame,
        raster: PreparedRasterScenePublication,
    ) -> Result<PreparedJointVectorRaster> {
        if !vector.prepared.resources.is_owned_cells() {
            return Err(bad(
                "Joint preparation requires sealed OwnedCells resources",
            ));
        }
        let installation = self.prepare_vector_installation(vector);
        let continuous_count = raster
            .raster
            .layers
            .iter()
            .filter(|l| l.continuous.is_some())
            .count();
        let candidate = PreparedJointVectorRaster {
            installation,
            raster,
            next_raster_epoch: Arc::new(()),
            continuous_count,
            #[cfg(feature = "s102-portrayal")]
            s102_target: None,
        };
        self.validate_joint_vector_raster(&candidate)?;
        Ok(candidate)
    }
    /// Strict regular-S102 PC check on top of the existing camera/GPU guards.
    /// expected_pc must come from the App's retained actual producer catalogue.
    /// This argument is an EXPECTATION; it never manufactures a material owner.
    #[cfg(feature = "s102-portrayal")]
    pub fn prepare_ready_vector_bound_s102_publication(
        &self,
        vector: ReadyVectorGpuFrame,
        raster: PreparedRasterScenePublication,
        expected_pc: Arc<ferrite_portrayal_catalog::BoundPortrayalCatalogue>,
        profile: String,
        settings: ferrite_s102::DepthSettings,
    ) -> Result<PreparedJointVectorRaster> {
        let mut candidate = self.prepare_ready_vector_raster_publication(vector, raster)?;
        candidate.s102_target = Some(BoundS102Target {
            pc: expected_pc,
            profile,
            settings,
        });
        self.validate_joint_vector_raster(&candidate)?;
        Ok(candidate)
    }
    fn validate_joint_vector_raster(&self, candidate: &PreparedJointVectorRaster) -> Result<()> {
        let vector = &candidate.installation.candidate;
        self.validate_vector_installation_scope(vector, true)?;
        self.validate_private_vector_gpu(vector)?;
        self.validate_raster_scene_publication(&candidate.raster)?;
        let raster = &candidate.raster.raster;
        #[cfg(feature = "s102-portrayal")]
        if let Some(target) = &candidate.s102_target {
            // The current boundary covers regular S102 only. Never bypass a
            // missing owner, legacy layer or continuous source with a digest.
            let key = |s: ferrite_s102::DepthSettings| {
                (
                    s.safety_contour.to_bits(),
                    s.shallow_contour.to_bits(),
                    s.deep_contour.to_bits(),
                    s.four_shades,
                )
            };
            let current = self.settings();
            let current_depth = ferrite_s102::DepthSettings {
                safety_contour: current.safety_contour,
                shallow_contour: current.shallow_contour,
                deep_contour: current.deep_contour,
                four_shades: !current.two_shades,
            };
            if target.profile != self.ui_state.color_profile
                || key(target.settings) != key(current_depth)
            {
                return Err(bad(
                    "Bound S102 target differs from current renderer policy",
                ));
            }
            if raster.layers.is_empty() {
                return Err(bad("Bound S102 target has no material"));
            }
            for layer in &raster.layers {
                let owner = layer
                    .regular_portrayal_owner
                    .as_ref()
                    .ok_or_else(|| bad("S102 raster has no actual evaluation owner"))?;
                if layer.continuous.is_some()
                    || !Arc::ptr_eq(owner.catalogue(), &target.pc)
                    || owner.profile_id() != target.profile
                    || key(owner.settings()) != key(target.settings)
                {
                    return Err(bad("S102 raster producer differs from bound target"));
                }
            }
        }
        require_joint_camera_targets(
            vector.prepared.context.scaler.flat_encoded_identity(),
            raster.layers.iter().map(|l| l.prepared_camera),
        )?;
        let target_key = vector.target_continuous_transform_key();
        if raster.transform != target_key {
            return Err(bad(
                "Joint old/target affine view differs; new-view support is not implemented",
            ));
        }
        let target = &vector.prepared.emission.vector_frame;
        if target.screen_pan_offset != (0., 0.)
            || target.screen_zoom_scale != 1.
            || target.screen_zoom_scale_y != 1.
            || target.screen_zoom_pivot != (0., 0.)
        {
            return Err(bad(
                "Joint vector preparation must already own reset affine uniforms",
            ));
        }
        if let Some(proof) = &raster.proof {
            if proof.transform_key != target_key
                || !proof.binding.matches_current(
                    &self.continuous_owner,
                    vector
                        .gpu
                        .view
                        .uniforms
                        .iter()
                        .flatten()
                        .map(bytemuck::bytes_of),
                    [self.state.size.width, self.state.size.height],
                    crate::state::MSAA_SAMPLE_COUNT,
                    self.state.config.format,
                    raster
                        .layers
                        .iter()
                        .filter_map(|l| l.continuous_identity.as_ref()),
                )
            {
                return Err(bad(
                    "Continuous proof does not bind the actual NEW private view/layers",
                ));
            }
        } else if candidate.continuous_count != 0 {
            return Err(bad("Joint continuous proof absent"));
        }
        Ok(())
    }
    /// One complete fresh guard, then private owner moves only. No two commit APIs.
    pub fn activate_ready_vector_raster_publication(
        &mut self,
        candidate: PreparedJointVectorRaster,
    ) -> Result<ActivatedVectorBindings> {
        self.validate_joint_vector_raster(&candidate)?;
        let PreparedJointVectorRaster {
            installation,
            raster,
            next_raster_epoch,
            continuous_count,
            #[cfg(feature = "s102-portrayal")]
                s102_target: _,
        } = candidate;
        let installed = self.install_validated_vector(installation);
        let PreparedRasterScenePublication {
            raster,
            enabled_groups,
        } = raster;
        self.raster_layers = raster.layers;
        self.continuous_frame = raster.proof;
        self.continuous_layer_count = continuous_count;
        self.raster_enabled_groups = enabled_groups;
        self.raster_epoch = next_raster_epoch;
        let PrivateVectorResources::OwnedCells(resources) = installed.resources else {
            unreachable!("OwnedCells mode checked before joint publication")
        };
        Ok(ActivatedVectorBindings {
            context: installed.context,
            resources,
        })
    }
}
#[cfg(test)]
mod activation_scope_tests {
    use super::*;
    #[test]
    fn each_unsupported_live_owner_declines_without_a_clear_or_success_token() {
        assert!(require_vector_activation_scope(false, false, false, false).is_ok());
        for index in 0..4 {
            let mut flags = [false; 4];
            flags[index] = true;
            assert!(
                require_vector_activation_scope(flags[0], flags[1], flags[2], flags[3]).is_err()
            );
        }
    }
    #[test]
    fn combined_unsupported_scope_never_admits() {
        for bits in 1u8..16 {
            assert!(require_vector_activation_scope(
                bits & 1 != 0,
                bits & 2 != 0,
                bits & 4 != 0,
                bits & 8 != 0
            )
            .is_err());
        }
    }
}

#[cfg(test)]
mod joint_camera_tests {
    use super::*;
    #[test]
    fn current_vector_palette_rejects_foreign_and_mixed_target_owners() {
        assert!(require_current_vector_palettes("Day", ["Day", "Day"].into_iter()).is_ok());
        assert!(require_current_vector_palettes("Day", ["Dusk"].into_iter()).is_err());
        assert!(require_current_vector_palettes("Day", ["Day", "Night"].into_iter()).is_err());
        assert!(require_current_vector_palettes("Day", ["day"].into_iter()).is_err());
    }
    fn scaler() -> ferrite_render::Scaler {
        ferrite_render::Scaler::new(
            ferrite_render::GeoBounds::new(-2., 48., 2., 52.),
            ferrite_render::Viewport::new(800., 600.),
        )
    }
    #[test]
    fn joint_camera_exact_empty_and_duplicate_inventory() {
        let identity = scaler().flat_encoded_identity();
        assert!(identity.is_some());
        assert!(require_joint_camera_targets(identity, [].into_iter()).is_ok());
        assert!(require_joint_camera_targets(identity, [identity, identity].into_iter()).is_ok());
        assert!(require_joint_camera_targets(None, [].into_iter()).is_err());
        assert!(require_joint_camera_targets(identity, [None].into_iter()).is_err());
    }
    #[test]
    fn joint_camera_rejects_real_bounds_and_density_changes() {
        let original = scaler();
        let identity = original.flat_encoded_identity();
        let mut different = original.clone();
        different.set_bounds(ferrite_render::GeoBounds::new(-1., 48., 3., 52.));
        assert!(require_joint_camera_targets(
            identity,
            [different.flat_encoded_identity()].into_iter()
        )
        .is_err());
        let mut different = original.clone();
        different.set_pixel_ratio(2.);
        assert!(require_joint_camera_targets(
            identity,
            [different.flat_encoded_identity()].into_iter()
        )
        .is_err());
    }
}

/// Target-camera/policy staging only. App activation is a separate unfinished boundary.
// UI policy is target intent; actual catalogue/material owners remain independent.
struct TargetRendererIntent {
    settings: SettingsState,
    profile: String,
    emission: VectorEmissionSettings,
}
pub struct PreparedTargetVectorRaster {
    installation: VectorInstallation,
    raster: PreparedRasterScenePublication,
    continuous_count: usize,
    target: TargetRendererIntent,
    #[cfg(feature = "s102-portrayal")]
    s102_target: Option<BoundS102Target>,
}
pub struct PreparedTargetSinglePcVectorRaster(PreparedTargetVectorRaster);
impl WgpuRenderer {
    pub fn prepare_target_vector_raster_publication(
        &self,
        vector: ReadyVectorGpuFrame,
        raster: PreparedRasterScenePublication,
        settings: SettingsState,
        profile: String,
        emission: VectorEmissionSettings,
    ) -> Result<PreparedTargetVectorRaster> {
        if !vector.prepared.resources.is_owned_cells() {
            return Err(bad(
                "TARGET OwnedCells preparation requires OwnedCells resources",
            ));
        }
        self.prepare_target_resource_mode_publication(vector, raster, settings, profile, emission)
    }
    pub fn prepare_target_single_pc_vector_raster_publication(
        &self,
        vector: ReadySinglePcVectorGpuFrame,
        raster: PreparedRasterScenePublication,
        settings: SettingsState,
        profile: String,
        emission: VectorEmissionSettings,
    ) -> Result<PreparedTargetSinglePcVectorRaster> {
        if vector.0.prepared.resources.is_owned_cells() {
            return Err(bad(
                "TARGET SinglePc preparation requires SinglePc resources",
            ));
        }
        self.prepare_target_resource_mode_publication(vector.0, raster, settings, profile, emission)
            .map(PreparedTargetSinglePcVectorRaster)
    }
    fn prepare_target_resource_mode_publication(
        &self,
        vector: ReadyVectorGpuFrame,
        raster: PreparedRasterScenePublication,
        settings: SettingsState,
        profile: String,
        emission: VectorEmissionSettings,
    ) -> Result<PreparedTargetVectorRaster> {
        let continuous_count = raster
            .raster
            .layers
            .iter()
            .filter(|l| l.continuous.is_some())
            .count();
        let candidate = PreparedTargetVectorRaster {
            installation: self.prepare_vector_installation(vector),
            raster,
            continuous_count,
            target: TargetRendererIntent {
                settings,
                profile,
                emission,
            },
            #[cfg(feature = "s102-portrayal")]
            s102_target: None,
        };
        self.validate_target_vector_raster_publication(&candidate)?;
        Ok(candidate)
    }
    // Fresh at construction and again immediately before a future private install.
    // Do NOT mutate live UI policy to pass any of these checks.
    pub fn validate_target_vector_raster_publication(
        &self,
        candidate: &PreparedTargetVectorRaster,
    ) -> Result<()> {
        let vector = &candidate.installation.candidate;
        // Actual OLD expected vector owner/environment and complete GPU/coverage checks.
        self.validate_private_vector_gpu(vector)?;
        // Actual OLD raster epoch/source/frame/affine checks stay intact.
        self.validate_raster_scene_publication(&candidate.raster)?;
        let target = &candidate.target;
        if ![
            target.settings.safety_depth,
            target.settings.safety_contour,
            target.settings.shallow_contour,
            target.settings.deep_contour,
        ]
        .iter()
        .all(|v| v.is_finite())
            || target.profile.is_empty()
            || policy_bits(vector.prepared.settings) != policy_bits(target.emission)
            || target.settings.show_shallow_pattern != target.emission.show_shallow_pattern
        {
            return Err(bad(
                "TARGET policy differs from actual private vector preparation",
            ));
        }
        match &vector.prepared.resources {
            PrivateVectorResources::OwnedCells(resources) => require_current_vector_palettes(
                &target.profile,
                resources.cell_profiles().map(|(_, p)| p.id.as_str()),
            )?,
            PrivateVectorResources::SinglePc(resources) => require_current_vector_palettes(
                &target.profile,
                std::iter::once(resources.profile().id.as_str()),
            )?,
        }
        let frame = &vector.prepared.emission.vector_frame;
        let viewport = vector.prepared.context.scaler.viewport;
        if frame.chart_geometry_viewport.map(|v| {
            [
                v.x.to_bits(),
                v.y.to_bits(),
                v.width.to_bits(),
                v.height.to_bits(),
            ]
        }) != Some([
            viewport.x.to_bits(),
            viewport.y.to_bits(),
            viewport.width.to_bits(),
            viewport.height.to_bits(),
        ]) || frame.geometry_transform != Some(vector.prepared.context.scaler.flat_transform())
            || frame.screen_pan_offset != (0., 0.)
            || frame.screen_zoom_scale != 1.
            || frame.screen_zoom_scale_y != 1.
            || frame.screen_zoom_pivot != (0., 0.)
        {
            return Err(bad(
                "TARGET vector camera is not the actual reset private context view",
            ));
        }
        let raster = &candidate.raster.raster;
        #[cfg(feature = "s102-portrayal")]
        if let Some(bound) = &candidate.s102_target {
            let key = |s: ferrite_s102::DepthSettings| {
                (
                    s.safety_contour.to_bits(),
                    s.shallow_contour.to_bits(),
                    s.deep_contour.to_bits(),
                    s.four_shades,
                )
            };
            let requested = ferrite_s102::DepthSettings {
                safety_contour: target.settings.safety_contour,
                shallow_contour: target.settings.shallow_contour,
                deep_contour: target.settings.deep_contour,
                four_shades: !target.settings.two_shades,
            };
            if bound.profile != target.profile || key(bound.settings) != key(requested) {
                return Err(bad("Bound S102 target differs from TARGET renderer policy"));
            }
            if raster.layers.is_empty() {
                return Err(bad("Bound S102 target has no material"));
            }
            for layer in &raster.layers {
                let owner = layer
                    .regular_portrayal_owner
                    .as_ref()
                    .ok_or_else(|| bad("S102 raster has no actual evaluation owner"))?;
                if layer.continuous.is_some()
                    || !Arc::ptr_eq(owner.catalogue(), &bound.pc)
                    || owner.profile_id() != bound.profile
                    || key(owner.settings()) != key(bound.settings)
                {
                    return Err(bad("S102 raster producer differs from bound TARGET"));
                }
            }
        }
        require_joint_camera_targets(
            vector.prepared.context.scaler.flat_encoded_identity(),
            raster.layers.iter().map(|l| l.prepared_camera),
        )?;
        // raster.transform is the OLD guard, never compare it to target_key here.
        let target_key = vector.target_continuous_transform_key();
        if let Some(proof) = &raster.proof {
            if proof.transform_key != target_key
                || !proof.binding.matches_current(
                    &self.continuous_owner,
                    vector
                        .gpu
                        .view
                        .uniforms
                        .iter()
                        .flatten()
                        .map(bytemuck::bytes_of),
                    [self.state.size.width, self.state.size.height],
                    crate::state::MSAA_SAMPLE_COUNT,
                    self.state.config.format,
                    raster
                        .layers
                        .iter()
                        .filter_map(|l| l.continuous_identity.as_ref()),
                )
            {
                return Err(bad(
                    "TARGET continuous proof does not bind private view and layers",
                ));
            }
        } else if candidate.continuous_count != 0 {
            return Err(bad("TARGET continuous proof absent"));
        }
        Ok(())
    }
    pub fn validate_target_single_pc_vector_raster_publication(
        &self,
        candidate: &PreparedTargetSinglePcVectorRaster,
    ) -> Result<()> {
        self.validate_target_vector_raster_publication(&candidate.0)
    }
}

impl PreparedTargetVectorRaster {
    pub fn context(&self) -> &RenderContext {
        self.installation.candidate.context()
    }
    pub fn target_profile(&self) -> &str {
        &self.target.profile
    }
    pub fn target_settings(&self) -> &SettingsState {
        &self.target.settings
    }
    pub fn target_emission_settings(&self) -> VectorEmissionSettings {
        self.target.emission
    }
}
impl PreparedTargetSinglePcVectorRaster {
    pub fn context(&self) -> &RenderContext {
        self.0.context()
    }
    pub fn target_profile(&self) -> &str {
        self.0.target_profile()
    }
    pub fn target_settings(&self) -> &SettingsState {
        self.0.target_settings()
    }
    pub fn target_emission_settings(&self) -> VectorEmissionSettings {
        self.0.target_emission_settings()
    }
}
impl WgpuRenderer {
    #[cfg(feature = "s102-portrayal")]
    #[expect(
        clippy::too_many_arguments,
        reason = "Independent target intent and actual retained producer expectation"
    )]
    pub fn prepare_target_vector_bound_s102_publication(
        &self,
        vector: ReadyVectorGpuFrame,
        raster: PreparedRasterScenePublication,
        settings: SettingsState,
        profile: String,
        emission: VectorEmissionSettings,
        expected_pc: Arc<ferrite_portrayal_catalog::BoundPortrayalCatalogue>,
        depth: ferrite_s102::DepthSettings,
    ) -> Result<PreparedTargetVectorRaster> {
        let mut candidate = self.prepare_target_vector_raster_publication(
            vector, raster, settings, profile, emission,
        )?;
        candidate.s102_target = Some(BoundS102Target {
            pc: expected_pc,
            profile: candidate.target.profile.clone(),
            settings: depth,
        });
        self.validate_target_vector_raster_publication(&candidate)?;
        Ok(candidate)
    }
    #[cfg(feature = "s102-portrayal")]
    #[expect(
        clippy::too_many_arguments,
        reason = "SinglePc retains its actual resource mode and producer expectation"
    )]
    pub fn prepare_target_single_pc_vector_bound_s102_publication(
        &self,
        vector: ReadySinglePcVectorGpuFrame,
        raster: PreparedRasterScenePublication,
        settings: SettingsState,
        profile: String,
        emission: VectorEmissionSettings,
        expected_pc: Arc<ferrite_portrayal_catalog::BoundPortrayalCatalogue>,
        depth: ferrite_s102::DepthSettings,
    ) -> Result<PreparedTargetSinglePcVectorRaster> {
        let mut candidate = self.prepare_target_single_pc_vector_raster_publication(
            vector, raster, settings, profile, emission,
        )?;
        candidate.0.s102_target = Some(BoundS102Target {
            pc: expected_pc,
            profile: candidate.0.target.profile.clone(),
            settings: depth,
        });
        self.validate_target_single_pc_vector_raster_publication(&candidate)?;
        Ok(candidate)
    }
    // Private-vector readback only: explicitly excludes raster composition/egui/native overlays.
    pub fn audit_target_vector_frame(
        &self,
        candidate: &PreparedTargetVectorRaster,
    ) -> Result<PrivateVectorReadback> {
        self.validate_target_vector_raster_publication(candidate)?;
        self.audit_private_vector_frame(&candidate.installation.candidate)
    }
    pub fn audit_target_single_pc_vector_frame(
        &self,
        candidate: &PreparedTargetSinglePcVectorRaster,
    ) -> Result<PrivateVectorReadback> {
        self.audit_target_vector_frame(&candidate.0)
    }
    pub fn reproject_target_raster_scene(
        &self,
        scene: &mut PreparedRasterScenePublication,
        scaler: &ferrite_render::Scaler,
    ) -> Result<()> {
        // OLD freshness retained; underlying routine rejects continuous without new qualification.
        self.reproject_raster_publication(&mut scene.raster, scaler)
    }
}

// Renderer TARGET installation boundary. App must have staged
// its source/registry/model/picks adoption before calling; no App token is minted.
fn validate_target_zoom(zoom: f64) -> Result<()> {
    if !zoom.is_finite() || zoom <= 0. || zoom > 500. {
        return Err(bad("Invalid TARGET navigation labels"));
    }
    Ok(())
}
impl WgpuRenderer {
    fn validate_target_activation_scope(
        &self,
        candidate: &PreparedTargetVectorRaster,
    ) -> Result<()> {
        // Same supported-overlay scope as current-only activation. Do not clear
        // live selection/routes/measurements to turn a rejection into success.
        require_vector_activation_scope(
            false,
            self.native_route_gpu.is_some(),
            self.ui_state.selected_feature.is_some()
                || !self.ui_state.selection_candidates.is_empty()
                || self.vector_emission.vector_frame.selection_anchor.is_some()
                || !self
                    .vector_emission
                    .vector_frame
                    .selection_world_geometry
                    .is_empty()
                || !self
                    .vector_emission
                    .vector_frame
                    .selection_screen_geometry
                    .is_empty(),
            self.vector_emission.flat_diagnostic.is_some() || self.gpu_timestamp_batch.is_some(),
        )?;
        let frame = &candidate
            .installation
            .candidate
            .prepared
            .emission
            .vector_frame;
        if frame.selection_anchor.is_some()
            || !frame.selection_world_geometry.is_empty()
            || !frame.selection_screen_geometry.is_empty()
        {
            return Err(bad("TARGET contains unsupported selection overlay"));
        }
        let has_regular = candidate
            .raster
            .raster
            .layers
            .iter()
            .any(|l| l.continuous.is_none());
        // Generic raster staging is useful for auditing, but cannot authorize
        // publication of S102 colours. Require the genuine retained producer.
        #[cfg(feature = "s102-portrayal")]
        if has_regular && candidate.s102_target.is_none() {
            return Err(bad(
                "TARGET activation requires bound S102 material authority",
            ));
        }
        #[cfg(not(feature = "s102-portrayal"))]
        if has_regular {
            return Err(bad(
                "Regular TARGET raster activation requires S102 portrayal support",
            ));
        }
        Ok(())
    }
    /// Target policy/camera installation, with the early OLD origin checked fresh.
    /// This returns actual context/resource owners; it does not adopt App models,
    /// source HDF/IC inputs, registry, hit tests, journal or navigation state.
    pub fn activate_target_vector_raster_publication(
        &mut self,
        candidate: PreparedTargetVectorRaster,
        zoom: f64,
        compilation_scale: u32,
    ) -> Result<ActivatedVectorBindings> {
        if !candidate
            .installation
            .candidate
            .prepared
            .resources
            .is_owned_cells()
        {
            return Err(bad(
                "TARGET OwnedCells activation requires OwnedCells resources",
            ));
        }
        let installed =
            self.activate_target_resource_mode_publication(candidate, zoom, compilation_scale)?;
        let PrivateVectorResources::OwnedCells(resources) = installed.resources else {
            unreachable!("Sealed TARGET resource mode was checked before installation")
        };
        Ok(ActivatedVectorBindings {
            context: installed.context,
            resources,
        })
    }
    pub fn activate_target_single_pc_vector_raster_publication(
        &mut self,
        candidate: PreparedTargetSinglePcVectorRaster,
        zoom: f64,
        compilation_scale: u32,
    ) -> Result<ActivatedSinglePcVectorBindings> {
        if candidate
            .0
            .installation
            .candidate
            .prepared
            .resources
            .is_owned_cells()
        {
            return Err(bad(
                "TARGET SinglePc activation requires SinglePc resources",
            ));
        }
        let installed =
            self.activate_target_resource_mode_publication(candidate.0, zoom, compilation_scale)?;
        let PrivateVectorResources::SinglePc(resources) = installed.resources else {
            unreachable!("Sealed TARGET resource mode was checked before installation")
        };
        Ok(ActivatedSinglePcVectorBindings {
            context: installed.context,
            resources: *resources,
        })
    }
    fn activate_target_resource_mode_publication(
        &mut self,
        candidate: PreparedTargetVectorRaster,
        zoom: f64,
        compilation_scale: u32,
    ) -> Result<ActivatedResourceModeBindings> {
        validate_target_zoom(zoom)?;
        // Allocate/clone independent bookkeeping before the last guard. Do not
        // call setters after installation: they invalidate the newly installed epoch.
        let next_raster_epoch = Arc::new(());
        let ruler = self
            .ui_state
            .coordinate_ruler_scaler
            .as_ref()
            .map(|_| candidate.context().scaler.clone());
        let frame = &candidate
            .installation
            .candidate
            .prepared
            .emission
            .vector_frame;
        let size = self.state.window.inner_size();
        let physical_extent = [size.width, size.height];
        let indication = (!frame.coverage_failed)
            .then_some(frame.chart_geometry_viewport)
            .flatten()
            .and_then(|viewport| {
                frame
                    .prepared_coverage
                    .as_ref()
                    .and_then(|prepared| prepared.pass(0).ok())
                    .and_then(|pass| pass.frame().scale_annotations())
                    .filter(|a| a.physical_extent() == physical_extent)
                    .filter(|a| {
                        a.reference_point
                            == [
                                viewport.x as f64 + viewport.width as f64 * 0.5,
                                viewport.y as f64 + viewport.height as f64 * 0.5,
                            ]
                    })
                    .and_then(|a| {
                        a.reference().map(|reference| {
                            crate::egui_integration::CoverageScaleIndication {
                                viewing_denominator: a.viewing_denominator,
                                overscale_factor: reference.state.factor,
                                physical_viewport: viewport,
                                sclbr: frame
                                    .coverage_scale_colours
                                    .get(&reference.dataset_id)
                                    .copied()
                                    .or(frame.coverage_scale_colour),
                            }
                        })
                    })
            });
        self.validate_target_activation_scope(&candidate)?;
        // Complete existing OLD origin/environment/epochs/GPU/coverage/raster
        // validation plus TARGET palette/emission/camera/producer/proof validation.
        // No fallible work, upload, callback, repair or public setter after this.
        self.validate_target_vector_raster_publication(&candidate)?;
        let PreparedTargetVectorRaster {
            installation,
            raster,
            continuous_count,
            target,
            #[cfg(feature = "s102-portrayal")]
                s102_target: _,
        } = candidate;
        let installed = self.install_validated_vector(installation);
        let PreparedRasterScenePublication {
            raster,
            enabled_groups,
        } = raster;
        self.raster_layers = raster.layers;
        self.continuous_frame = raster.proof;
        self.continuous_layer_count = continuous_count;
        self.raster_enabled_groups = enabled_groups;
        self.raster_epoch = next_raster_epoch;
        self.symbol_scale = target.emission.symbol_scale;
        self.show_soundings = target.emission.show_soundings;
        self.animation_mode = target.emission.animation_mode;
        self.background_color = target.emission.background_color;
        self.ui_state.settings = target.settings;
        self.ui_state.color_profile = target.profile;
        self.zoom_level = zoom;
        // Zero retains the existing unknown-scale state for legacy producer metadata.
        self.compilation_scale = compilation_scale;
        self.ui_state.zoom_level = zoom;
        self.ui_state.coordinate_ruler_scaler = ruler;
        self.ui_state.coverage_scale_indication = indication;
        Ok(installed)
    }
}

#[cfg(test)]
mod target_activation_navigation_tests {
    use super::*;
    #[test]
    fn target_zoom_rejects_nonfinite_and_out_of_range_values() {
        for zoom in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.,
            -1.,
            500.000001,
        ] {
            assert!(validate_target_zoom(zoom).is_err());
        }
        assert!(validate_target_zoom(500.).is_ok());
        assert!(validate_target_zoom(0.01).is_ok());
    }
}
