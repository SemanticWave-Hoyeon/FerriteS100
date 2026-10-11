//! Shared original chart encoder; ordinary scene includes raster/native callbacks, private vector scene does not.
use super::*;
#[path = "scene_draw_plan.rs"]
pub(super) mod scene_draw_plan;
/// Largest screen-space extent not scaled by the view (line half-width plus
/// offsets, symbol corners around their anchor), with a wide safety factor.
const WRAP_CULL_MARGIN_PX: f32 = 1024.;
/// The view-uniform transform of `pipeline.rs`, applied to retained `bounds`
/// moved by a longitude copy's pan `shift`, widened by the unscaled margin.
/// False only when the copy provably misses the surface; NaN keeps the copy.
fn wrap_copy_may_reach_surface(
    bounds: [f32; 4],
    shift: f32,
    pan: (f32, f32),
    pivot: (f32, f32),
    zoom: [f32; 2],
    extent: [u32; 2],
) -> bool {
    let x = |v: f32| (v + pan.0 + shift - pivot.0) * zoom[0] + pivot.0;
    let y = |v: f32| (v + pan.1 - pivot.1) * zoom[1] + pivot.1;
    let misses = x(bounds[2]) + WRAP_CULL_MARGIN_PX < 0.
        || y(bounds[3]) + WRAP_CULL_MARGIN_PX < 0.
        || x(bounds[0]) - WRAP_CULL_MARGIN_PX > extent[0] as f32
        || y(bounds[1]) - WRAP_CULL_MARGIN_PX > extent[1] as f32;
    !misses
}
pub(super) struct PairRef<'a> {
    pub vertices: &'a Option<wgpu::Buffer>,
    pub indices: &'a Option<wgpu::Buffer>,
}
pub(super) struct VectorDrawScene<'a> {
    pub emission: &'a VectorEmissionOwned,
    pub pipelines: &'a RenderPipelines,
    pub area: PairRef<'a>,
    pub line: PairRef<'a>,
    pub pattern: PairRef<'a>,
    pub world_lines: PairRef<'a>,
    pub world_masks: PairRef<'a>,
    pub views: [&'a wgpu::BindGroup; 3],
    pub line_compact: bool,
    pub compact: Option<&'a crate::exact_line_quad::Pipelines>,
    pub symbols: &'a EmittedSymbolBuffers,
    pub text: &'a [GpuChartText],
    pub extent: [u32; 2],
    pub background: Color,
    pub draw_range_index_enabled: bool,
    pub raster_renderer: Option<&'a WgpuRenderer>,
}
impl VectorDrawScene<'_> {
    fn raster_layers(&self) -> &[GpuRasterLayer] {
        self.raster_renderer
            .map_or(&[], |r| r.raster_layers.as_slice())
    }
    fn raster_groups(&self) -> Option<&std::collections::HashSet<u32>> {
        self.raster_renderer
            .and_then(|r| r.raster_enabled_groups.as_ref())
    }
    /// Which longitude copies can reach the surface. A ±360° copy whose
    /// retained bounds, moved through its own view uniform and widened by the
    /// unscaled stroke/symbol margin, misses the surface draws nothing and is
    /// not encoded. Unknown bounds or raster layers keep every copy.
    fn wrap_pass_visibility(&self, count: u8) -> [bool; 3] {
        let mut visible = [true, count > 1, count > 2];
        let frame = &self.emission.vector_frame;
        let Some(b) = frame.scene_bounds.filter(|_| count == 3) else {
            return visible;
        };
        if !self.raster_layers().is_empty() {
            return visible;
        }
        for (pass, shift) in [
            (1, -frame.lon_wrap_screen_px),
            (2, frame.lon_wrap_screen_px),
        ] {
            visible[pass] = wrap_copy_may_reach_surface(
                b,
                shift,
                frame.screen_pan_offset,
                frame.screen_zoom_pivot,
                [frame.screen_zoom_scale, frame.screen_zoom_scale_y],
                self.extent,
            );
        }
        visible
    }
    fn is_device_fixed_source(&self, source: usize) -> bool {
        self.emission
            .vector_frame
            .static_source_classification
            .as_ref()
            .map_or_else(
                || {
                    self.emission
                        .vector_frame
                        .device_fixed_sources
                        .contains(&source)
                },
                |s| s.is_device_fixed(source),
            )
    }
    fn bind_coverage_pipeline<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        primitive: CoveragePrimitive,
        source: Option<usize>,
        wrap_pass: usize,
    ) -> bool {
        use crate::coverage_gpu_frame::CoverageGpuBinding;
        if self.emission.vector_frame.coverage_failed
            || (wrap_pass != 0 && source.is_some_and(|s| self.is_device_fixed_source(s)))
        {
            return false;
        }
        let binding = match resolve_frame_binding(&self.emission.vector_frame, source, wrap_pass) {
            Ok(binding) => binding,
            Err(error) => {
                tracing::error!("Coverage draw rejected: {error}");
                return false;
            }
        };
        match binding {
            CoverageGpuBinding::Hidden => false,
            CoverageGpuBinding::Unclipped => {
                pass.set_pipeline(match primitive {
                    CoveragePrimitive::Area => &self.pipelines.area_pipeline,
                    CoveragePrimitive::Line => {
                        if self.line_compact {
                            let Some(pipelines) = &self.compact else {
                                return false;
                            };
                            &pipelines.ordinary
                        } else {
                            &self.pipelines.line_pipeline
                        }
                    }
                    CoveragePrimitive::Symbol => self
                        .pipelines
                        .symbol_instance_pipeline
                        .as_ref()
                        .unwrap_or(&self.pipelines.texture_pipeline),
                    CoveragePrimitive::Pattern => &self.pipelines.pattern_fill_pipeline,
                    CoveragePrimitive::Text => &self.pipelines.chart_text_pipeline,
                });
                true
            }
            CoverageGpuBinding::Masked(group) => {
                let Some(pipelines) = &self.emission.coverage_pipelines else {
                    return false;
                };
                pass.set_pipeline(match primitive {
                    CoveragePrimitive::Area => &pipelines.area,
                    CoveragePrimitive::Line => {
                        if self.line_compact {
                            let Some(compact) = &self.compact else {
                                return false;
                            };
                            let Some(masked) = compact.masked(pipelines) else {
                                return false;
                            };
                            masked
                        } else {
                            &pipelines.line
                        }
                    }
                    CoveragePrimitive::Symbol => pipelines
                        .symbol_instance
                        .as_ref()
                        .unwrap_or(&pipelines.symbol),
                    CoveragePrimitive::Pattern => &pipelines.pattern,
                    CoveragePrimitive::Text => &pipelines.text,
                });
                if matches!(primitive, CoveragePrimitive::Area | CoveragePrimitive::Line) {
                    pass.set_bind_group(1, &pipelines.empty_asset, &[]);
                }
                pass.set_bind_group(2, group, &[]);
                true
            }
        }
    }
    pub(super) fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        resolve_target: Option<&wgpu::TextureView>,
        gpu_query: Option<&wgpu_profiler::GpuProfilerQuery>,
        batch_timestamp_writes: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) {
        let cached_plan = self.emission.scene_draw_plan.prepare(self);
        let draw_index = if cached_plan.is_none() && self.draw_range_index_enabled {
            let total = self
                .emission
                .vector_frame
                .frame_cpu
                .area_priority_ranges
                .len()
                .checked_add(self.emission.vector_frame.frame_cpu.pattern_ranges.len())
                .and_then(|n| {
                    n.checked_add(
                        self.emission
                            .vector_frame
                            .frame_cpu
                            .line_priority_ranges
                            .len(),
                    )
                })
                .and_then(|n| {
                    n.checked_add(
                        self.emission
                            .vector_frame
                            .frame_cpu
                            .symbol_priority_ranges
                            .len(),
                    )
                })
                .and_then(|n| n.checked_add(self.text.len()))
                .and_then(|n| n.checked_add(self.raster_layers().len()));
            total.and_then(DrawRangeIndex::new).map(|mut index| {
                for (i, &(p, q, _, _, _)) in self
                    .emission
                    .vector_frame
                    .frame_cpu
                    .area_priority_ranges
                    .iter()
                    .enumerate()
                {
                    index.push(DrawKind::Area, (p, q), i);
                }
                for (i, &(p, q, _, _, _, _, _)) in self
                    .emission
                    .vector_frame
                    .frame_cpu
                    .pattern_ranges
                    .iter()
                    .enumerate()
                {
                    index.push(DrawKind::Pattern, (p, q), i);
                }
                for (i, &(p, q, _, _, _)) in self
                    .emission
                    .vector_frame
                    .frame_cpu
                    .line_priority_ranges
                    .iter()
                    .enumerate()
                {
                    index.push(DrawKind::Line, (p, q), i);
                }
                for (i, &(p, q, _, _, _)) in self
                    .emission
                    .vector_frame
                    .frame_cpu
                    .symbol_priority_ranges
                    .iter()
                    .enumerate()
                {
                    index.push(DrawKind::Symbol, (p, q), i);
                }
                for (i, text) in self.text.iter().enumerate() {
                    index.push(DrawKind::Text, (text.plane, text.priority), i);
                }
                for (i, layer) in self.raster_layers().iter().enumerate() {
                    index.push(DrawKind::Raster, layer.draw_order.render_key(), i);
                }
                index.finish();
                index
            })
        } else {
            None
        };
        let draw_index = cached_plan
            .as_ref()
            .and_then(|p| p.index.as_ref())
            .or(draw_index.as_ref());
        // Preserve position() first-match semantics, including duplicate keys.
        let symbol_lookup = draw_index
            .filter(|_| cached_plan.is_none() && self.symbols.len() <= 32_768)
            .map(|_| {
                let mut lookup = FxHashMap::default();
                for (i, (p, q, start, end, _, _, _)) in self.symbols.iter().enumerate() {
                    lookup.entry((*p, *q, *start, *end)).or_insert(i);
                }
                lookup
            });
        let bg = self.background.to_array();
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    resolve_target,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: bg[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: batch_timestamp_writes
                    .or_else(|| gpu_query.and_then(|query| query.render_pass_timestamp_writes())),
            });

            let extent = self.extent;
            let viewport = self
                .emission
                .vector_frame
                .chart_geometry_viewport
                .unwrap_or_else(|| {
                    ferrite_render::Viewport::new(extent[0] as f32, extent[1] as f32)
                });
            let Some(chart_scissor) = chart_pass_scissor(viewport, extent) else {
                return;
            };
            render_pass.set_scissor_rect(
                chart_scissor[0],
                chart_scissor[1],
                chart_scissor[2],
                chart_scissor[3],
            );

            // === LAYER 1: World map coastlines (lowest layer) ===
            if let (Some(vb), Some(ib)) = (self.world_lines.vertices, self.world_lines.indices) {
                let idx_count = self
                    .emission
                    .vector_frame
                    .frame_cpu
                    .world_map_line_indices
                    .len() as u32;
                if idx_count > 0 {
                    render_pass.set_pipeline(&self.pipelines.line_pipeline);
                    render_pass.set_bind_group(0, self.views[0], &[]);
                    render_pass.set_vertex_buffer(0, vb.slice(..));
                    render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                    render_pass.draw_indexed(0..idx_count, 0, 0..1);
                }
            }

            // === LAYER 2: Opaque background rectangles over chart bboxes (mask coastlines) ===
            if let (Some(vb), Some(ib)) = (self.world_masks.vertices, self.world_masks.indices) {
                let idx_count = self
                    .emission
                    .vector_frame
                    .frame_cpu
                    .world_map_mask_indices
                    .len() as u32;
                if idx_count > 0 {
                    render_pass.set_pipeline(&self.pipelines.area_pipeline);
                    render_pass.set_bind_group(0, self.views[0], &[]);
                    render_pass.set_vertex_buffer(0, vb.slice(..));
                    render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                    render_pass.draw_indexed(0..idx_count, 0, 0..1);
                }
            }

            // === LAYER 3: Chart data (S-101 priority-based rendering) ===
            // Drawn at center, left (-360°), and right (+360°) offsets for wrapping.
            // Raster group visibility remains live; cached metadata grants no draw permission.
            let all_priorities: std::borrow::Cow<'_, [(CompositionPlane, i32)]> =
                if let Some(plan) = &cached_plan {
                    if self.raster_layers().is_empty() {
                        std::borrow::Cow::Borrowed(&plan.priorities)
                    } else {
                        let mut priorities = plan.priorities.clone();
                        for layer in self.raster_layers() {
                            if ferrite_render::raster_groups_visible(
                                &layer.viewing_groups,
                                self.raster_groups(),
                            ) {
                                priorities.push(layer.draw_order.render_key());
                            }
                        }
                        priorities.sort_unstable();
                        priorities.dedup();
                        std::borrow::Cow::Owned(priorities)
                    }
                } else {
                    let mut priority_set = FxHashSet::default();
                    for &(plane, pri, _, _, _) in
                        &self.emission.vector_frame.frame_cpu.area_priority_ranges
                    {
                        priority_set.insert((plane, pri));
                    }
                    for &(plane, pri, _, _, _) in
                        &self.emission.vector_frame.frame_cpu.line_priority_ranges
                    {
                        priority_set.insert((plane, pri));
                    }
                    for &(plane, pri, _, _, _) in
                        &self.emission.vector_frame.frame_cpu.symbol_priority_ranges
                    {
                        priority_set.insert((plane, pri));
                    }
                    for &(plane, pri, _, _, _, _, _) in
                        &self.emission.vector_frame.frame_cpu.pattern_ranges
                    {
                        priority_set.insert((plane, pri));
                    }
                    for text in self.text {
                        priority_set.insert((text.plane, text.priority));
                    }
                    for layer in self.raster_layers() {
                        if ferrite_render::raster_groups_visible(
                            &layer.viewing_groups,
                            self.raster_groups(),
                        ) {
                            priority_set.insert(layer.draw_order.render_key());
                        }
                    }
                    let mut all_priorities: Vec<(CompositionPlane, i32)> =
                        priority_set.into_iter().collect();
                    all_priorities.sort_unstable();

                    std::borrow::Cow::Owned(all_priorities)
                };

            // Number of wrapping passes: center (always) + left/right if wrapping
            let wrap_pass_count: u8 = if self.emission.vector_frame.lon_wrap_screen_px > 0.0 {
                3
            } else {
                1
            };

            let wrap_visible = self.wrap_pass_visibility(wrap_pass_count);

            // Complete each priority across longitude copies before advancing.
            // Otherwise a low-priority wrapped copy can cover an earlier high-priority copy.
            for &(plane, priority) in all_priorities.iter() {
                for wrap_pass in 0..wrap_pass_count {
                    if !wrap_visible[wrap_pass as usize] {
                        continue;
                    }
                    let view_bg = match wrap_pass {
                        1 => self.views[1],
                        2 => self.views[2],
                        _ => self.views[0],
                    };
                    if let Some(renderer) = self.raster_renderer {
                        renderer.draw_rasters(
                            &mut render_pass,
                            view_bg,
                            (plane, priority),
                            draw_index,
                        );
                    }
                    // Render areas for this priority
                    if let (Some(vb), Some(ib)) = (self.area.vertices, self.area.indices) {
                        for range_index in selected_indices(
                            draw_index,
                            DrawKind::Area,
                            (plane, priority),
                            self.emission
                                .vector_frame
                                .frame_cpu
                                .area_priority_ranges
                                .len(),
                        ) {
                            let (pl, pri, start, end, source) =
                                self.emission.vector_frame.frame_cpu.area_priority_ranges
                                    [range_index];
                            if pl == plane && pri == priority && end > start {
                                if !self.bind_coverage_pipeline(
                                    &mut render_pass,
                                    CoveragePrimitive::Area,
                                    source,
                                    wrap_pass as usize,
                                ) {
                                    continue;
                                }
                                render_pass.set_bind_group(0, view_bg, &[]);
                                render_pass.set_vertex_buffer(0, vb.slice(..));
                                render_pass
                                    .set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                                render_pass.draw_indexed(start as u32..end as u32, 0, 0..1);
                            }
                        }
                    }

                    // Render pattern fills for this priority
                    if let (Some(vb), Some(ib)) = (self.pattern.vertices, self.pattern.indices) {
                        for range_index in selected_indices(
                            draw_index,
                            DrawKind::Pattern,
                            (plane, priority),
                            self.emission.vector_frame.frame_cpu.pattern_ranges.len(),
                        ) {
                            let (pl, pri, start, end, pat_key, wrap_mode, source) =
                                &self.emission.vector_frame.frame_cpu.pattern_ranges[range_index];
                            if *pl == plane
                                && *pri == priority
                                && end > start
                                && (*wrap_mode == 255 || *wrap_mode == wrap_pass)
                            {
                                if let Some(pat_tex) = self.emission.pattern_textures.get(pat_key) {
                                    if !self.bind_coverage_pipeline(
                                        &mut render_pass,
                                        CoveragePrimitive::Pattern,
                                        *source,
                                        wrap_pass as usize,
                                    ) {
                                        continue;
                                    }
                                    render_pass.set_bind_group(
                                        0,
                                        if *wrap_mode == 255 {
                                            view_bg
                                        } else {
                                            self.views[0]
                                        },
                                        &[],
                                    );
                                    render_pass.set_bind_group(1, &pat_tex.bind_group, &[]);
                                    render_pass.set_vertex_buffer(0, vb.slice(..));
                                    render_pass
                                        .set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                                    render_pass.draw_indexed(*start as u32..*end as u32, 0, 0..1);
                                }
                            }
                        }
                    }

                    // Render lines for this priority
                    if let (Some(vb), Some(ib)) = (self.line.vertices, self.line.indices) {
                        for range_index in selected_indices(
                            draw_index,
                            DrawKind::Line,
                            (plane, priority),
                            self.emission
                                .vector_frame
                                .frame_cpu
                                .line_priority_ranges
                                .len(),
                        ) {
                            let (pl, pri, start, end, source) =
                                self.emission.vector_frame.frame_cpu.line_priority_ranges
                                    [range_index];
                            if pl == plane && pri == priority && end > start {
                                if !self.bind_coverage_pipeline(
                                    &mut render_pass,
                                    CoveragePrimitive::Line,
                                    source,
                                    wrap_pass as usize,
                                ) {
                                    continue;
                                }
                                render_pass.set_bind_group(0, view_bg, &[]);
                                render_pass.set_vertex_buffer(0, vb.slice(..));
                                render_pass
                                    .set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                                if self.line_compact {
                                    render_pass.draw_indexed(
                                        0..6,
                                        0,
                                        (start / 6) as u32..(end / 6) as u32,
                                    );
                                } else {
                                    render_pass.draw_indexed(start as u32..end as u32, 0, 0..1);
                                }
                            }
                        }
                    }

                    // Render symbols for this priority (pre-built GPU buffers)
                    for range_index in selected_indices(
                        draw_index,
                        DrawKind::Symbol,
                        (plane, priority),
                        self.emission
                            .vector_frame
                            .frame_cpu
                            .symbol_priority_ranges
                            .len(),
                    ) {
                        let (pl, pri, start, end, source) =
                            self.emission.vector_frame.frame_cpu.symbol_priority_ranges
                                [range_index];
                        if pl == plane && pri == priority && end > start {
                            // Find pre-built buffer (built before render pass)
                            let cache_idx = if let Some(plan) =
                                cached_plan.as_ref().filter(|p| p.symbol_lookup_enabled())
                            {
                                plan.symbol((pl, pri, start, end))
                            } else {
                                match &symbol_lookup {
                                    Some(lookup) => lookup.get(&(pl, pri, start, end)).copied(),
                                    None => self.symbols.iter().position(
                                        |(cp, cpr, cs, ce, _, _, _)| {
                                            *cp == pl && *cpr == pri && *cs == start && *ce == end
                                        },
                                    ),
                                }
                            };
                            if let Some(buf_idx) = cache_idx {
                                let (_, _, _, _, ref sym_vb, ref sym_ib, ref ranges) =
                                    self.symbols[buf_idx];

                                if !self.bind_coverage_pipeline(
                                    &mut render_pass,
                                    CoveragePrimitive::Symbol,
                                    source,
                                    wrap_pass as usize,
                                ) {
                                    continue;
                                }
                                render_pass.set_bind_group(0, view_bg, &[]);
                                render_pass.set_vertex_buffer(0, sym_vb.slice(..));
                                render_pass
                                    .set_index_buffer(sym_ib.slice(..), wgpu::IndexFormat::Uint32);

                                for &(sym_id, idx_start, idx_count) in ranges {
                                    if let Some(tex) = self.emission.symbol_textures.get(&sym_id) {
                                        render_pass.set_bind_group(1, &tex.bind_group, &[]);
                                        if self.pipelines.symbol_instance_pipeline.is_some() {
                                            // Original six indices per accepted symbol map to consecutive instances.
                                            render_pass.draw_indexed(
                                                0..6,
                                                0,
                                                idx_start / 6..(idx_start + idx_count) / 6,
                                            );
                                        } else {
                                            render_pass.draw_indexed(
                                                idx_start..idx_start + idx_count,
                                                0,
                                                0..1,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                for text_index in selected_indices(
                    draw_index,
                    DrawKind::Text,
                    (plane, priority),
                    self.text.len(),
                ) {
                    let text = &self.text[text_index];
                    if text.plane == plane && text.priority == priority {
                        let Some([x, y, width, height]) =
                            intersect_scissors(text.scissor, chart_scissor)
                        else {
                            continue;
                        };
                        render_pass.set_scissor_rect(x, y, width, height);
                        if !self.bind_coverage_pipeline(
                            &mut render_pass,
                            CoveragePrimitive::Text,
                            text.source,
                            text.wrap_pass as usize,
                        ) {
                            continue;
                        }
                        render_pass.set_bind_group(0, self.views[0], &[]);
                        render_pass.set_bind_group(1, &text.bind_group, &[]);
                        render_pass.set_vertex_buffer(0, text.vertices.slice(..));
                        render_pass
                            .set_index_buffer(text.indices.slice(..), wgpu::IndexFormat::Uint32);
                        render_pass.draw_indexed(0..text.index_count, 0, 0..1);
                    }
                }
                // Text has its own clip; the next priority must return to the
                // chart pane, not to the full surface behind application chrome.
                render_pass.set_scissor_rect(
                    chart_scissor[0],
                    chart_scissor[1],
                    chart_scissor[2],
                    chart_scissor[3],
                );
            }
            // Dedicated S-98 annotation; never enters instructions, displayed
            // geometry, dependency execution or selection IDs. Affine previews
            // withhold a stale mask until authoritative coverage is prepared.
            if !self.emission.vector_frame.coverage_failed
                && self.emission.vector_frame.screen_pan_offset == (0., 0.)
                && self.emission.vector_frame.screen_zoom_scale == 1.
                && self.emission.vector_frame.screen_zoom_scale_y == 1.
            {
                for annotation in &self.emission.vector_frame.overscale_annotation {
                    annotation.encode(&mut render_pass, self.views[0], extent);
                }
            }
            // Native host editing illustration, independently owned and clipped.
            // Its identity uniform never inherits the ENC affine preview.
            if let Some(renderer) = self.raster_renderer {
                if let Some(native) = &renderer.native_route_gpu {
                    let (x, y, width, height) = renderer.chart_viewport_pixels();
                    let current_scissor = chart_pass_scissor(
                        ferrite_render::Viewport::with_origin(x, y, width, height),
                        extent,
                    );
                    if native.gpu_identity_matches(&renderer.state, self.pipelines)
                        && native.frame_matches(
                            renderer.vector_emission.native_route_target_camera,
                            extent,
                            current_scissor,
                        )
                    {
                        native.draw(&mut render_pass, self.pipelines);
                        renderer.native_route_last_encoded.set(true);
                    }
                }
            }
        }
    }
}

pub(super) fn resolve_frame_binding(
    frame: &VectorFrameState,
    source: Option<usize>,
    wrap_pass: usize,
) -> Result<crate::coverage_gpu_frame::CoverageGpuBinding<'_>> {
    use crate::coverage_gpu_frame::CoverageGpuBinding;
    if frame.coverage_failed {
        return Err(WgpuError::Render("Coverage preparation failed".into()));
    }
    let fixed = |index| {
        frame.static_source_classification.as_ref().map_or_else(
            || frame.device_fixed_sources.contains(&index),
            |c| c.is_device_fixed(index),
        )
    };
    if wrap_pass != 0 && source.is_some_and(fixed) {
        return Ok(CoverageGpuBinding::Hidden);
    }
    match source {
        None => Ok(CoverageGpuBinding::Unclipped),
        Some(index) => match (&frame.coverage_frame, &frame.prepared_coverage) {
            (Some(gpu), Some(prepared)) => gpu.resolve_instruction(prepared, wrap_pass, index),
            (None, None) if fixed(index) => Ok(CoverageGpuBinding::Unclipped),
            _ => Err(WgpuError::Render(
                "Chart source coverage binding missing".into(),
            )),
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_coverage_admission_preserves_unclipped_fixed_and_wrap_hidden() {
        use crate::coverage_gpu_frame::CoverageGpuBinding;
        let mut frame = VectorFrameState::empty_fixture();
        assert!(matches!(
            resolve_frame_binding(&frame, None, 0).unwrap(),
            CoverageGpuBinding::Unclipped
        ));
        assert!(resolve_frame_binding(&frame, Some(0), 0).is_err());
        frame.device_fixed_sources.insert(0);
        assert!(matches!(
            resolve_frame_binding(&frame, Some(0), 0).unwrap(),
            CoverageGpuBinding::Unclipped
        ));
        assert!(matches!(
            resolve_frame_binding(&frame, Some(0), 1).unwrap(),
            CoverageGpuBinding::Hidden
        ));
        assert!(resolve_frame_binding(&frame, Some(1), 0).is_err());
        frame.coverage_failed = true;
        assert!(resolve_frame_binding(&frame, None, 0).is_err());
        assert!(resolve_frame_binding(&frame, Some(0), 0).is_err());
    }
}

#[cfg(test)]
mod wrap_cull_tests {
    use super::wrap_copy_may_reach_surface as reach;
    const EXTENT: [u32; 2] = [1600, 1000];
    #[test]
    fn far_copies_are_culled_and_near_or_zoomed_out_copies_are_kept() {
        let chart = [100., 100., 1500., 900.];
        // 360 degrees far beyond the surface: both copies miss.
        assert!(!reach(
            chart,
            -40_000.,
            (0., 0.),
            (0., 0.),
            [1., 1.],
            EXTENT
        ));
        assert!(!reach(chart, 40_000., (0., 0.), (0., 0.), [1., 1.], EXTENT));
        // World view: the copy one wrap away overlaps the surface.
        assert!(reach(chart, -1_200., (0., 0.), (0., 0.), [1., 1.], EXTENT));
        // An affine zoom-out pulls a far copy back onto the surface.
        assert!(reach(chart, 4_000., (0., 0.), (0., 0.), [0.2, 0.2], EXTENT));
    }
    #[test]
    fn margin_keeps_symbols_and_strokes_just_outside_the_anchor_bounds() {
        let point = [0., 500., 0., 500.];
        // Anchor 1000 px left of the surface: a symbol may still reach it.
        assert!(reach(point, -1_000., (0., 0.), (0., 0.), [1., 1.], EXTENT));
        assert!(!reach(point, -1_100., (0., 0.), (0., 0.), [1., 1.], EXTENT));
    }
    #[test]
    fn non_finite_transform_keeps_the_copy() {
        let chart = [0., 0., 10., 10.];
        assert!(reach(chart, f32::NAN, (0., 0.), (0., 0.), [1., 1.], EXTENT));
    }
}
