//! Camera-relative, double-single GPU projection. Source coordinates remain f64
//! in the kernel. One immutable base mesh is GPU resident; changing overlays
//! upload only their source vertices. Draw order and the existing shaders stay intact.
use crate::globe_scene::{GlobeMesh, GlobeVertex};
use ferrite_kernel::globe_camera::GlobeCamera;
use std::sync::Arc;
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct SourceVertex {
    high: [f32; 3],
    font: u32,
    low: [f32; 3],
    tint: u32,
    color: [f32; 4],
}
fn split(x: f64) -> (f32, f32) {
    let high = x as f32;
    (high, (x - high as f64) as f32)
}
impl SourceVertex {
    pub(super) fn new(v: &GlobeVertex, font: Option<[u8; 4]>) -> Self {
        Self {
            high: v.ecef_m.map(|x| split(x).0),
            low: v.ecef_m.map(|x| split(x).1),
            font: u32::from(font.is_some()),
            tint: u32::from_ne_bytes(font.unwrap_or([255; 4])),
            color: if font.is_some() {
                [v.color[0], v.color[1], 1., 1.]
            } else {
                v.color
            },
        }
    }
}
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct CameraParams {
    eye_high: [f32; 4],
    eye_low: [f32; 4],
    right_high: [f32; 4],
    right_low: [f32; 4],
    up_high: [f32; 4],
    up_low: [f32; 4],
    forward_high: [f32; 4],
    forward_low: [f32; 4],
    coefficients_high: [f32; 4],
    coefficients_low: [f32; 4],
    counts: [u32; 4],
}
fn pair(v: [f64; 3]) -> ([f32; 4], [f32; 4]) {
    (
        [split(v[0]).0, split(v[1]).0, split(v[2]).0, 0.],
        [split(v[0]).1, split(v[1]).1, split(v[2]).1, 0.],
    )
}
impl CameraParams {
    pub(super) fn new(c: &GlobeCamera) -> Option<Self> {
        let p = c.reverse_depth_projection_frame();
        if p.eye_m.iter().any(|x| !x.is_finite() || x.abs() > 1e12)
            || p.divisors
                .iter()
                .any(|x| !x.is_finite() || x.abs() < 1e-12 || x.abs() > 1e12)
            || !p.depth[0].is_finite()
            || p.depth[0].abs() > 1e12
            || !p.depth[1].is_finite()
            || p.depth[1].abs() > 1e20
        {
            return None;
        }
        let (eye_high, eye_low) = pair(p.eye_m);
        let (right_high, right_low) = pair(p.right);
        let (up_high, up_low) = pair(p.up);
        let (forward_high, forward_low) = pair(p.forward);
        let k = [p.divisors[0], p.divisors[1], p.depth[0], p.depth[1]];
        Some(Self {
            eye_high,
            eye_low,
            right_high,
            right_low,
            up_high,
            up_low,
            forward_high,
            forward_low,
            coefficients_high: k.map(|x| split(x).0),
            coefficients_low: k.map(|x| split(x).1),
            counts: [0; 4],
        })
    }
}
struct ResidentBase {
    mesh: Arc<GlobeMesh>,
    buffer: wgpu::Buffer,
}
struct ResidentEntry {
    owner: Arc<GlobeMesh>,
    start: u32,
    count: u32,
    cpu_bytes: usize,
    epoch: u64,
}
struct ResidentPool {
    buffer: wgpu::Buffer,
    initialized: bool,
    capacity: u32,
    free: Vec<std::ops::Range<u32>>,
    entries: rustc_hash::FxHashMap<usize, ResidentEntry>,
    epoch: u64,
    cpu_bytes: usize,
    frame_upload_bytes: usize,
    uploads: u64,
    hits: u64,
    evictions: u64,
}
impl ResidentPool {
    fn new(device: &wgpu::Device) -> Self {
        let bytes = (32 * 1024 * 1024u64)
            .min(device.limits().max_buffer_size)
            .min(device.limits().max_storage_buffer_binding_size as u64);
        let capacity = (bytes / 48) as u32;
        Self {
            buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("empty resident chart source"),
                size: 48,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            initialized: false,
            capacity,
            free: vec![0..capacity],
            entries: Default::default(),
            epoch: 0,
            cpu_bytes: 0,
            frame_upload_bytes: 0,
            uploads: 0,
            hits: 0,
            evictions: 0,
        }
    }
    fn begin_frame(&mut self, owners: &rustc_hash::FxHashMap<usize, Arc<GlobeMesh>>) {
        self.epoch += 1;
        self.frame_upload_bytes = 0;
        for key in owners.keys() {
            if let Some(entry) = self.entries.get_mut(key) {
                entry.epoch = self.epoch;
            }
        }
    }
    fn reserve(&mut self, count: u32) -> Option<u32> {
        let slot = self.free.iter().position(|r| r.end - r.start >= count)?;
        let start = self.free[slot].start;
        self.free[slot].start += count;
        if self.free[slot].is_empty() {
            self.free.remove(slot);
        }
        Some(start)
    }
    fn release(&mut self, range: std::ops::Range<u32>) {
        self.free.push(range);
        self.free.sort_by_key(|r| r.start);
        let mut i = 1;
        while i < self.free.len() {
            if self.free[i - 1].end == self.free[i].start {
                self.free[i - 1].end = self.free[i].end;
                self.free.remove(i);
            } else {
                i += 1;
            }
        }
    }
    fn evict_unused(&mut self) -> bool {
        let Some(key) = self
            .entries
            .iter()
            .filter(|(_, e)| e.epoch != self.epoch)
            .min_by_key(|(_, e)| e.epoch)
            .map(|(k, _)| *k)
        else {
            return false;
        };
        let e = self.entries.remove(&key).unwrap();
        self.cpu_bytes -= e.cpu_bytes;
        self.release(e.start..e.start + e.count);
        self.evictions += 1;
        true
    }
    fn acquire(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        owner: &Arc<GlobeMesh>,
    ) -> Option<u32> {
        let key = Arc::as_ptr(owner) as usize;
        if let Some(e) = self.entries.get_mut(&key) {
            e.epoch = self.epoch;
            self.hits += 1;
            debug_assert!(Arc::ptr_eq(&e.owner, owner));
            return Some(e.start);
        }
        let count = u32::try_from(owner.vertices.len()).ok()?;
        if count == 0 || count > self.capacity {
            return None;
        }
        let cpu_bytes = owner.vertices.capacity() * std::mem::size_of::<GlobeVertex>()
            + owner.indices.capacity() * 4
            + std::mem::size_of::<GlobeMesh>()
            + 2 * std::mem::size_of::<usize>();
        if cpu_bytes > 64 * 1024 * 1024 {
            return None;
        }
        while self.cpu_bytes + cpu_bytes > 64 * 1024 * 1024 || self.entries.len() >= 8192 {
            if !self.evict_unused() {
                return None;
            }
        }
        let start = loop {
            if let Some(start) = self.reserve(count) {
                break start;
            }
            if !self.evict_unused() {
                return None;
            }
        };
        if !self.initialized {
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("resident chart source arena"),
                size: self.capacity as u64 * 48,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.initialized = true;
        }
        let encoded: Vec<_> = owner
            .vertices
            .iter()
            .map(|v| SourceVertex::new(v, None))
            .collect();
        queue.write_buffer(
            &self.buffer,
            start as u64 * 48,
            bytemuck::cast_slice(&encoded),
        );
        self.frame_upload_bytes += count as usize * 48;
        self.uploads += 1;
        self.cpu_bytes += cpu_bytes;
        self.entries.insert(
            key,
            ResidentEntry {
                owner: owner.clone(),
                start,
                count,
                cpu_bytes,
                epoch: self.epoch,
            },
        );
        Some(start)
    }
}

pub(super) struct GpuProjection {
    pool: ResidentPool,
    references: Option<wgpu::Buffer>,
    reference_capacity: usize,
    pipeline: wgpu::ComputePipeline,
    uniform: wgpu::Buffer,
    dynamic: Option<wgpu::Buffer>,
    dynamic_capacity: usize,
    small_dynamic_frames: u32,
    resident: Option<ResidentBase>,
    bind_group: Option<(u64, wgpu::BindGroup)>,
    allocations: u64,
    dispatches: u64,
    base_upload_bytes: usize,
    last_upload_bytes: usize,
}
impl GpuProjection {
    pub(super) fn supported(device: &wgpu::Device) -> bool {
        let l = device.limits();
        l.max_storage_buffers_per_shader_stage >= 5
            && l.max_compute_invocations_per_workgroup >= 64
            && l.max_compute_workgroup_size_x >= 64
            && l.max_compute_workgroups_per_dimension >= 16384
    }
    pub(super) fn new(device: &wgpu::Device, resident: Option<Arc<GlobeMesh>>) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("double-single camera-relative globe projection"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("GPU globe projection"),
            layout: None,
            module: &shader,
            entry_point: Some("project"),
            compilation_options: Default::default(),
            cache: None,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("GPU globe camera"),
            size: std::mem::size_of::<CameraParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut base_upload_bytes = 0;
        let resident = resident.map(|mesh| {
            let encoded: Vec<_> = mesh
                .vertices
                .iter()
                .map(|v| SourceVertex::new(v, None))
                .collect();
            base_upload_bytes = encoded.len() * 48;
            let dummy = SourceVertex::zeroed();
            let data = if encoded.is_empty() {
                bytemuck::bytes_of(&dummy)
            } else {
                bytemuck::cast_slice(&encoded)
            };
            let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("immutable globe source mesh"),
                contents: data,
                usage: wgpu::BufferUsages::STORAGE,
            });
            ResidentBase { mesh, buffer }
        });
        Self {
            pool: ResidentPool::new(device),
            references: None,
            reference_capacity: 0,
            pipeline,
            uniform,
            dynamic: None,
            dynamic_capacity: 0,
            small_dynamic_frames: 0,
            resident,
            bind_group: None,
            allocations: 0,
            dispatches: 0,
            base_upload_bytes,
            last_upload_bytes: 0,
        }
    }
    pub(super) fn matches_base(&self, mesh: &GlobeMesh) -> bool {
        self.resident
            .as_ref()
            .is_some_and(|r| std::ptr::eq(Arc::as_ptr(&r.mesh), mesh))
    }
    pub(super) fn begin_frame(&mut self, owners: &rustc_hash::FxHashMap<usize, Arc<GlobeMesh>>) {
        self.pool.begin_frame(owners);
    }
    pub(super) fn resident_area(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        owner: &Arc<GlobeMesh>,
    ) -> Option<u32> {
        let initialized = self.pool.initialized;
        let result = self.pool.acquire(device, queue, owner);
        if initialized != self.pool.initialized {
            self.bind_group = None;
        }
        result
    }
    pub(super) fn dispatch(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        output: &wgpu::Buffer,
        output_generation: u64,
        mut camera: CameraParams,
        vertices: &[SourceVertex],
        references: &[u32],
        base_count: usize,
    ) -> Result<(), String> {
        let size = (vertices.len() * 48)
            .max(48)
            .checked_next_power_of_two()
            .ok_or("GPU source size overflow")?;
        if size > 64 * 1024 * 1024
            || size as u64 > device.limits().max_storage_buffer_binding_size as u64
            || output.size() > device.limits().max_storage_buffer_binding_size as u64
        {
            return Err("GPU projection storage budget exceeded".into());
        }
        if size <= self.dynamic_capacity / 2 {
            self.small_dynamic_frames += 1;
        } else {
            self.small_dynamic_frames = 0;
        }
        let trim = self.small_dynamic_frames >= 8;
        if size > self.dynamic_capacity || trim {
            self.dynamic = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("changing globe source vertices"),
                size: size as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.dynamic_capacity = size;
            self.small_dynamic_frames = 0;
            self.allocations += 1;
            self.bind_group = None;
        }
        let dynamic = self.dynamic.as_ref().unwrap();
        if !vertices.is_empty() {
            queue.write_buffer(dynamic, 0, bytemuck::cast_slice(vertices));
        }
        let reference_size = (references.len() * 4).max(4).next_power_of_two();
        if reference_size as u64 > device.limits().max_buffer_size
            || reference_size > device.limits().max_storage_buffer_binding_size as usize
        {
            return Err("GPU reference budget exceeded".into());
        }
        if reference_size > self.reference_capacity {
            self.references = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ordered globe source references"),
                size: reference_size as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.reference_capacity = reference_size;
            self.bind_group = None;
        }
        if !references.is_empty() {
            queue.write_buffer(
                self.references.as_ref().unwrap(),
                0,
                bytemuck::cast_slice(references),
            );
        }
        camera.counts = [
            (references.len() + base_count) as u32,
            base_count as u32,
            0,
            0,
        ];
        queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&camera));
        if self.bind_group.as_ref().map(|b| b.0) != Some(output_generation) {
            let base = if base_count > 0 {
                &self
                    .resident
                    .as_ref()
                    .ok_or("Missing resident base")?
                    .buffer
            } else {
                dynamic
            };
            // Base selection can change independently of allocation. Rebuild below
            // when the caller changes which immutable mesh starts this scene.
            self.bind_group = Some((
                output_generation,
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("GPU globe sources and clips"),
                    layout: &self.pipeline.get_bind_group_layout(0),
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: base.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: dynamic.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: output.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: self.uniform.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: self.pool.buffer.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: self.references.as_ref().unwrap().as_entire_binding(),
                        },
                    ],
                }),
            ));
        }
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("project resident globe geometry"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("globe double-single projection"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group.as_ref().unwrap().1, &[]);
            pass.dispatch_workgroups(camera.counts[0].div_ceil(64), 1, 1);
        }
        queue.submit(Some(encoder.finish()));
        self.dispatches += 1;
        self.last_upload_bytes = vertices.len() * 48
            + references.len() * 4
            + self.pool.frame_upload_bytes
            + std::mem::size_of::<CameraParams>();
        Ok(())
    }
    pub(super) fn invalidate_binding(&mut self) {
        self.bind_group = None;
    }
    pub(super) fn uploaded_bytes(&self) -> usize {
        self.last_upload_bytes
    }
    pub(super) fn statistics(&self) -> serde_json::Value {
        serde_json::json!({"dispatches":self.dispatches,"dynamic_buffer_allocations":self.allocations,
            "resident_area_entries":self.pool.entries.len(),"resident_area_uploads":self.pool.uploads,"resident_area_hits":self.pool.hits,"resident_area_evictions":self.pool.evictions,"resident_area_frame_upload_bytes":self.pool.frame_upload_bytes,"resident_area_gpu_capacity_bytes":if self.pool.initialized{self.pool.capacity as usize*48}else{48},"resident_area_cpu_owner_bytes":self.pool.cpu_bytes,"reference_capacity_bytes":self.reference_capacity,"dynamic_capacity_bytes":self.dynamic_capacity,"resident_source_bytes":self.base_upload_bytes,
            "resident_source_uploads":usize::from(self.resident.is_some()),"last_upload_bytes":self.last_upload_bytes})
    }
}
use bytemuck::Zeroable;
const SHADER: &str = r#"
struct Source {high:vec3<f32>,font:u32,low:vec3<f32>,tint:u32,color:vec4<f32>};
struct Camera {
 eye_high:vec4<f32>,eye_low:vec4<f32>,right_high:vec4<f32>,right_low:vec4<f32>,
 up_high:vec4<f32>,up_low:vec4<f32>,forward_high:vec4<f32>,forward_low:vec4<f32>,
 coefficients_high:vec4<f32>,coefficients_low:vec4<f32>,counts:vec4<u32>
};
@group(0) @binding(0) var<storage,read> base_vertices:array<Source>;
@group(0) @binding(1) var<storage,read> dynamic_vertices:array<Source>;
@group(0) @binding(2) var<storage,read_write> projected:array<u32>;
@group(0) @binding(3) var<uniform> camera:Camera;
@group(0) @binding(4) var<storage,read> resident_areas:array<Source>;
@group(0) @binding(5) var<storage,read> source_references:array<u32>;
fn ds_normalize(a:f32,b:f32)->vec2<f32>{let s=a+b;let v=s-a;let e=(a-(s-v))+(b-v);return vec2<f32>(s,e);}
fn ds_add(a:vec2<f32>,b:vec2<f32>)->vec2<f32>{let s=a.x+b.x;let v=s-a.x;let e=(a.x-(s-v))+(b.x-v);return ds_normalize(s,e+a.y+b.y);}
fn ds_sub(a:vec2<f32>,b:vec2<f32>)->vec2<f32>{return ds_add(a,-b);}
fn ds_mul(a:vec2<f32>,b:vec2<f32>)->vec2<f32>{
 let ah=bitcast<f32>(bitcast<u32>(a.x)&0xfffff000u);let al=a.x-ah;
 let bh=bitcast<f32>(bitcast<u32>(b.x)&0xfffff000u);let bl=b.x-bh;
 let p=a.x*b.x;let e=((ah*bh-p)+ah*bl+al*bh)+al*bl;
 return ds_normalize(p,e+a.x*b.y+a.y*b.x+a.y*b.y);
}
fn ds_div(a:vec2<f32>,b:vec2<f32>)->vec2<f32>{let q=a.x/b.x;let r=ds_sub(a,ds_mul(b,vec2<f32>(q,0.)));return ds_normalize(q,(r.x+r.y)/b.x);}
fn ds_dot(x:vec2<f32>,y:vec2<f32>,z:vec2<f32>,high:vec4<f32>,low:vec4<f32>)->vec2<f32>{
 return ds_add(ds_add(ds_mul(x,vec2<f32>(high.x,low.x)),ds_mul(y,vec2<f32>(high.y,low.y))),ds_mul(z,vec2<f32>(high.z,low.z)));
}
@compute @workgroup_size(64) fn project(@builtin(global_invocation_id) id:vec3<u32>){
 let i=id.x;if i>=camera.counts.x{return;}
 var v:Source;
 if i<camera.counts.y{v=base_vertices[i];}else{let r=source_references[i-camera.counts.y];if (r&0x80000000u)!=0u{v=resident_areas[r&0x7fffffffu];}else{v=dynamic_vertices[r];}}
 // Keep the high subtraction separate from the residual subtraction.
 // Normalizing Earth-scale operands before subtraction loses metre-scale offsets
 // under native shader arithmetic optimizations (caught by the precision audit).
 let x=vec2<f32>(v.high.x-camera.eye_high.x,v.low.x-camera.eye_low.x);
 let y=vec2<f32>(v.high.y-camera.eye_high.y,v.low.y-camera.eye_low.y);
 let z=vec2<f32>(v.high.z-camera.eye_high.z,v.low.z-camera.eye_low.z);
 let depth=ds_dot(x,y,z,camera.forward_high,camera.forward_low);
 var cx=ds_div(ds_dot(x,y,z,camera.right_high,camera.right_low),vec2<f32>(camera.coefficients_high.x,camera.coefficients_low.x));
 var cy=ds_div(ds_dot(x,y,z,camera.up_high,camera.up_low),vec2<f32>(camera.coefficients_high.y,camera.coefficients_low.y));
 var cz=ds_sub(ds_mul(depth,vec2<f32>(camera.coefficients_high.z,camera.coefficients_low.z)),vec2<f32>(camera.coefficients_high.w,camera.coefficients_low.w));
 var cw=depth;
 if v.font!=0u{cx=ds_div(cx,depth);cy=ds_div(cy,depth);cz=ds_div(cz,depth);cw=vec2<f32>(1.,0.);}
 let n=i*9u;
 projected[n]=bitcast<u32>(cx.x+cx.y);projected[n+1u]=bitcast<u32>(cy.x+cy.y);
 projected[n+2u]=bitcast<u32>(cz.x+cz.y);projected[n+3u]=bitcast<u32>(cw.x+cw.y);
 projected[n+4u]=bitcast<u32>(v.color.x);projected[n+5u]=bitcast<u32>(v.color.y);
 projected[n+6u]=bitcast<u32>(v.color.z);projected[n+7u]=bitcast<u32>(v.color.w);projected[n+8u]=v.tint;
}
"#;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoding_keeps_centimetre_differences_at_earth_radius() {
        for x in [6378137., -6378137., 6356752.314245, 1e9] {
            let (h, l) = split(x + 0.01);
            assert!(((h as f64 + l as f64) - (x + 0.01)).abs() < 1e-6);
        }
        assert_eq!(std::mem::size_of::<SourceVertex>(), 48);
        assert_eq!(std::mem::size_of::<CameraParams>(), 176);
    }
}
