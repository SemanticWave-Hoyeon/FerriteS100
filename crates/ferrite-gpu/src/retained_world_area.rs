//! Experimental solid-area world input retention. Default OFF.
//! Original CPU vertices remain the independent shadow and picking/audit input.
//! This V1 is a GPU-path qualification candidate, not removal of CPU tessellation.
use crate::Vertex2D;
use bytemuck::{Pod, Zeroable};
use ferrite_render::{FlatProjection, FlatTransform};

const CAP: usize = 32 * 1024 * 1024;
const FIXED_CHARGE: usize = 4096;
// Two application-owned CPU inputs, one GPU input and a GPU output. Renderer
// keeps the prior output during replacement; charge that separately below.
const BYTES_PER_VERTEX: usize = 32 + 32 + 32 + 24;
const MODEL_ERROR_PX: f64 = 0.125;
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct WorldVertex {
    coordinates: [f32; 4],
    color: [f32; 4],
}
// Complete absolute f64 source bits stay in both CPU ownership vectors.
// Only the GPU upload uses localized hi/lo coordinates; hit equality cannot
// collapse distinct original source values after localization/rounding.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct SourceVertex {
    coordinates: [f64; 2],
    color: [f32; 4],
}
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Camera {
    origin: [f32; 4],
    coefficients: [f32; 4],
    count: [u32; 4],
}
#[derive(Default, Clone, Copy)]
pub(crate) struct Statistics {
    pub frames: u64,
    pub admissions: u64,
    pub declines: u64,
    pub input_hits: u64,
    pub input_uploads: u64,
    pub dispatches: u64,
    pub charged_owned_bytes: usize,
    pub peak_charged_owned_bytes: usize,
    pub vertices: usize,
}
impl Statistics {
    pub(crate) fn audit_value(self) -> serde_json::Value {
        serde_json::json!({
            "frames": self.frames,
            "admissions": self.admissions,
            "declines": self.declines,
            "input_hits": self.input_hits,
            "input_uploads": self.input_uploads,
            "dispatches": self.dispatches,
            "charged_owned_bytes": self.charged_owned_bytes,
            "peak_charged_owned_bytes": self.peak_charged_owned_bytes,
            "vertices": self.vertices,
        })
    }
}

struct Resources {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
}
struct Slot {
    _input: wgpu::Buffer,
    group: wgpu::BindGroup,
}
pub(crate) struct RetainedWorldAreas {
    enabled: bool,
    valid: bool,
    active: bool,
    epoch: Option<u64>,
    transform: Option<FlatTransform>,
    frame: Vec<SourceVertex>,
    retained: Vec<SourceVertex>,
    anchor: Option<[f64; 2]>,
    resources: Option<Resources>,
    slot: Option<Slot>,
    output_bytes: usize,
    statistics: Statistics,
}
fn policy(value: Option<&std::ffi::OsStr>) -> bool {
    value.and_then(|v| v.to_str()) == Some("1")
}
fn split(value: f64) -> Option<[f32; 2]> {
    let hi = value as f32;
    let lo = (value - hi as f64) as f32;
    (value.is_finite() && hi.is_finite() && lo.is_finite()).then_some([hi, lo])
}
#[cfg(test)]
fn camera(transform: FlatTransform, count: usize) -> Option<Camera> {
    camera_with_anchor(transform, count, [0., 0.])
}
fn camera_with_anchor(transform: FlatTransform, count: usize, anchor: [f64; 2]) -> Option<Camera> {
    if transform.projection != FlatProjection::EllipsoidalMercator {
        return None;
    }
    let x = split(transform.geographic_origin[0] - anchor[0])?;
    let y = split(
        transform
            .projection
            .project_y(transform.geographic_origin[1])
            - anchor[1],
    )?;
    let c = [
        transform.scale[0] as f32,
        transform.scale[1] as f32,
        transform.offset[0] as f32,
        transform.offset[1] as f32,
    ];
    if !c.iter().all(|v| v.is_finite()) || c[0] <= 0. || c[1] <= 0. {
        return None;
    }
    Some(Camera {
        origin: [x[0], x[1], y[0], y[1]],
        coefficients: c,
        count: [u32::try_from(count).ok()?, 0, 0, 0],
    })
}
// Same explicitly ordered operations as the WGSL model. GPU arithmetic still
// requires actual independent readback qualification, especially contraction.
fn model(vertex: &WorldVertex, c: Camera) -> [f32; 2] {
    let dx = (vertex.coordinates[0] - c.origin[0]) + (vertex.coordinates[1] - c.origin[1]);
    let dy = (c.origin[2] - vertex.coordinates[2]) + (c.origin[3] - vertex.coordinates[3]);
    [
        dx * c.coefficients[0] + c.coefficients[2],
        dy * c.coefficients[1] + c.coefficients[3],
    ]
}
fn shadow_matches(frame: &[WorldVertex], c: Camera, shadow: &[Vertex2D]) -> bool {
    if frame.is_empty() || frame.len() != shadow.len() {
        return false;
    }
    frame.iter().zip(shadow).all(|(v, old)| {
        let xy = model(v, c);
        v.color.map(f32::to_bits) == old.color.map(f32::to_bits)
            && (0..2).all(|axis| {
                xy[axis].is_finite()
                    && old.position[axis].is_finite()
                    && (xy[axis] as f64 - old.position[axis] as f64).abs() <= MODEL_ERROR_PX
            })
    })
}
fn localized(v: &SourceVertex, anchor: [f64; 2]) -> Option<WorldVertex> {
    let x = split(v.coordinates[0] - anchor[0])?;
    let y = split(v.coordinates[1] - anchor[1])?;
    Some(WorldVertex {
        coordinates: [x[0], x[1], y[0], y[1]],
        color: v.color,
    })
}
// Actual failed Metal readback matched this reassociated/FMA form for every
// component (142238/142238). Check it as well as the ordered CPU model; this is
// still an empirical backend guard, not all permitted WGSL arithmetic proof.
fn reconstructed_model(v: &WorldVertex, c: Camera) -> [f32; 2] {
    let dx = (v.coordinates[0] + v.coordinates[1]) - (c.origin[0] + c.origin[1]);
    let dy = (c.origin[2] + c.origin[3]) - (v.coordinates[2] + v.coordinates[3]);
    [
        dx.mul_add(c.coefficients[0], c.coefficients[2]),
        dy.mul_add(c.coefficients[1], c.coefficients[3]),
    ]
}
fn source_shadow_matches(
    frame: &[SourceVertex],
    anchor: [f64; 2],
    c: Camera,
    shadow: &[Vertex2D],
) -> bool {
    !frame.is_empty()
        && frame.len() == shadow.len()
        && frame.iter().zip(shadow).all(|(source, old)| {
            let Some(v) = localized(source, anchor) else {
                return false;
            };
            shadow_matches(std::slice::from_ref(&v), c, std::slice::from_ref(old))
                && reconstructed_model(&v, c)
                    .iter()
                    .zip(old.position)
                    .all(|(&got, expected)| {
                        got.is_finite() && (got as f64 - expected as f64).abs() <= MODEL_ERROR_PX
                    })
        })
}
fn charge(count: usize, previous_output: usize) -> Option<usize> {
    count
        .checked_mul(BYTES_PER_VERTEX)?
        .checked_add(previous_output)?
        .checked_add(FIXED_CHARGE)
        .filter(|n| *n <= CAP)
}
fn same_payload(a: &[SourceVertex], b: &[SourceVertex]) -> bool {
    // Exact complete bits, including color and directed vertex order. No hash
    // collision or pointer/count assumption. Current CPU indices remain fresh.
    bytemuck::cast_slice::<_, u8>(a) == bytemuck::cast_slice::<_, u8>(b)
}
impl RetainedWorldAreas {
    /// Copy only the already sampled policy, not any published arena or GPU resource.
    pub(crate) fn fork_cold(&self) -> Self {
        Self::new(Some(std::ffi::OsStr::new(if self.enabled {
            "1"
        } else {
            "0"
        })))
    }

    pub(crate) fn new(value: Option<&std::ffi::OsStr>) -> Self {
        Self {
            enabled: policy(value),
            valid: true,
            active: false,
            epoch: None,
            transform: None,
            frame: Vec::new(),
            retained: Vec::new(),
            anchor: None,
            resources: None,
            slot: None,
            output_bytes: 0,
            statistics: Statistics::default(),
        }
    }
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }
    pub(crate) fn reject_source(&mut self) {
        if self.enabled {
            self.valid = false;
        }
    }
    /// Hidden audit only; exact production inputs, no shader model duplication.
    pub(crate) fn audit_projection_inputs(&self) -> (Vec<u8>, Option<[u8; 48]>) {
        let Some(anchor) = self.anchor else {
            return (Vec::new(), None);
        };
        let uniform = self
            .transform
            .and_then(|t| camera_with_anchor(t, self.frame.len(), anchor))
            .and_then(|c| bytemuck::bytes_of(&c).try_into().ok());
        let mut bytes = Vec::with_capacity(self.frame.len() * 32);
        for source in &self.frame {
            if let Some(v) = localized(source, anchor) {
                bytes.extend_from_slice(bytemuck::bytes_of(&v));
            }
        }
        (bytes, uniform)
    }
    pub(crate) fn statistics(&self) -> Statistics {
        self.statistics
    }
    pub(crate) fn bind_epoch(&mut self, epoch: u64) {
        if !self.enabled || self.epoch == Some(epoch) {
            return;
        }
        self.epoch = Some(epoch);
        self.anchor = None;
        self.retained = Vec::new();
        self.slot = None;
        self.active = false;
    }
    pub(crate) fn begin_frame(&mut self) {
        if !self.enabled {
            return;
        }
        self.frame.clear();
        self.transform = None;
        self.valid = true;
        self.active = false;
    }
    pub(crate) fn capture(
        &mut self,
        world: &[f64],
        color: [f32; 4],
        t: FlatTransform,
        start: usize,
    ) {
        if !self.enabled || !self.valid {
            return;
        }
        // A dependency trial may have rolled back the original CPU prefix.
        if start <= self.frame.len() {
            self.frame.truncate(start);
        }
        if start != self.frame.len()
            || !world.len().is_multiple_of(2)
            || t.projection != FlatProjection::EllipsoidalMercator
            || self.transform.is_some_and(|old| old != t)
            || !color.iter().all(|v| v.is_finite())
        {
            self.valid = false;
            return;
        }
        let Some(total) = start.checked_add(world.len() / 2) else {
            self.valid = false;
            return;
        };
        if charge(total, self.output_bytes).is_none() {
            self.valid = false;
            return;
        }
        let extra = total - self.frame.len();
        if self.frame.try_reserve_exact(extra).is_err()
            || charge(self.frame.capacity(), self.output_bytes).is_none()
        {
            self.frame = Vec::new();
            self.valid = false;
            return;
        }
        self.transform = Some(t);
        for pair in world.as_chunks::<2>().0 {
            if !pair.iter().all(|v| v.is_finite()) {
                self.valid = false;
                return;
            }
            if self.anchor.is_none() {
                self.anchor = Some([pair[0], pair[1]]);
            }
            self.frame.push(SourceVertex {
                coordinates: [pair[0], pair[1]],
                color,
            });
        }
    }

    fn decline(&mut self) -> Option<wgpu::Buffer> {
        self.anchor = None;
        self.active = false;
        self.slot = None;
        self.retained = Vec::new();
        self.statistics.declines = self.statistics.declines.saturating_add(1);
        None
    }
    /// Return a new output only on a cold/replaced slot. A hit leaves the renderer
    /// owning the existing output buffer. BindGroup owns its GPU reference.
    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        shadow: &[Vertex2D],
        identity_fast_view: bool,
    ) -> Option<wgpu::Buffer> {
        if !self.enabled {
            return None;
        }
        self.statistics.frames = self.statistics.frames.saturating_add(1);
        self.active = false;
        if self.frame.len() > shadow.len() {
            self.frame.truncate(shadow.len());
        }
        let count = self.frame.len();
        let Some(anchor) = self.anchor else {
            return self.decline();
        };
        let Some(c) = self
            .transform
            .and_then(|t| camera_with_anchor(t, count, anchor))
        else {
            return self.decline();
        };
        let Some(bytes) = charge(self.frame.capacity().max(count), self.output_bytes) else {
            return self.decline();
        };
        let limits = device.limits();
        if !self.valid
            || !identity_fast_view
            || count == 0
            || count != shadow.len()
            || (count as u64) * 32 > u64::from(limits.max_storage_buffer_binding_size)
            || (count as u64) * 24 > u64::from(limits.max_storage_buffer_binding_size)
            || (count as u64) * 32 > limits.max_buffer_size
            || count.div_ceil(64) > limits.max_compute_workgroups_per_dimension as usize
        {
            return self.decline();
        }
        // Bounded-error model gate is per vertex against independently generated
        // legacy f64->f32 output. It does not certify WGSL or permit old masks.
        if !source_shadow_matches(&self.frame, anchor, c, shadow) {
            return self.decline();
        }
        if self.resources.is_none() {
            self.resources = Some(Resources::new(device));
        }
        let resources = self.resources.as_ref().unwrap();
        queue.write_buffer(&resources.uniform, 0, bytemuck::bytes_of(&c));
        let hit = self.slot.is_some() && same_payload(&self.frame, &self.retained);
        let output = if hit {
            self.statistics.input_hits = self.statistics.input_hits.saturating_add(1);
            None
        } else {
            self.slot = None;
            self.retained = Vec::new();
            // Input identity is owned full bits, not a source hash or old pointer.
            if self.retained.try_reserve_exact(count).is_err()
                || charge(
                    self.retained.capacity().max(self.frame.capacity()),
                    self.output_bytes,
                )
                .is_none()
            {
                return self.decline();
            }
            self.retained.extend_from_slice(&self.frame);
            // Fill mapped GPU input directly: no third CPU vertex vector. Source
            // and retained vectors remain32bytes/vertex, preserving original charge.
            let input = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("retained-world-area-input-localized"),
                size: count as u64 * 32,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: true,
            });
            {
                let mut mapped = input.slice(..).get_mapped_range_mut();
                for (index, source) in self.retained.iter().enumerate() {
                    let local = localized(source, anchor)
                        .expect("Source localization already passed complete shadow admission");
                    mapped[index * 32..(index + 1) * 32]
                        .copy_from_slice(bytemuck::bytes_of(&local));
                }
            }
            input.unmap();
            let output = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("retained-world-area-output"),
                size: (count as u64) * 24,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::VERTEX
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("retained-world-area-projection"),
                layout: &resources.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: input.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: output.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: resources.uniform.as_entire_binding(),
                    },
                ],
            });
            self.slot = Some(Slot {
                _input: input,
                group,
            });
            self.output_bytes = count * 24;
            self.statistics.input_uploads = self.statistics.input_uploads.saturating_add(1);
            Some(output)
        };
        self.statistics.admissions = self.statistics.admissions.saturating_add(1);
        self.statistics.vertices = count;
        self.statistics.charged_owned_bytes = bytes;
        self.statistics.peak_charged_owned_bytes =
            self.statistics.peak_charged_owned_bytes.max(bytes);
        self.active = true;
        output
    }
    pub(crate) fn active(&self) -> bool {
        self.active
    }
    pub(crate) fn encode(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        query: Option<&wgpu_profiler::GpuProfilerQuery>,
    ) {
        if self.encode_owned_resources(encoder, query) {
            self.statistics.dispatches = self.statistics.dispatches.saturating_add(1);
        }
    }
    // Shared actual compute encoder. Caller must hold the resource owner; no live buffer is accepted as input.
    pub(crate) fn encode_owned_resources(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        query: Option<&wgpu_profiler::GpuProfilerQuery>,
    ) -> bool {
        if !self.active {
            return false;
        }
        let (Some(resources), Some(slot)) = (&self.resources, &self.slot) else {
            return false;
        };
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("retained-world-area-camera"),
            timestamp_writes: query.and_then(|q| q.compute_pass_timestamp_writes()),
        });
        pass.set_pipeline(&resources.pipeline);
        pass.set_bind_group(0, &slot.group, &[]);
        pass.dispatch_workgroups((self.statistics.vertices as u32).div_ceil(64), 1, 1);
        true
    }
}
impl Resources {
    fn new(device: &wgpu::Device) -> Self {
        let storage = |binding, read_only| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("retained-world-area-layout"),
            entries: &[
                storage(0, true),
                storage(1, false),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("retained-world-area-camera"),
            size: 48,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("retained-world-area-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("retained-world-area-compute-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("retained-world-area-compute"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("project"),
            compilation_options: Default::default(),
            cache: None,
        });
        Self {
            pipeline,
            layout,
            uniform,
        }
    }
}
const SHADER: &str = r#"
struct World { coordinates:vec4<f32>, color:vec4<f32> }
struct Camera { origin:vec4<f32>, coefficients:vec4<f32>, count:vec4<u32> }
@group(0) @binding(0) var<storage,read> input:array<World>;
@group(0) @binding(1) var<storage,read_write> output:array<u32>;
@group(0) @binding(2) var<uniform> camera:Camera;
@compute @workgroup_size(64)
fn project(@builtin(global_invocation_id) id:vec3<u32>) {
    if id.x>=camera.count.x {return;}
    let v=input[id.x];
    let dx=(v.coordinates.x-camera.origin.x)+(v.coordinates.y-camera.origin.y);
    let dy=(camera.origin.z-v.coordinates.z)+(camera.origin.w-v.coordinates.w);
    let x=dx*camera.coefficients.x+camera.coefficients.z;
    let y=dy*camera.coefficients.y+camera.coefficients.w;
    let j=id.x*6u;
    output[j]=bitcast<u32>(x);output[j+1u]=bitcast<u32>(y);
    output[j+2u]=bitcast<u32>(v.color.x);output[j+3u]=bitcast<u32>(v.color.y);
    output[j+4u]=bitcast<u32>(v.color.z);output[j+5u]=bitcast<u32>(v.color.w);
}
"#;
#[cfg(test)]
mod tests {
    use super::*;
    fn transform() -> FlatTransform {
        FlatTransform {
            projection: FlatProjection::EllipsoidalMercator,
            scale: [1234., 1234.],
            offset: [123., 80.],
            geographic_origin: [-5., 51.],
        }
    }
    #[test]
    fn default_off_exact_flag_and_layout() {
        assert!(!policy(None));
        assert!(policy(Some("1".as_ref())));
        for v in ["0", "true", "01", ""] {
            assert!(!policy(Some(v.as_ref())));
        }
        assert_eq!(std::mem::size_of::<WorldVertex>(), 32);
        assert_eq!(std::mem::size_of::<Camera>(), 48);
        assert_eq!(std::mem::size_of::<Vertex2D>(), 24);
    }
    #[test]
    fn capture_order_bits_color_and_epoch_invalidation() {
        let mut r = RetainedWorldAreas::new(Some("1".as_ref()));
        r.bind_epoch(1);
        r.begin_frame();
        r.capture(&[1., 2., 3., 4.], [1., 0., 0., 1.], transform(), 0);
        assert_eq!(r.frame.len(), 2);
        let old = r.frame.clone();
        assert!(same_payload(&old, &r.frame));
        r.frame.swap(0, 1);
        assert!(!same_payload(&old, &r.frame));
        r.frame = old.clone();
        r.frame[0].color[1] = -0.;
        assert!(!same_payload(&old, &r.frame));
        r.retained = old;
        r.bind_epoch(2);
        assert!(r.retained.is_empty());
    }
    #[test]
    fn malformed_projection_missing_prefix_and_trial_rollback_decline_or_truncate() {
        let mut r = RetainedWorldAreas::new(Some("1".as_ref()));
        r.begin_frame();
        r.capture(&[1., 2., 3., 4.], [1.; 4], transform(), 0);
        r.capture(&[5., 6.], [1.; 4], transform(), 1);
        assert_eq!(r.frame.len(), 2); // dependency rollback retains the current authoritative prefix
        r.capture(&[7., 8.], [1.; 4], transform(), 4);
        assert!(!r.valid);
        r.begin_frame();
        let mut t = transform();
        t.projection = FlatProjection::LocalGeographic;
        r.capture(&[1., 2.], [1.; 4], t, 0);
        assert!(!r.valid);
        r.begin_frame();
        r.capture(&[f64::NAN, 2.], [1.; 4], transform(), 0);
        assert!(!r.valid);
    }
    #[test]
    fn checked_peak_admission_and_overflow() {
        assert!(charge((CAP - FIXED_CHARGE) / BYTES_PER_VERTEX, 0).is_some());
        assert!(charge((CAP - FIXED_CHARGE) / BYTES_PER_VERTEX + 1, 0).is_none());
        assert!(charge(usize::MAX, 0).is_none());
        assert!(charge(1, usize::MAX).is_none());
    }
    #[test]
    fn independent_legacy_f64_projection_vs_split_model_changes_with_camera() {
        for zoom in [1., 20., 200.] {
            for pan in [-0.01, 0., 0.01] {
                let mut t = transform();
                t.scale = [zoom * 1234., zoom * 1234.];
                t.geographic_origin[0] += pan;
                let c = camera(t, 1).unwrap();
                let x = -4.95;
                let y = t.projection.project_y(51.05);
                let a = split(x).unwrap();
                let b = split(y).unwrap();
                let v = WorldVertex {
                    coordinates: [a[0], a[1], b[0], b[1]],
                    color: [1.; 4],
                };
                let original = [
                    ((x - t.geographic_origin[0]) * t.scale[0] + t.offset[0]) as f32,
                    ((t.projection.project_y(t.geographic_origin[1]) - y) * t.scale[1]
                        + t.offset[1]) as f32,
                ];
                let got = model(&v, c);
                assert!(
                    (0..2).all(|i| (got[i] as f64 - original[i] as f64).abs() <= MODEL_ERROR_PX)
                );
            }
        }
    }
    #[test]
    fn unsupported_camera_and_unrepresentable_uniform_decline_without_gpu() {
        let mut t = transform();
        t.projection = FlatProjection::LocalGeographic;
        assert!(camera(t, 1).is_none());
        t = transform();
        t.scale[0] = f64::MAX;
        assert!(camera(t, 1).is_none());
        t = transform();
        t.geographic_origin[1] = 90.;
        assert!(camera(t, 1).is_none());
        assert!(split(f64::INFINITY).is_none());
    }
    #[test]
    fn shadow_gate_checks_every_vertex_color_and_camera_not_just_count() {
        let t = transform();
        let c = camera(t, 2).unwrap();
        let x = split(-4.95).unwrap();
        let y = split(t.projection.project_y(51.05)).unwrap();
        let v = WorldVertex {
            coordinates: [x[0], x[1], y[0], y[1]],
            color: [1., 0., 0., 1.],
        };
        let xy = model(&v, c);
        let mut shadow = vec![Vertex2D::new(xy[0], xy[1], v.color); 2];
        assert!(shadow_matches(&[v, v], c, &shadow));
        shadow[1].position[0] += 0.25;
        assert!(!shadow_matches(&[v, v], c, &shadow));
        shadow[1].position = xy;
        shadow[1].color[1] = -0.;
        assert!(!shadow_matches(&[v, v], c, &shadow));
        assert!(!shadow_matches(&[v, v], c, &shadow[..1]));
        shadow[1].color = v.color;
        assert!(shadow_matches(&[v, v], c, &shadow));
        let mut changed = t;
        changed.offset[0] += 1.;
        assert!(!shadow_matches(
            &[v, v],
            camera(changed, 2).unwrap(),
            &shadow[..]
        ));
        assert!(!shadow_matches(&[], c, &[]));
    }
}

// Exact CPU projection memoization is separate from the GPU error model. It
// removes repeated same-camera projections, never certifies a different camera.
const SHADOW_CAP: usize = 16 * 1024 * 1024;
const SHADOW_ENTRIES: usize = 1024;
const SHADOW_BUCKET_CHARGE: usize = 256;
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ShadowKey {
    owner: usize,
    projection: FlatProjection,
    camera: [u64; 6],
    color: [u32; 4],
}
struct ShadowEntry<T> {
    owner: std::sync::Arc<T>,
    vertices: std::sync::Arc<Vec<Vertex2D>>,
}
#[derive(Default, Clone, Copy)]
pub(crate) struct ShadowStatistics {
    pub hits: u64,
    pub misses: u64,
    pub declines: u64,
    pub resets: u64,
    pub retained_charge: usize,
    pub peak_retained_charge: usize,
    pub vertices_projected: u64,
    pub vertices_reused: u64,
}
impl ShadowStatistics {
    pub(crate) fn audit_value(self) -> serde_json::Value {
        serde_json::json!({
            "hits": self.hits,
            "misses": self.misses,
            "declines": self.declines,
            "resets": self.resets,
            "retained_charge": self.retained_charge,
            "peak_retained_charge": self.peak_retained_charge,
            "vertices_projected": self.vertices_projected,
            "vertices_reused": self.vertices_reused,
        })
    }
}

pub(crate) struct ExactAreaShadowCache<T> {
    enabled: bool,
    epoch: Option<u64>,
    entries: rustc_hash::FxHashMap<ShadowKey, ShadowEntry<T>>,
    payload_charge: usize,
    statistics: ShadowStatistics,
}
fn shadow_key<T>(
    owner: &std::sync::Arc<T>,
    t: FlatTransform,
    color: [f32; 4],
) -> Option<ShadowKey> {
    let camera = [
        t.scale[0],
        t.scale[1],
        t.offset[0],
        t.offset[1],
        t.geographic_origin[0],
        t.geographic_origin[1],
    ];
    if !camera.iter().all(|x| x.is_finite()) || !color.iter().all(|x| x.is_finite()) {
        return None;
    }
    Some(ShadowKey {
        owner: std::sync::Arc::as_ptr(owner) as usize,
        projection: t.projection,
        camera: camera.map(f64::to_bits),
        color: color.map(f32::to_bits),
    })
}
impl<T> ExactAreaShadowCache<T> {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            epoch: None,
            entries: rustc_hash::FxHashMap::default(),
            payload_charge: 0,
            statistics: ShadowStatistics::default(),
        }
    }
    pub(crate) fn statistics(&self) -> ShadowStatistics {
        self.statistics
    }
    fn clear(&mut self) {
        self.entries = rustc_hash::FxHashMap::default();
        self.payload_charge = 0;
        self.statistics.retained_charge = 0;
        self.statistics.resets = self.statistics.resets.saturating_add(1);
    }
    pub(crate) fn bind_epoch(&mut self, epoch: u64) {
        if self.enabled && self.epoch != Some(epoch) {
            self.clear();
            self.epoch = Some(epoch);
        }
    }
    pub(crate) fn reset(&mut self) {
        self.clear();
        self.epoch = None;
    }
    /// Caller passes coordinates belonging to this owned immutable triangulation.
    /// The private renderer callsite obtains BOTH from the same cache entry.
    pub(crate) fn project(
        &mut self,
        owner: &std::sync::Arc<T>,
        world: &[f64],
        owned_source_bytes: usize,
        t: FlatTransform,
        color: [f32; 4],
    ) -> Option<std::sync::Arc<Vec<Vertex2D>>> {
        if !self.enabled || self.epoch.is_none() {
            return None;
        }
        let key = shadow_key(owner, t, color)?;
        if let Some(entry) = self.entries.get(&key) {
            debug_assert!(std::sync::Arc::ptr_eq(&entry.owner, owner));
            self.statistics.hits = self.statistics.hits.saturating_add(1);
            self.statistics.vertices_reused = self
                .statistics
                .vertices_reused
                .saturating_add(entry.vertices.len() as u64);
            return Some(std::sync::Arc::clone(&entry.vertices));
        }
        self.statistics.misses = self.statistics.misses.saturating_add(1);
        if world.is_empty() || !world.len().is_multiple_of(2) {
            return None;
        }
        let n = world.len() / 2;
        let charge = n
            .checked_mul(std::mem::size_of::<Vertex2D>())
            .and_then(|b| b.checked_add(owned_source_bytes))
            .and_then(|b| b.checked_add(128))?;
        if charge > SHADOW_CAP.saturating_sub(SHADOW_BUCKET_CHARGE * 4) {
            self.statistics.declines = self.statistics.declines.saturating_add(1);
            return None;
        }
        // Drop old ownership before allocating a replacement. The original
        // triangulation and renderer vertices are outside this RETENTION cap.
        if self.entries.len() >= SHADOW_ENTRIES
            || self
                .payload_charge
                .saturating_add(charge)
                .saturating_add((self.entries.capacity() + 1) * SHADOW_BUCKET_CHARGE)
                > SHADOW_CAP
        {
            self.clear();
        }
        if self.entries.try_reserve(1).is_err() {
            return None;
        }
        let mut vertices = Vec::new();
        if vertices.try_reserve_exact(n).is_err() {
            return None;
        }
        let actual_charge = vertices
            .capacity()
            .checked_mul(std::mem::size_of::<Vertex2D>())
            .and_then(|b| b.checked_add(owned_source_bytes))
            .and_then(|b| b.checked_add(128))?;
        let total = self
            .payload_charge
            .checked_add(actual_charge)?
            .checked_add(self.entries.capacity().checked_mul(SHADOW_BUCKET_CHARGE)?)?;
        if total > SHADOW_CAP {
            self.statistics.declines = self.statistics.declines.saturating_add(1);
            return None;
        }
        // Identical operation order to add_area_cached's independent legacy loop.
        let [scale_x, scale_y] = t.scale;
        let [offset_x, offset_y] = t.offset;
        let [min_x, max_lat] = t.geographic_origin;
        let max_y = t.projection.project_y(max_lat);
        for p in world.as_chunks::<2>().0 {
            let sx = ((p[0] - min_x) * scale_x + offset_x) as f32;
            let sy = ((max_y - p[1]) * scale_y + offset_y) as f32;
            if !sx.is_finite() || !sy.is_finite() {
                return None;
            }
            vertices.push(Vertex2D::new(sx, sy, color));
        }
        self.statistics.vertices_projected =
            self.statistics.vertices_projected.saturating_add(n as u64);
        let vertices = std::sync::Arc::new(vertices);
        self.entries.insert(
            key,
            ShadowEntry {
                owner: std::sync::Arc::clone(owner),
                vertices: std::sync::Arc::clone(&vertices),
            },
        );
        self.payload_charge += actual_charge;
        self.statistics.retained_charge = total;
        self.statistics.peak_retained_charge = self.statistics.peak_retained_charge.max(total);
        Some(vertices)
    }
}

#[cfg(test)]
mod exact_shadow_tests {
    use super::*;
    fn t() -> FlatTransform {
        FlatTransform {
            projection: FlatProjection::LocalGeographic,
            scale: [12., 20.],
            offset: [3., 5.],
            geographic_origin: [1., 10.],
        }
    }
    fn bits(v: &[Vertex2D]) -> Vec<[u32; 6]> {
        v.iter()
            .map(|x| {
                [
                    x.position[0].to_bits(),
                    x.position[1].to_bits(),
                    x.color[0].to_bits(),
                    x.color[1].to_bits(),
                    x.color[2].to_bits(),
                    x.color[3].to_bits(),
                ]
            })
            .collect()
    }
    #[test]
    fn exact_camera_owner_color_bits_hit_and_legacy_formula() {
        let owner = std::sync::Arc::new(());
        let world = [1., 2., 3., 4.];
        let color = [1., 0.5, 0., 1.];
        let transform = t();
        let mut cache = ExactAreaShadowCache::new(true);
        cache.bind_epoch(7);
        let first = cache.project(&owner, &world, 0, transform, color).unwrap();
        let independent: Vec<_> = (0..2)
            .map(|i| {
                Vertex2D::new(
                    ((world[i * 2] - 1.) * 12. + 3.) as f32,
                    ((10. - world[i * 2 + 1]) * 20. + 5.) as f32,
                    color,
                )
            })
            .collect();
        assert_eq!(bits(&first), bits(&independent));
        let again = cache.project(&owner, &world, 0, transform, color).unwrap();
        assert!(std::sync::Arc::ptr_eq(&first, &again));
        let mut moved = transform;
        moved.offset[0] += 1.;
        let changed = cache.project(&owner, &world, 0, moved, color).unwrap();
        assert_ne!(bits(&first), bits(&changed));
        let mut recolor = color;
        recolor[2] = -0.;
        let recolored = cache
            .project(&owner, &world, 0, transform, recolor)
            .unwrap();
        assert_ne!(bits(&first), bits(&recolored));
        assert_eq!(cache.statistics().hits, 1);
    }
    #[test]
    fn epoch_and_distinct_owned_source_never_alias() {
        let a = std::sync::Arc::new(());
        let b = std::sync::Arc::new(());
        let mut c = ExactAreaShadowCache::new(true);
        c.bind_epoch(1);
        let x = c.project(&a, &[1., 2.], 0, t(), [1.; 4]).unwrap();
        let y = c.project(&b, &[1., 2.], 0, t(), [1.; 4]).unwrap();
        assert!(!std::sync::Arc::ptr_eq(&x, &y));
        c.bind_epoch(2);
        let z = c.project(&a, &[1., 2.], 0, t(), [1.; 4]).unwrap();
        assert!(!std::sync::Arc::ptr_eq(&x, &z));
        assert_eq!(c.statistics().hits, 0);
    }
    #[test]
    fn cap_and_malformed_decline_leave_original_fallback_available() {
        let owner = std::sync::Arc::new(());
        let mut c = ExactAreaShadowCache::new(true);
        c.bind_epoch(1);
        assert!(c
            .project(&owner, &[1., 2.], SHADOW_CAP, t(), [1.; 4])
            .is_none());
        assert!(c.project(&owner, &[1.], 0, t(), [1.; 4]).is_none());
        assert!(c
            .project(&owner, &[f64::NAN, 2.], 0, t(), [1.; 4])
            .is_none());
        assert!(c.project(&owner, &[1., 2.], 0, t(), [1.; 4]).is_some());
        assert!(c.statistics().peak_retained_charge <= SHADOW_CAP);
    }
    #[test]
    fn unset_disabled_has_no_retained_keys_or_projection_work() {
        let mut c = ExactAreaShadowCache::new(false);
        c.bind_epoch(1);
        assert!(c
            .project(&std::sync::Arc::new(()), &[1., 2.], 0, t(), [1.; 4])
            .is_none());
        assert!(c.entries.is_empty());
        assert_eq!(c.statistics().vertices_projected, 0);
    }
    #[test]
    fn ownership_is_retained_until_epoch_reset_and_source_removal() {
        let owner = std::sync::Arc::new(17_u64);
        let weak = std::sync::Arc::downgrade(&owner);
        let mut c = ExactAreaShadowCache::new(true);
        c.bind_epoch(1);
        let old = c.project(&owner, &[1., 2.], 8, t(), [1.; 4]).unwrap();
        drop(owner);
        assert!(weak.upgrade().is_some());
        c.bind_epoch(2);
        assert!(weak.upgrade().is_none());
        let replacement = std::sync::Arc::new(17_u64);
        let next = c.project(&replacement, &[1., 2.], 8, t(), [1.; 4]).unwrap();
        assert!(!std::sync::Arc::ptr_eq(&old, &next));
        assert_eq!(c.statistics().hits, 0);
    }
    #[test]
    fn entry_limit_and_palette_dpi_projection_mutations_are_exact_misses() {
        let mut c = ExactAreaShadowCache::new(true);
        c.bind_epoch(1);
        for i in 0..=SHADOW_ENTRIES {
            let owner = std::sync::Arc::new(i);
            assert!(c.project(&owner, &[1., 2.], 8, t(), [1.; 4]).is_some());
            assert!(c.entries.len() <= SHADOW_ENTRIES);
            assert!(c.statistics().retained_charge <= SHADOW_CAP);
        }
        let owner = std::sync::Arc::new(9999_usize);
        for mut transform in [t(), t(), t()] {
            transform.scale[0] *= 2.; // actual pixel scale change, not profile name.
            let x = c.project(&owner, &[1., 2.], 8, transform, [1.; 4]).unwrap();
            assert_eq!(bits(&x)[0][0], 3_f32.to_bits());
        }
        let mut projected = t();
        projected.projection = FlatProjection::EllipsoidalMercator;
        let merc = c.project(&owner, &[1., 2.], 8, projected, [1.; 4]).unwrap();
        let exact_y = ((projected.projection.project_y(10.) - 2.) * 20. + 5.) as f32;
        assert_eq!(merc[0].position[1].to_bits(), exact_y.to_bits());
    }
}

#[cfg(test)]
mod local_anchor_tests {
    use super::*;
    #[test]
    fn complete_original_source_bits_do_not_collapse_after_localization() {
        assert_eq!(std::mem::size_of::<SourceVertex>(), 32);
        let a = SourceVertex {
            coordinates: [1., 2.],
            color: [1.; 4],
        };
        let mut b = a;
        b.coordinates[0] = f64::from_bits(1f64.to_bits() + 1);
        assert!(!same_payload(&[a], &[b]));
        b = a;
        b.color[0] = -0.;
        let mut zero = a;
        zero.color[0] = 0.;
        assert!(!same_payload(&[zero], &[b]));
    }
    #[test]
    fn epoch_anchor_stable_for_camera_changes_and_reset_for_new_source() {
        let mut r = RetainedWorldAreas::new(Some("1".as_ref()));
        r.bind_epoch(1);
        r.begin_frame();
        let t = FlatTransform {
            projection: FlatProjection::EllipsoidalMercator,
            scale: [102031.93693178773; 2],
            offset: [256.83513711566707, 49.],
            geographic_origin: [-2.1039915, 48.67738920070776],
        };
        let y = t.projection.project_y(48.65);
        r.capture(&[-2.2, y], [1.; 4], t, 0);
        let anchor = r.anchor;
        r.begin_frame();
        let mut p = t;
        p.geographic_origin[0] += 0.01;
        r.capture(&[-2.2, y], [1.; 4], p, 0);
        assert_eq!(r.anchor, anchor);
        r.bind_epoch(2);
        assert!(r.anchor.is_none());
    }
    #[test]
    fn observed_reassociated_backend_model_remains_inside_unchanged_bound_when_local() {
        let t = FlatTransform {
            projection: FlatProjection::EllipsoidalMercator,
            scale: [102031.93693178773; 2],
            offset: [256.83513711566707, 49.],
            geographic_origin: [-2.1039915, 48.67738920070776],
        };
        let anchor = [-2.2, t.projection.project_y(48.65)];
        for dx in [-0.1, -0.01, 0., 0.01, 0.1] {
            let source = SourceVertex {
                coordinates: [-2.103 + dx, t.projection.project_y(48.675 + dx)],
                color: [0.15, 0.4, 0.75, 0.8],
            };
            let expected = Vertex2D::new(
                ((source.coordinates[0] - t.geographic_origin[0]) * t.scale[0] + t.offset[0])
                    as f32,
                ((t.projection.project_y(t.geographic_origin[1]) - source.coordinates[1])
                    * t.scale[1]
                    + t.offset[1]) as f32,
                source.color,
            );
            assert!(source_shadow_matches(
                &[source],
                anchor,
                camera_with_anchor(t, 1, anchor).unwrap(),
                &[expected]
            ));
        }
    }
}
