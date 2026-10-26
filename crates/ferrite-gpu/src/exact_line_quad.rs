//! Opt-in lossless line-quad representation; legacy emission remains authoritative.
//! Initial admission is AFTER legacy emission. Never change clipping or normal arithmetic.
use crate::LineVertex;

pub(crate) fn enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}

#[derive(Default, serde::Serialize)]
pub(crate) struct Work {
    pub attempts: u64,
    pub admitted: u64,
    pub declines: u64,
    pub legacy_payload_bytes: u64,
    pub packed_upload_bytes: u64,
    pub current_gpu_payload_bytes: u64,
    pub shared_index_uploads: u64,
}

/// Renderer/device-local pipelines and immutable index topology. Never queue-written.
/// Holding the exact coverage owner prevents foreign layout reuse after replacement.
pub(crate) struct Pipelines {
    pub ordinary: wgpu::RenderPipeline,
    pub indices: wgpu::Buffer,
    masked: Option<(
        std::sync::Arc<crate::coverage_pipeline::CoveragePipelines>,
        wgpu::RenderPipeline,
    )>,
}

impl Pipelines {
    pub(crate) fn matches_coverage(
        &self,
        owner: Option<&std::sync::Arc<crate::coverage_pipeline::CoveragePipelines>>,
    ) -> bool {
        match (&self.masked, owner) {
            (None, None) => true,
            (Some((held, _)), Some(owner)) => std::sync::Arc::ptr_eq(held, owner),
            _ => false,
        }
    }
    pub(crate) fn new(
        state: &crate::GpuState,
        view: &wgpu::BindGroupLayout,
    ) -> crate::Result<Self> {
        Ok(Self {
            ordinary: make_pipeline(state, view, None)?,
            indices: state.create_index_buffer(&QUAD_INDICES, "exact-line-shared-quad-index"),
            masked: None,
        })
    }

    pub(crate) fn prepare_masked(
        &mut self,
        state: &crate::GpuState,
        view: &wgpu::BindGroupLayout,
        owner: Option<&std::sync::Arc<crate::coverage_pipeline::CoveragePipelines>>,
    ) -> crate::Result<()> {
        let Some(owner) = owner else {
            self.masked = None;
            return Ok(());
        };
        if self
            .masked
            .as_ref()
            .is_some_and(|(old, _)| std::sync::Arc::ptr_eq(old, owner))
        {
            return Ok(());
        }
        let pipeline = make_pipeline(state, view, Some(&owner.clip_layout))?;
        self.masked = Some((std::sync::Arc::clone(owner), pipeline));
        Ok(())
    }

    pub(crate) fn masked(
        &self,
        owner: &std::sync::Arc<crate::coverage_pipeline::CoveragePipelines>,
    ) -> Option<&wgpu::RenderPipeline> {
        self.masked
            .as_ref()
            .filter(|(held, _)| std::sync::Arc::ptr_eq(held, owner))
            .map(|(_, p)| p)
    }
}

fn make_pipeline(
    state: &crate::GpuState,
    view: &wgpu::BindGroupLayout,
    clip: Option<&wgpu::BindGroupLayout>,
) -> crate::Result<wgpu::RenderPipeline> {
    let empty = state
        .device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("exact-line-empty-asset"),
            entries: &[],
        });
    let layouts = match clip {
        Some(clip) => vec![view, &empty, clip],
        None => vec![view],
    };
    let layout = state
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("exact-line-instance-layout"),
            bind_group_layouts: &layouts,
            push_constant_ranges: &[],
        });
    let source = include_str!("exact_line_quad.wgsl");
    let source = if clip.is_some() {
        crate::coverage_clip::fragment_clipped_shader(
            source,
            2,
            &[crate::coverage_clip::FragmentEntry {
                name: "fs_main",
                input_type: "VertexOutput",
                position_field: "clip_position",
            }],
        )?
    } else {
        source.to_owned()
    };
    let shader = state
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("exact-line-instance-shader"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
    Ok(state
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("exact-line-instance-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[ExactLineQuad::desc()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: state.format(),
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
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
        }))
}

pub(crate) const QUAD_INDICES: [u32; 6] = [0, 1, 2, 0, 2, 3];
// CPU packed payload + immutable GPU packed payload: <=16MiB logical charge.
// Original legacy buffers, old/new publication peaks and backend alignment are separate.
pub(crate) const CPU_CAP_BYTES: usize = 8 * 1024 * 1024;

fn admitted_quads(vertices: usize, indices: usize) -> Option<usize> {
    let n = vertices.checked_div(4)?;
    (vertices.is_multiple_of(4)
        && indices == n.checked_mul(6)?
        && n <= u32::MAX as usize / 4
        && n.checked_mul(std::mem::size_of::<ExactLineQuad>())? <= CPU_CAP_BYTES)
        .then_some(n)
}

pub(crate) fn layout_admitted(
    vertices: usize,
    indices: usize,
    mut ranges: impl Iterator<Item = (usize, usize)>,
) -> bool {
    admitted_quads(vertices, indices).is_some()
        && !ranges
            .any(|(a, b)| a > b || b > indices || !a.is_multiple_of(6) || !b.is_multiple_of(6))
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct ExactLineQuad {
    start: [f32; 2],
    end: [f32; 2],
    offset0: [f32; 2],
    offset1: [f32; 2],
    offset2: [f32; 2],
    offset3: [f32; 2],
    color: [f32; 4],
}

fn equal_bits<const N: usize>(a: [f32; N], b: [f32; N]) -> bool {
    a.map(f32::to_bits) == b.map(f32::to_bits)
}

impl ExactLineQuad {
    // All four offsets are stored. Do NOT negate in GPU shader: signed-zero and original
    // arithmetic remain exactly captured, including hatches and transformed anchors.
    fn from_legacy(v: &[LineVertex]) -> Option<Self> {
        if v.len() != 4
            || !equal_bits(v[0].position, v[1].position)
            || !equal_bits(v[2].position, v[3].position)
            || !v[1..].iter().all(|p| equal_bits(p.color, v[0].color))
            || !v.iter().all(|p| {
                p.position
                    .iter()
                    .chain(p.offset.iter())
                    .chain(p.color.iter())
                    .all(|v| v.is_finite())
            })
        {
            return None;
        }
        Some(Self {
            start: v[0].position,
            end: v[2].position,
            offset0: v[0].offset,
            offset1: v[1].offset,
            offset2: v[2].offset,
            offset3: v[3].offset,
            color: v[0].color,
        })
    }

    pub(crate) fn from_emitted(
        start: [f32; 2],
        end: [f32; 2],
        offsets: [[f32; 2]; 4],
        color: [f32; 4],
    ) -> Option<Self> {
        if !start
            .iter()
            .chain(end.iter())
            .chain(offsets.iter().flatten())
            .chain(color.iter())
            .all(|x| x.is_finite())
        {
            return None;
        }
        Some(Self {
            start,
            end,
            offset0: offsets[0],
            offset1: offsets[1],
            offset2: offsets[2],
            offset3: offsets[3],
            color,
        })
    }
    pub(crate) fn reanchored(self, anchor: [f32; 2]) -> Option<Self> {
        let mut offsets = [self.offset0, self.offset1, self.offset2, self.offset3];
        for (i, o) in offsets.iter_mut().enumerate() {
            let p = if i < 2 { self.start } else { self.end };
            o[0] += p[0] - anchor[0];
            o[1] += p[1] - anchor[1];
        }
        Self::from_emitted(anchor, anchor, offsets, self.color)
    }
    pub(crate) fn expand(self) -> [LineVertex; 4] {
        let vertex = |p: [f32; 2], o: [f32; 2]| LineVertex::new(p[0], p[1], o[0], o[1], self.color);
        [
            vertex(self.start, self.offset0),
            vertex(self.start, self.offset1),
            vertex(self.end, self.offset2),
            vertex(self.end, self.offset3),
        ]
    }

    pub(crate) fn desc() -> wgpu::VertexBufferLayout<'static> {
        const A: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
            0 => Float32x2, 1 => Float32x2, 2 => Float32x2,
            3 => Float32x2, 4 => Float32x2, 5 => Float32x2, 6 => Float32x4
        ];
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &A,
        }
    }
}

// Not public, not cached across scenes: exact present frame bytes only. All-or-none
// admission returns None before GPU calls. Caller retains legacy upload on decline.
pub(crate) fn pack_legacy(
    vertices: &[LineVertex],
    indices: &[u32],
    mut ranges: impl Iterator<Item = (usize, usize)>,
) -> Option<Vec<ExactLineQuad>> {
    let n = admitted_quads(vertices.len(), indices.len())?;
    if ranges
        .any(|(a, b)| a > b || b > indices.len() || !a.is_multiple_of(6) || !b.is_multiple_of(6))
    {
        return None;
    }
    // Validate every quad/index before reserve. No hidden partial fallback.
    for (ordinal, v) in vertices.as_chunks::<4>().0.iter().enumerate() {
        ExactLineQuad::from_legacy(v)?;
        let base = (ordinal * 4) as u32;
        if indices[ordinal * 6..ordinal * 6 + 6] != QUAD_INDICES.map(|i| i + base) {
            return None;
        }
    }
    let mut out = Vec::new();
    out.try_reserve_exact(n).ok()?;
    if out
        .capacity()
        .checked_mul(std::mem::size_of::<ExactLineQuad>())?
        > CPU_CAP_BYTES
    {
        return None;
    }
    for v in vertices.as_chunks::<4>().0 {
        out.push(ExactLineQuad::from_legacy(v)?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn original_quad(start: [f32; 2], end: [f32; 2], width: f32) -> [LineVertex; 4] {
        // Independent original emitter math, kept outside candidate packing.
        let dx = end[0] - start[0];
        let dy = end[1] - start[1];
        let len = (dx * dx + dy * dy).sqrt();
        let nx = -dy / len * width * 0.5;
        let ny = dx / len * width * 0.5;
        let c = [0.0, 0.3, 0.7, 0.5];
        [
            LineVertex::new(start[0], start[1], -nx, -ny, c),
            LineVertex::new(start[0], start[1], nx, ny, c),
            LineVertex::new(end[0], end[1], nx, ny, c),
            LineVertex::new(end[0], end[1], -nx, -ny, c),
        ]
    }
    #[test]
    fn original_horizontal_vertical_diagonal_signed_zero_bytes() {
        for (a, b) in [
            ([0.0, -0.0], [10.0, -0.0]),
            ([-0.0, 0.0], [-0.0, 20.0]),
            ([-20.25, 3.75], [1042.125, -899.5]),
        ] {
            for w in [0.125, 1.0, 8.5] {
                let v = original_quad(a, b, w);
                let p = pack_legacy(&v, &QUAD_INDICES, [(0, 6)].into_iter()).unwrap();
                assert_eq!(
                    bytemuck::cast_slice::<_, u8>(&v),
                    bytemuck::cast_slice::<_, u8>(&p[0].expand())
                );
            }
        }
        assert_eq!(std::mem::size_of::<ExactLineQuad>(), 64);
    }
    #[test]
    fn exact_source_order_and_disjoint_ranges() {
        let a = original_quad([1.0, 2.0], [7.0, 8.0], 2.0);
        let b = original_quad([10.0, 12.0], [70.0, 80.0], 0.5);
        let v: Vec<_> = a.into_iter().chain(b).collect();
        let i: Vec<_> = QUAD_INDICES
            .into_iter()
            .chain(QUAD_INDICES.map(|i| i + 4))
            .collect();
        let p = pack_legacy(&v, &i, [(0, 6), (6, 12)].into_iter()).unwrap();
        let expanded: Vec<_> = p.into_iter().flat_map(ExactLineQuad::expand).collect();
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&v),
            bytemuck::cast_slice::<_, u8>(&expanded)
        );
        assert!(pack_legacy(&v, &i, [(1, 6)].into_iter()).is_none());
    }
    #[test]
    fn arbitrary_anchor_transforms_and_invalid_layout_whole_decline() {
        let mut v = original_quad([1.0, 2.0], [7.0, 8.0], 2.0);
        // Dynamic screen-ray anchor rewrites can break opposite offset equality.
        v[2].position = v[0].position;
        assert!(pack_legacy(&v, &QUAD_INDICES, std::iter::empty()).is_none());
        v = original_quad([1.0, 2.0], [7.0, 8.0], 2.0);
        v[1].color[0] = -0.0;
        assert!(pack_legacy(&v, &QUAD_INDICES, std::iter::empty()).is_none());
        v[1].color[0] = f32::NAN;
        assert!(pack_legacy(&v, &QUAD_INDICES, std::iter::empty()).is_none());
        assert!(pack_legacy(&v[..3], &QUAD_INDICES, std::iter::empty()).is_none());
    }
    #[test]
    fn index_order_foreign_base_and_empty() {
        let v = original_quad([1.0, 2.0], [7.0, 8.0], 2.0);
        assert!(pack_legacy(&v, &[0, 2, 1, 0, 2, 3], std::iter::empty()).is_none());
        assert!(pack_legacy(&v, &[4, 5, 6, 4, 6, 7], std::iter::empty()).is_none());
        assert!(pack_legacy(&[], &[], [(0, 0)].into_iter())
            .unwrap()
            .is_empty());
    }
    #[test]
    fn capacity_admission_before_allocation_and_policy() {
        let n = CPU_CAP_BYTES / std::mem::size_of::<ExactLineQuad>();
        assert_eq!(admitted_quads(n * 4, n * 6), Some(n));
        assert_eq!(admitted_quads((n + 1) * 4, (n + 1) * 6), None);
        assert_eq!(admitted_quads(usize::MAX, usize::MAX), None);
        assert!(layout_admitted(0, 0, [(0, 0)].into_iter()));
        assert!(!layout_admitted(4, 6, [(0, 7)].into_iter()));
        assert!(!layout_admitted(4, 6, [(1, 6)].into_iter()));
        assert!(!enabled(None));
        assert!(enabled(Some(std::ffi::OsStr::new("1"))));
        for s in ["0", "true", " 1", "1 ", "invalid"] {
            assert!(!enabled(Some(std::ffi::OsStr::new(s))));
        }
    }
    #[cfg(unix)]
    #[test]
    fn nonunicode_policy_denies() {
        use std::os::unix::ffi::OsStrExt;
        assert!(!enabled(Some(std::ffi::OsStr::from_bytes(&[0xff]))));
    }
    #[test]
    fn every_original_corner_and_range_maps_to_same_triangle_order() {
        // Independent model of instance vertex fetch, matching WGSL selection only.
        let a = original_quad([1.0, -0.0], [7.0, 8.0], 2.0);
        let b = original_quad([-7.0, 18.0], [70.0, -80.0], 1.5);
        let v: Vec<_> = a.into_iter().chain(b).collect();
        let i: Vec<_> = QUAD_INDICES
            .into_iter()
            .chain(QUAD_INDICES.map(|i| i + 4))
            .collect();
        let p = pack_legacy(&v, &i, [(0, 6), (6, 12)].into_iter()).unwrap();
        for (ordinal, q) in p.iter().enumerate() {
            for (triangle_corner, corner) in QUAD_INDICES.into_iter().enumerate() {
                let position = if corner >= 2 { q.end } else { q.start };
                let offset = match corner {
                    0 => q.offset0,
                    1 => q.offset1,
                    2 => q.offset2,
                    _ => q.offset3,
                };
                let fetched =
                    LineVertex::new(position[0], position[1], offset[0], offset[1], q.color);
                let legacy = v[i[ordinal * 6 + triangle_corner] as usize];
                assert_eq!(bytemuck::bytes_of(&fetched), bytemuck::bytes_of(&legacy));
            }
            let begin = ordinal * 6;
            assert_eq!((begin / 6)..((begin + 6) / 6), ordinal..ordinal + 1);
        }
        assert!(p.capacity() * std::mem::size_of::<ExactLineQuad>() <= CPU_CAP_BYTES);
    }
    #[test]
    fn captured_screen_ray_anchor_preserves_all_four_distinct_offsets() {
        // Actual untimed Day18 pose199 quad81774: original CPU input, not GPU readback.
        let bits: [[u32; 8]; 4] = [
            [
                0x44ab5d07, 0x43a6c5b4, 0x42ff4e1c, 0x42b36281, 0x00000000, 0x00000000, 0x00000000,
                0x3f800000,
            ],
            [
                0x44ab5d07, 0x43a6c5b4, 0x42f00e64, 0x42a7788f, 0x00000000, 0x00000000, 0x00000000,
                0x3f800000,
            ],
            [
                0x44ab5d07, 0x43a6c5b4, 0x42dc9234, 0x42c06917, 0x00000000, 0x00000000, 0x00000000,
                0x3f800000,
            ],
            [
                0x44ab5d07, 0x43a6c5b4, 0x42ebd1ec, 0x42cc5309, 0x00000000, 0x00000000, 0x00000000,
                0x3f800000,
            ],
        ];
        let vertices = bits.map(|b| {
            LineVertex::new(
                f32::from_bits(b[0]),
                f32::from_bits(b[1]),
                f32::from_bits(b[2]),
                f32::from_bits(b[3]),
                [
                    f32::from_bits(b[4]),
                    f32::from_bits(b[5]),
                    f32::from_bits(b[6]),
                    f32::from_bits(b[7]),
                ],
            )
        });
        assert_ne!(
            vertices[0].offset.map(f32::to_bits),
            vertices[3].offset.map(f32::to_bits)
        );
        assert_ne!(
            vertices[1].offset.map(f32::to_bits),
            vertices[2].offset.map(f32::to_bits)
        );
        let packed = pack_legacy(&vertices, &QUAD_INDICES, [(0, 6)].into_iter()).unwrap();
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&vertices),
            bytemuck::cast_slice::<_, u8>(&packed[0].expand())
        );
    }
}
