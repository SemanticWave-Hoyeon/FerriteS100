//! Product-neutral globe coverage. Local physical pixels are shared by color
//! rendering and ID picking. A binding belongs to one prepared camera epoch.
use super::*;
use crate::coverage_clip::{
    create_clip_layout, fragment_clipped_shader, ClipTransform, CoverageClip, FragmentEntry,
};
use ferrite_kernel::coverage_frame::{CoverageFrame, FrameCoverageDecision};
use std::collections::BTreeMap;

pub(super) enum CoverageAction {
    Unclipped,
    Hidden,
    Clip(Arc<CoverageClip>),
}
#[derive(Debug, PartialEq, Eq)]
pub(super) struct CoverageBatch {
    pub range: std::ops::Range<u32>,
    pub group: usize,
    pub source: usize,
}
struct BatchInput {
    range: std::ops::Range<u32>,
    group: usize,
    source: usize,
    decision: FrameCoverageDecision,
}
fn append_batch(
    result: &mut Vec<CoverageBatch>,
    previous_decision: &mut Option<FrameCoverageDecision>,
    input: BatchInput,
    enabled: bool,
) {
    if input.range.is_empty() || input.decision == FrameCoverageDecision::Hidden {
        return;
    }
    if enabled && *previous_decision == Some(input.decision) {
        if let Some(last) = result
            .last_mut()
            .filter(|last| last.group == input.group && last.range.end == input.range.start)
        {
            last.range.end = input.range.end;
            return;
        }
    }
    *previous_decision = Some(input.decision);
    result.push(CoverageBatch {
        range: input.range,
        group: input.group,
        source: input.source,
    });
}
#[cfg(test)]
fn build_batches(
    inputs: impl IntoIterator<Item = BatchInput>,
    enabled: bool,
) -> Vec<CoverageBatch> {
    let mut result = Vec::new();
    let mut previous_decision = None;
    for input in inputs {
        append_batch(&mut result, &mut previous_decision, input, enabled);
    }
    result
}

pub(super) struct CoverageBinding {
    pub epoch: u64,
    pub actions: Vec<CoverageAction>,
    pub pixel_bytes: usize,
    pub mask_count: usize,
    pub batches: Vec<CoverageBatch>,
}
pub(super) struct CoveragePipelines {
    pub layout: wgpu::BindGroupLayout,
    pub base: wgpu::RenderPipeline,
    pub overlay: wgpu::RenderPipeline,
    pub screen: wgpu::RenderPipeline,
    pub texture: Option<wgpu::RenderPipeline>,
    pub surface_texture: Option<wgpu::RenderPipeline>,
    pub source_grid_texture: Option<wgpu::RenderPipeline>,
    pub pattern: Option<wgpu::RenderPipeline>,
}
impl CoveragePipelines {
    fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        samples: u32,
        textures: Option<&wgpu::BindGroupLayout>,
        patterns: Option<&wgpu::BindGroupLayout>,
    ) -> Result<Self, String> {
        let layout = create_clip_layout(device);
        let make = |textured: bool, mode: GlobeDepthMode| -> Result<wgpu::RenderPipeline, String> {
            let entries = if textured {
                vec![
                    FragmentEntry {
                        name: "fs_main",
                        input_type: "Out",
                        position_field: "position",
                    },
                    FragmentEntry {
                        name: "fs_linear",
                        input_type: "Out",
                        position_field: "position",
                    },
                ]
            } else {
                vec![FragmentEntry {
                    name: "fs_main",
                    input_type: "Out",
                    position_field: "position",
                }]
            };
            let grid_source=globe_grid_shader();
            let source = fragment_clipped_shader(
                if textured {
                    if mode==GlobeDepthMode::SourceGridTexture {&grid_source} else {GLOBE_TEXTURE_SHADER}
                } else {
                    GLOBE_COLOR_SHADER
                },
                u32::from(textured),
                &entries,
            )
            .map_err(|e| e.to_string())?;
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Coverage-clipped globe"),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
            let layouts = if textured {
                vec![textures.unwrap(), &layout]
            } else {
                vec![&layout]
            };
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Globe coverage"),
                bind_group_layouts: &layouts,
                push_constant_ranges: &[],
            });
            let attributes = wgpu::vertex_attr_array![0=>Float32x4,1=>Float32x4,2=>Unorm8x4];
            Ok(
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("Coverage-clipped globe"),
                    layout: Some(&pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_main"),
                        buffers: &[wgpu::VertexBufferLayout {
                            array_stride: 36,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &attributes,
                        }],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some(if textured && format.is_srgb() {
                            "fs_linear"
                        } else {
                            "fs_main"
                        }),
                        targets: &[Some(wgpu::ColorTargetState {
                            format,
                            blend: Some(if textured {
                                wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING
                            } else {
                                wgpu::BlendState::ALPHA_BLENDING
                            }),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: Default::default(),
                    }),
                    primitive: wgpu::PrimitiveState {
                        cull_mode: None,
                        ..Default::default()
                    },
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: wgpu::TextureFormat::Depth32Float,
                        depth_write_enabled: mode == GlobeDepthMode::Occluder,
                        depth_compare: depth_compare(mode,textured),
                        stencil: Default::default(),
                        bias: depth_bias(mode,textured),
                    }),
                    multisample: wgpu::MultisampleState {
                        count: samples,
                        ..Default::default()
                    },
                    multiview: None,
                    cache: None,
                }),
            )
        };
        let source_grid_texture=textures.map(|_|make(true,GlobeDepthMode::SourceGridTexture)).transpose()?;
        let surface_texture=textures.map(|_|make(true,GlobeDepthMode::SurfaceTexture)).transpose()?;
        let base = make(false, GlobeDepthMode::Occluder)?;
        let overlay = make(false, GlobeDepthMode::SurfaceOverlay)?;
        let screen = make(false, GlobeDepthMode::ScreenOverlay)?;
        let texture = textures
            .map(|_| make(true, GlobeDepthMode::SurfaceOverlay))
            .transpose()?;
        let pattern = patterns
            .map(|p| create_globe_pattern_pipeline(device, format, samples, p, Some(&layout)))
            .transpose()?;
        Ok(Self {
            layout,
            base,
            overlay,
            screen,
            texture,
            surface_texture,
            source_grid_texture,
            pattern,
        })
    }
}
impl GlobeSceneRenderer {
    /// Capture this ticket before projecting coverage for the prepared camera.
    /// A later geometry/camera preparation invalidates it even at equal sizes.
    pub fn coverage_epoch(&self) -> Option<u64> {
        self.prepared_viewport.map(|_| self.prepared_epoch)
    }
    /// Explicitly require a fresh coverage binding on every prepare. Disabling
    /// is reserved for a caller's known exempt/legacy scene, never a failed bind.
    pub fn require_coverage(&mut self, required: bool) {
        self.coverage_required = required;
        if !required {
            self.coverage_binding = None;
        }
    }
    pub fn bind_prepared_coverage(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        epoch: u64,
        frame: &CoverageFrame,
        decisions: &[FrameCoverageDecision],
        byte_budget: usize,
    ) -> Result<usize, String> {
        self.coverage_required = true;
        self.coverage_binding = None;
        if self.coverage_epoch() != Some(epoch) || decisions.len() != self.source_draws {
            return Err("Globe coverage belongs to another prepared camera or draw order".into());
        }
        // Existing color groups already encode exact texture-object identity,
        // depth policy and original painter order. Split only at mask changes.
        let mut batches = Vec::new();
        if self.coverage_batching_enabled {
            let mut group = 0usize;
            let mut previous_decision = None;
            for (draw, range, _, _, _) in &self.pick_ranges {
                if range.is_empty() {
                    continue;
                }
                while self
                    .ranges
                    .get(group)
                    .is_some_and(|(parent, _, _, _)| parent.end <= range.start)
                {
                    group += 1;
                }
                let parent = self
                    .ranges
                    .get(group)
                    .ok_or("Coverage draw has no color group")?;
                if range.start < parent.0.start || range.end > parent.0.end {
                    return Err("Coverage draw crosses color group".into());
                }
                let decision = *decisions
                    .get(*draw as usize)
                    .ok_or("Coverage source outside binding")?;
                append_batch(
                    &mut batches,
                    &mut previous_decision,
                    BatchInput {
                        range: range.clone(),
                        group,
                        source: *draw as usize,
                        decision,
                    },
                    true,
                );
            }
        }
        let mut required = BTreeMap::new();
        let mut bytes = 0usize;
        for decision in decisions {
            if let FrameCoverageDecision::ClipDataset(id) = decision {
                if required.contains_key(id) {
                    continue;
                }
                let mask = frame
                    .mask(*id)
                    .ok_or("Globe coverage dataset has no selected mask")?;
                let size = mask.size();
                let count = if size.contains(&0) {
                    1
                } else {
                    (size[0] as usize)
                        .checked_mul(size[1] as usize)
                        .ok_or("Globe coverage byte overflow")?
                };
                bytes = bytes
                    .checked_add(count)
                    .ok_or("Globe coverage byte overflow")?;
                if bytes > byte_budget {
                    return Err("Globe coverage texture budget exceeded".into());
                }
                required.insert(*id, mask);
            }
        }
        if !required.is_empty() && self.coverage_pipelines.is_none() {
            self.coverage_pipelines = Some(CoveragePipelines::new(
                device,
                self.format,
                self.samples,
                self.pick_texture_layout.as_ref(),
                self.pattern_pipeline.as_ref().map(|_| &self.pattern_layout),
            )?);
        }
        if !required.is_empty() && self.pattern_pipeline.is_some() {
            let pipelines = self.coverage_pipelines.as_mut().unwrap();
            if pipelines.pattern.is_none() {
                pipelines.pattern = Some(create_globe_pattern_pipeline(
                    device,
                    self.format,
                    self.samples,
                    &self.pattern_layout,
                    Some(&pipelines.layout),
                )?);
            }
        }
        let mut resources = BTreeMap::new();
        for (id, mask) in required {
            let clip = CoverageClip::new(
                device,
                queue,
                &self.coverage_pipelines.as_ref().unwrap().layout,
                Some(mask),
                ClipTransform::IDENTITY,
                byte_budget,
            )
            .map_err(|e| e.to_string())?;
            resources.insert(id, Arc::new(clip));
        }
        let actions = decisions
            .iter()
            .map(|d| match d {
                FrameCoverageDecision::Unclipped => CoverageAction::Unclipped,
                FrameCoverageDecision::Hidden => CoverageAction::Hidden,
                FrameCoverageDecision::ClipDataset(id) => {
                    CoverageAction::Clip(resources[id].clone())
                }
            })
            .collect();
        self.coverage_binding = Some(CoverageBinding {
            epoch,
            actions,
            pixel_bytes: bytes,
            mask_count: resources.len(),
            batches,
        });
        Ok(bytes)
    }
    /// Toggle adjacent color batching. Picking retains original source IDs.
    /// A changed policy requires a fresh binding before rendering or picking.
    pub fn set_coverage_batching_enabled(&mut self, enabled: bool) {
        if self.coverage_batching_enabled != enabled {
            self.coverage_batching_enabled = enabled;
            self.coverage_binding = None;
        }
    }
    pub fn coverage_pixel_bytes(&self) -> usize {
        self.coverage_binding.as_ref().map_or(0, |c| c.pixel_bytes)
    }
    pub(super) fn validate_coverage(&self) -> Result<(), String> {
        if self.coverage_required && self.coverage_binding.is_none() {
            return Err("Required globe coverage is missing or stale".into());
        }
        if self
            .coverage_binding
            .as_ref()
            .is_some_and(|c| c.epoch != self.prepared_epoch || c.actions.len() != self.source_draws)
        {
            return Err("Globe coverage belongs to another prepared camera or draw order".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    #[test]
    fn contiguous_batches_preserve_indices_mask_texture_depth_and_source_ids() {
        use FrameCoverageDecision::{ClipDataset, Hidden, Unclipped};
        let decisions = [Unclipped, ClipDataset(0), ClipDataset(1), Hidden];
        for a in decisions {
            for b in decisions {
                for c in decisions {
                    for groups in [[0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 2]] {
                        let make = || {
                            [a, b, c]
                                .into_iter()
                                .enumerate()
                                .map(|(i, decision)| BatchInput {
                                    range: (i as u32 * 3)..(i as u32 * 3 + 3),
                                    group: groups[i],
                                    source: i,
                                    decision,
                                })
                        };
                        let off = build_batches(make(), false);
                        let on = build_batches(make(), true);
                        // Compare complete index sequence and group, without reordering.
                        let flatten = |v: &[CoverageBatch]| {
                            v.iter()
                                .flat_map(|batch| {
                                    batch.range.clone().map(move |index| {
                                        (index, batch.group, [a, b, c][batch.source])
                                    })
                                })
                                .collect::<Vec<_>>()
                        };
                        assert_eq!(flatten(&off), flatten(&on));
                        for batch in on {
                            assert_ne!([a, b, c][batch.source], Hidden);
                            assert_eq!(batch.range.start / 3, batch.source as u32);
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn gaps_hidden_indices_and_empty_draws_do_not_bridge_or_reorder() {
        let input = |start, end, source, decision| BatchInput {
            range: start..end,
            source,
            group: 0,
            decision,
        };
        let result = build_batches(
            [
                input(0, 3, 0, FrameCoverageDecision::ClipDataset(0)),
                input(3, 6, 1, FrameCoverageDecision::Hidden),
                input(6, 6, 2, FrameCoverageDecision::ClipDataset(1)),
                input(6, 9, 3, FrameCoverageDecision::ClipDataset(0)),
                input(9, 12, 4, FrameCoverageDecision::ClipDataset(0)),
            ],
            true,
        );
        assert_eq!(
            result,
            vec![
                CoverageBatch {
                    range: 0..3,
                    group: 0,
                    source: 0
                },
                CoverageBatch {
                    range: 6..12,
                    group: 0,
                    source: 3
                }
            ]
        );
    }
}
