//! Private native route GPU publication, explicitly host editing (not XSLT portrayal).
use crate::{
    GpuState, LineVertex, NativeRouteOverlayResources, PreparedNativeRouteOverlay, RenderPipelines,
    TextureVertex, ViewUniforms,
};
use ferrite_render::{Scaler, ScreenPoint};
use std::{ops::Range, sync::Arc};
use wgpu::util::DeviceExt;
const MAX_QUADS: usize = 65_536;
const MAX_PAYLOAD: usize = 32 * 1024 * 1024;

/// Renderer creates once and replaces on device/pipeline recreation. External callers cannot mint.
pub struct NativeRouteGpuOwner {
    _private: (),
}
impl NativeRouteGpuOwner {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self { _private: () })
    }
}
struct NativeRouteTexture {
    _texture: wgpu::Texture,
    binding: wgpu::BindGroup,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeRoutePickId {
    Waypoint { route_id: u32, waypoint_id: u32 },
    Leg { route_id: u32, from: u32, to: u32 },
}
#[derive(Clone, Debug)]
pub struct NativeRouteVisibleRange {
    pub id: NativeRoutePickId,
    pub indices: Range<u32>,
    pub bounds: [f32; 4],
}
struct CpuGeometry {
    line_vertices: Vec<LineVertex>,
    line_indices: Vec<u32>,
    symbol_vertices: Vec<TextureVertex>,
    symbol_indices: Vec<u32>,
    line_ranges: Vec<NativeRouteVisibleRange>,
    symbol_ranges: Vec<NativeRouteVisibleRange>,
}
/// Fully private buffers, texture and view uniforms; never writes current chart allocations.
pub struct PreparedNativeRouteGpuPublication {
    owner: Arc<NativeRouteGpuOwner>,
    packet: Arc<PreparedNativeRouteOverlay>,
    texture: Arc<NativeRouteTexture>,
    _uniform: wgpu::Buffer,
    device: wgpu::Device,
    view_layout: wgpu::BindGroupLayout,
    texture_layout: wgpu::BindGroupLayout,
    line_pipeline: wgpu::RenderPipeline,
    texture_pipeline: wgpu::RenderPipeline,
    format: wgpu::TextureFormat,
    uniform_values: ViewUniforms,
    view_binding: wgpu::BindGroup,
    line_vb: Option<wgpu::Buffer>,
    line_ib: Option<wgpu::Buffer>,
    symbol_vb: Option<wgpu::Buffer>,
    symbol_ib: Option<wgpu::Buffer>,
    geometry: CpuGeometry,
    full_surface: [u32; 2],
    scissor: [u32; 4],
}
impl PreparedNativeRouteGpuPublication {
    pub(crate) fn prepare(
        state: &GpuState,
        pipelines: &RenderPipelines,
        owner: Arc<NativeRouteGpuOwner>,
        packet: Arc<PreparedNativeRouteOverlay>,
        previous: Option<&Self>,
        scaler: &Scaler,
    ) -> Result<Self, String> {
        packet.validate(
            packet.resource_owner(),
            packet.route_revision(),
            scaler,
            packet.pixels_per_mm(),
        )?;
        let (w, h) = state.viewport_size();
        let full_surface = surface_size(w, h)?;
        let scissor = scissor(scaler, full_surface)?;
        let geometry = prepare_geometry(&packet, scaler)?;
        let symbol = packet.resources().waypoint_symbol();
        let texture_bytes = symbol.pixels.len();
        let bytes = geometry_payload(&geometry)?
            .checked_add(texture_bytes)
            .and_then(|n| n.checked_add(std::mem::size_of::<ViewUniforms>()))
            .ok_or("Native overlay payload overflow")?;
        if bytes > MAX_PAYLOAD {
            return Err("Native overlay retained logical payload exceeds 32 MiB".into());
        }
        let limits = state.device.limits();
        if symbol.width == 0
            || symbol.height == 0
            || symbol.width > limits.max_texture_dimension_2d
            || symbol.height > limits.max_texture_dimension_2d
        {
            return Err("Native waypoint texture exceeds device capability".into());
        }
        for size in [
            geometry.line_vertices.len() * std::mem::size_of::<LineVertex>(),
            geometry.line_indices.len() * 4,
            geometry.symbol_vertices.len() * std::mem::size_of::<TextureVertex>(),
            geometry.symbol_indices.len() * 4,
        ] {
            if size as u64 > limits.max_buffer_size {
                return Err("Native overlay buffer exceeds device capability".into());
            }
        }
        // Immutable texture/binding reuse only under identical renderer owner and PC object.
        let texture = previous
            .filter(|p| {
                p.gpu_identity_matches(state, pipelines)
                    && Arc::ptr_eq(&p.owner, &owner)
                    && Arc::ptr_eq(p.packet.resource_owner(), packet.resource_owner())
            })
            .map(|p| p.texture.clone())
            .unwrap_or_else(|| {
                let (texture, view) = state.create_texture_from_rgba(
                    &symbol.pixels,
                    symbol.width,
                    symbol.height,
                    "native:S421:RTEWPT01",
                );
                let binding = pipelines.create_texture_bind_group(&state.device, &view);
                Arc::new(NativeRouteTexture {
                    _texture: texture,
                    binding,
                })
            });
        // Already-projected vertices: always identity pan/zoom, independently of live ENC preview.
        let uniform_values = ViewUniforms::new(w, h, 1.);
        let uniform = state
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("native:S421:view"),
                contents: bytemuck::bytes_of(&uniform_values),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_SRC,
            });
        let view_binding = pipelines.create_view_bind_group(&state.device, &uniform);
        let buffer = |label, bytes: &[u8], usage| {
            if bytes.is_empty() {
                None
            } else {
                Some(
                    state
                        .device
                        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: Some(label),
                            contents: bytes,
                            usage: usage | wgpu::BufferUsages::COPY_SRC,
                        }),
                )
            }
        };
        let line_vb = buffer(
            "native:S421:line-VB",
            bytemuck::cast_slice(&geometry.line_vertices),
            wgpu::BufferUsages::VERTEX,
        );
        let line_ib = buffer(
            "native:S421:line-IB",
            bytemuck::cast_slice(&geometry.line_indices),
            wgpu::BufferUsages::INDEX,
        );
        let symbol_vb = buffer(
            "native:S421:waypoint-VB",
            bytemuck::cast_slice(&geometry.symbol_vertices),
            wgpu::BufferUsages::VERTEX,
        );
        let symbol_ib = buffer(
            "native:S421:waypoint-IB",
            bytemuck::cast_slice(&geometry.symbol_indices),
            wgpu::BufferUsages::INDEX,
        );
        Ok(Self {
            owner,
            packet,
            texture,
            _uniform: uniform,
            device: state.device.clone(),
            view_layout: pipelines.view_bind_group_layout.clone(),
            texture_layout: pipelines.texture_bind_group_layout.clone(),
            line_pipeline: pipelines.line_pipeline.clone(),
            texture_pipeline: pipelines.texture_pipeline.clone(),
            format: state.format(),
            uniform_values,
            view_binding,
            line_vb,
            line_ib,
            symbol_vb,
            symbol_ib,
            geometry,
            full_surface,
            scissor,
        })
    }
    pub(crate) fn gpu_identity_matches(
        &self,
        state: &GpuState,
        pipelines: &RenderPipelines,
    ) -> bool {
        self.device == state.device
            && self.view_layout == pipelines.view_bind_group_layout
            && self.texture_layout == pipelines.texture_bind_group_layout
            && self.line_pipeline == pipelines.line_pipeline
            && self.texture_pipeline == pipelines.texture_pipeline
            && self.format == state.format()
    }
    pub(crate) fn frame_matches(
        &self,
        camera: Option<[u64; 16]>,
        surface: [u32; 2],
        scissor: Option<[u32; 4]>,
    ) -> bool {
        camera == Some(self.packet.camera_identity())
            && surface == self.full_surface
            && scissor == Some(self.scissor)
    }
    pub fn validate(
        &self,
        owner: &Arc<NativeRouteGpuOwner>,
        resources: &Arc<NativeRouteOverlayResources>,
        revision: u64,
        scaler: &Scaler,
        pixels_per_mm: f64,
        surface: [u32; 2],
    ) -> Result<(), String> {
        if !Arc::ptr_eq(owner, &self.owner)
            || surface != self.full_surface
            || scissor(scaler, surface)? != self.scissor
        {
            return Err("Stale native overlay renderer/surface/scissor".into());
        }
        self.packet
            .validate(resources, revision, scaler, pixels_per_mm)
    }
    pub fn packet(&self) -> &Arc<PreparedNativeRouteOverlay> {
        &self.packet
    }
    pub fn visible_waypoints(&self) -> &[NativeRouteVisibleRange] {
        &self.geometry.symbol_ranges
    }
    pub fn visible_legs(&self) -> &[NativeRouteVisibleRange] {
        &self.geometry.line_ranges
    }
    /// Separate native namespace: conservative displayed-billboard bounds candidate, NOT alpha-aware GPU picking.
    pub fn waypoint_bounds_candidate(&self, p: ScreenPoint) -> Option<NativeRoutePickId> {
        if !p.x.is_finite()
            || !p.y.is_finite()
            || p.x < self.scissor[0] as f32
            || p.y < self.scissor[1] as f32
            || p.x >= (self.scissor[0] + self.scissor[2]) as f32
            || p.y >= (self.scissor[1] + self.scissor[3]) as f32
        {
            return None;
        }
        self.geometry
            .symbol_ranges
            .iter()
            .rev()
            .find(|r| {
                p.x >= r.bounds[0] && p.x <= r.bounds[2] && p.y >= r.bounds[1] && p.y <= r.bounds[3]
            })
            .map(|r| r.id)
    }
    /// Called inside the chart's existing MSAA render pass AFTER chart/raster, BEFORE egui.
    /// Caller validates immediately before this draw; same pipeline/device owner is mandatory.
    pub(crate) fn draw<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        pipelines: &'a RenderPipelines,
    ) {
        pass.set_scissor_rect(
            self.scissor[0],
            self.scissor[1],
            self.scissor[2],
            self.scissor[3],
        );
        pass.set_bind_group(0, &self.view_binding, &[]);
        if let (Some(vb), Some(ib)) = (&self.line_vb, &self.line_ib) {
            pass.set_pipeline(&pipelines.line_pipeline);
            pass.set_vertex_buffer(0, vb.slice(..));
            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.geometry.line_indices.len() as u32, 0, 0..1);
        }
        if let (Some(vb), Some(ib)) = (&self.symbol_vb, &self.symbol_ib) {
            pass.set_pipeline(&pipelines.texture_pipeline);
            pass.set_bind_group(1, &self.texture.binding, &[]);
            pass.set_vertex_buffer(0, vb.slice(..));
            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.geometry.symbol_indices.len() as u32, 0, 0..1);
        }
    }
    /// Bounded explicit diagnostic; never called by the production frame loop.
    /// The renderer enforces hidden/unfocused mode before permitting the wait.
    pub(crate) fn audit_gpu_buffers(
        &self,
        state: &GpuState,
        dir: &std::path::Path,
    ) -> Result<(), String> {
        self.audit_cpu_inputs(dir).map_err(|e| e.to_string())?;
        let buffers: [(&str, Option<&wgpu::Buffer>, &[u8]); 5] = [
            (
                "native-line.vertices",
                self.line_vb.as_ref(),
                bytemuck::cast_slice(&self.geometry.line_vertices),
            ),
            (
                "native-line.indices",
                self.line_ib.as_ref(),
                bytemuck::cast_slice(&self.geometry.line_indices),
            ),
            (
                "native-symbol.vertices",
                self.symbol_vb.as_ref(),
                bytemuck::cast_slice(&self.geometry.symbol_vertices),
            ),
            (
                "native-symbol.indices",
                self.symbol_ib.as_ref(),
                bytemuck::cast_slice(&self.geometry.symbol_indices),
            ),
            (
                "native-view.uniform",
                Some(&self._uniform),
                bytemuck::bytes_of(&self.uniform_values),
            ),
        ];
        let mut rows = Vec::new();
        for (name, source, expected) in buffers {
            if expected.is_empty() {
                if source.is_some() {
                    return Err("Unexpected empty native buffer".into());
                }
                std::fs::write(dir.join(format!("{name}.gpu")), []).map_err(|e| e.to_string())?;
                rows.push(serde_json::json!({"name":name,"bytes":0,"exact":true}));
                continue;
            }
            let source = source.ok_or("Missing native GPU buffer")?;
            let size = expected.len() as u64;
            if size > MAX_PAYLOAD as u64 || !size.is_multiple_of(4) {
                return Err("Native audit buffer admission failed".into());
            }
            let staging = state.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("native:S421:readback"),
                size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder =
                state
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("native:S421:readback-copy"),
                    });
            encoder.copy_buffer_to_buffer(source, 0, &staging, 0, size);
            state.queue.submit(Some(encoder.finish()));
            let slice = staging.slice(..);
            let (tx, rx) = std::sync::mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
            state.device.poll(wgpu::Maintain::Wait);
            rx.recv()
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            let actual = slice.get_mapped_range();
            std::fs::write(dir.join(format!("{name}.gpu")), &actual).map_err(|e| e.to_string())?;
            let exact = actual.as_ref() == expected;
            rows.push(serde_json::json!({"name":name,"bytes":size,"exact":exact}));
            drop(actual);
            staging.unmap();
            if !exact {
                return Err(format!("Native GPU upload mismatch: {name}"));
            }
        }
        let value = serde_json::json!({"scope":"resident native VB/IB/uniform exact readback; texture and full portrayal not qualified by this check", "buffers":rows});
        std::fs::write(
            dir.join("gpu-readback.json"),
            serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
    }
    /// CPU upload inputs only. This is NOT resident GPU prefix readback.
    pub fn audit_cpu_inputs(&self, dir: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        for (name, bytes) in [
            (
                "native-line.vertices.cpu",
                bytemuck::cast_slice(&self.geometry.line_vertices),
            ),
            (
                "native-line.indices.cpu",
                bytemuck::cast_slice(&self.geometry.line_indices),
            ),
            (
                "native-symbol.vertices.cpu",
                bytemuck::cast_slice(&self.geometry.symbol_vertices),
            ),
            (
                "native-symbol.indices.cpu",
                bytemuck::cast_slice(&self.geometry.symbol_indices),
            ),
            (
                "native-waypoint.rgba.cpu",
                self.packet.resources().waypoint_symbol().pixels.as_slice(),
            ),
        ] {
            std::fs::write(dir.join(name), bytes)?;
        }
        std::fs::write(
            dir.join("native-view.uniform.cpu"),
            bytemuck::bytes_of(&self.uniform_values),
        )?;
        let symbol = self.packet.resources().waypoint_symbol();
        let meta = serde_json::json!({"scope":"native host editing CPU upload inputs; no GPU readback/XSLT proof","provider":"native:s421-route","source_digest":self.packet.resources().source_digest(),"route_revision":self.packet.route_revision(),"camera":self.packet.camera_identity(),"pixels_per_mm_bits":self.packet.pixels_per_mm().to_bits(),"palette":self.packet.resources().palette(),"texture_size":[symbol.width,symbol.height],"render_scale_bits":symbol.render_scale.to_bits(),"texture_pivot_bits":[symbol.texture_pivot.0.to_bits(),symbol.texture_pivot.1.to_bits()],"line_indices":self.geometry.line_indices.len(),"symbol_indices":self.geometry.symbol_indices.len(),"surface":self.full_surface,"scissor":self.scissor});
        std::fs::write(
            dir.join("native-overlay.json"),
            serde_json::to_vec_pretty(&meta)?,
        )
    }
}
fn surface_size(w: f32, h: f32) -> Result<[u32; 2], String> {
    if !w.is_finite()
        || !h.is_finite()
        || w <= 0.
        || h <= 0.
        || w > u32::MAX as f32
        || h > u32::MAX as f32
    {
        return Err("Invalid native surface size".into());
    }
    Ok([w as u32, h as u32])
}
fn scissor(s: &Scaler, size: [u32; 2]) -> Result<[u32; 4], String> {
    let v = &s.viewport;
    if ![v.x, v.y, v.width, v.height].iter().all(|n| n.is_finite())
        || v.width <= 0.
        || v.height <= 0.
    {
        return Err("Invalid native viewport".into());
    }
    let x = v.x.max(0.).floor();
    let y = v.y.max(0.).floor();
    let r = (v.x + v.width).ceil().min(size[0] as f32);
    let b = (v.y + v.height).ceil().min(size[1] as f32);
    if ![x, y, r, b].iter().all(|n| n.is_finite()) || r <= x || b <= y {
        return Err("Native viewport has no drawable surface intersection".into());
    }
    Ok([x as u32, y as u32, (r - x) as u32, (b - y) as u32])
}
fn geometry_payload(g: &CpuGeometry) -> Result<usize, String> {
    g.line_vertices
        .len()
        .checked_mul(std::mem::size_of::<LineVertex>())
        .and_then(|n| n.checked_add(g.line_indices.len() * 4))
        .and_then(|n| n.checked_add(g.symbol_vertices.len() * std::mem::size_of::<TextureVertex>()))
        .and_then(|n| n.checked_add(g.symbol_indices.len() * 4))
        .and_then(|n| {
            n.checked_add(g.line_ranges.capacity() * std::mem::size_of::<NativeRouteVisibleRange>())
        })
        .and_then(|n| {
            n.checked_add(
                g.symbol_ranges.capacity() * std::mem::size_of::<NativeRouteVisibleRange>(),
            )
        })
        .ok_or_else(|| "Native geometry payload overflow".into())
}
fn prepare_geometry(
    packet: &PreparedNativeRouteOverlay,
    scaler: &Scaler,
) -> Result<CpuGeometry, String> {
    let mut g = CpuGeometry {
        line_vertices: Vec::new(),
        line_indices: Vec::new(),
        symbol_vertices: Vec::new(),
        symbol_indices: Vec::new(),
        line_ranges: Vec::new(),
        symbol_ranges: Vec::new(),
    };
    let style = packet.resources().leg_style();
    if style.pen.cap_style != ferrite_portrayal_catalog::CapStyle::Butt {
        return Err("Native host overlay supports authored Butt cap only".into());
    }
    let ppm = packet.pixels_per_mm();
    let width = (style.pen.width * ppm) as f32;
    if !width.is_finite() || width <= 0. {
        return Err("Invalid native physical line width".into());
    }
    let v = &scaler.viewport;
    let clip = [
        v.x - width,
        v.y - width,
        v.x + v.width + width,
        v.y + v.height + width,
    ];
    if !clip.iter().all(|x| x.is_finite()) {
        return Err("Native clipping bounds overflow".into());
    }
    let rgba = packet.resources().leg_rgba();
    let color = rgba.map(|c| c as f32 / 255.);
    let line_style = ferrite_render::LineStyle {
        dash_cycle: style.dash_cycle().map_err(str::to_owned)?,
        ..Default::default()
    };
    for path in packet.paths() {
        let projected: Vec<_> = if path.line_points.is_empty() {
            // Original undeclared straight illustration, without extra retained copies.
            path.waypoints
                .iter()
                .map(|(_, s)| [s.x as f64, s.y as f64])
                .collect()
        } else {
            path.line_points
                .iter()
                .map(|s| [s.x as f64, s.y as f64])
                .collect()
        };
        let mut visible = Vec::new();
        for (i, p) in projected.windows(2).enumerate() {
            let dx = p[1][0] as f32 - p[0][0] as f32;
            let dy = p[1][1] as f32 - p[0][1] as f32;
            if !dx.is_finite() || !dy.is_finite() {
                return Err("Native clip delta overflow".into());
            }
            if let Some((a, b, c, d)) = crate::WgpuRenderer::clip_line_segment(
                p[0][0] as f32,
                p[0][1] as f32,
                p[1][0] as f32,
                p[1][1] as f32,
                clip[0],
                clip[1],
                clip[2],
                clip[3],
            ) {
                let (start, end) = if dx.abs() >= dy.abs() && dx != 0. {
                    ((a - p[0][0] as f32) / dx, (c - p[0][0] as f32) / dx)
                } else if dy != 0. {
                    ((b - p[0][1] as f32) / dy, (d - p[0][1] as f32) / dy)
                } else {
                    continue;
                };
                let start = f64::from(start).clamp(0., 1.);
                let end = f64::from(end).clamp(0., 1.);
                if !start.is_finite() || !end.is_finite() {
                    return Err("Native clip fraction overflow".into());
                }
                if end > start {
                    visible.push(ferrite_render::LineSpan {
                        segment: i,
                        start,
                        end,
                    });
                }
            }
        }
        let dashed = ferrite_render::dash_projected_line_spans_clipped(
            &projected,
            ppm,
            &line_style,
            &visible,
            0.,
        )?;
        let spans = dashed.as_deref().unwrap_or(&visible);
        for span in spans {
            let a = projected[span.segment];
            let b = projected[span.segment + 1];
            let start = ScreenPoint::new(
                (a[0] + (b[0] - a[0]) * span.start) as f32,
                (a[1] + (b[1] - a[1]) * span.start) as f32,
            );
            let end = ScreenPoint::new(
                (a[0] + (b[0] - a[0]) * span.end) as f32,
                (a[1] + (b[1] - a[1]) * span.end) as f32,
            );
            push_line(
                &mut g,
                start,
                end,
                width,
                color,
                clip,
                NativeRoutePickId::Leg {
                    route_id: path.route_id,
                    from: if path.segment_owners.is_empty() {
                        path.waypoints[span.segment].0
                    } else {
                        path.segment_owners[span.segment].0
                    },
                    to: if path.segment_owners.is_empty() {
                        path.waypoints[span.segment + 1].0
                    } else {
                        path.segment_owners[span.segment].1
                    },
                },
            )?;
        }
        for (id, point) in &path.waypoints {
            push_symbol(
                &mut g,
                *point,
                packet.resources().waypoint_symbol(),
                ppm,
                [v.x, v.y, v.x + v.width, v.y + v.height],
                NativeRoutePickId::Waypoint {
                    route_id: path.route_id,
                    waypoint_id: *id,
                },
            )?;
        }
    }
    Ok(g)
}
fn quad_budget(g: &CpuGeometry) -> Result<(), String> {
    if g.line_indices.len() / 6 + g.symbol_indices.len() / 6 >= MAX_QUADS {
        return Err("Native visible quad count exceeds 65536".into());
    }
    Ok(())
}
fn push_line(
    g: &mut CpuGeometry,
    a: ScreenPoint,
    b: ScreenPoint,
    width: f32,
    color: [f32; 4],
    clip: [f32; 4],
    id: NativeRoutePickId,
) -> Result<(), String> {
    let Some((x0, y0, x1, y1)) = crate::WgpuRenderer::clip_line_segment(
        a.x, a.y, b.x, b.y, clip[0], clip[1], clip[2], clip[3],
    ) else {
        return Ok(());
    };
    let dx = x1 - x0;
    let dy = y1 - y0;
    let length = (dx * dx + dy * dy).sqrt();
    if !length.is_finite() {
        return Err("Native line length overflow".into());
    }
    if length < 0.001 {
        return Ok(());
    };
    quad_budget(g)?;
    let nx = -dy / length * width * 0.5;
    let ny = dx / length * width * 0.5;
    let base = g.line_vertices.len() as u32;
    let first = g.line_indices.len() as u32;
    g.line_vertices.extend([
        LineVertex::new(x0, y0, -nx, -ny, color),
        LineVertex::new(x0, y0, nx, ny, color),
        LineVertex::new(x1, y1, nx, ny, color),
        LineVertex::new(x1, y1, -nx, -ny, color),
    ]);
    g.line_indices
        .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    g.line_ranges.push(NativeRouteVisibleRange {
        id,
        indices: first..first + 6,
        bounds: [
            x0.min(x1) - width / 2.,
            y0.min(y1) - width / 2.,
            x0.max(x1) + width / 2.,
            y0.max(y1) + width / 2.,
        ],
    });
    Ok(())
}
fn push_symbol(
    g: &mut CpuGeometry,
    p: ScreenPoint,
    symbol: &crate::SymbolGeometry,
    ppm: f64,
    viewport: [f32; 4],
    id: NativeRoutePickId,
) -> Result<(), String> {
    // usvg dimensions are 96-DPI pixels; existing raster scale multiplies those dimensions.
    let scale = (ppm / (96. / 25.4) / f64::from(symbol.render_scale)) as f32;
    let (xp, yp) = symbol.pivot_in_texture();
    let x = p.x - xp * scale;
    let y = p.y - yp * scale;
    let r = x + symbol.width as f32 * scale;
    let b = y + symbol.height as f32 * scale;
    if ![x, y, r, b, scale].iter().all(|x| x.is_finite()) || scale <= 0. {
        return Err("Invalid native symbol geometry/calibration".into());
    }
    if r < viewport[0] || b < viewport[1] || x > viewport[2] || y > viewport[3] {
        return Ok(());
    }
    quad_budget(g)?;
    let base = g.symbol_vertices.len() as u32;
    let first = g.symbol_indices.len() as u32;
    let anchor = [p.x, p.y];
    g.symbol_vertices.extend([
        TextureVertex::new(x, y, 0., 0., anchor),
        TextureVertex::new(r, y, 1., 0., anchor),
        TextureVertex::new(r, b, 1., 1., anchor),
        TextureVertex::new(x, b, 0., 1., anchor),
    ]);
    g.symbol_indices
        .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    g.symbol_ranges.push(NativeRouteVisibleRange {
        id,
        indices: first..first + 6,
        bounds: [x, y, r, b],
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn empty() -> CpuGeometry {
        CpuGeometry {
            line_vertices: vec![],
            line_indices: vec![],
            symbol_vertices: vec![],
            symbol_indices: vec![],
            line_ranges: vec![],
            symbol_ranges: vec![],
        }
    }
    fn id() -> NativeRoutePickId {
        NativeRoutePickId::Waypoint {
            route_id: 7,
            waypoint_id: 9,
        }
    }
    fn symbol() -> crate::SymbolGeometry {
        crate::SymbolGeometry {
            name: "RTEWPT01".into(),
            pixels: vec![255; 4 * 4 * 4],
            width: 4,
            height: 4,
            bounds: (0., 0., 2., 2.),
            pivot: (1., 1.),
            texture_pivot: (2., 2.),
            render_scale: 2.,
        }
    }
    #[test]
    fn natural_size_and_pivot_use_physical_calibration() {
        let mut g = empty();
        let s = symbol();
        let base = 96. / 25.4;
        push_symbol(
            &mut g,
            ScreenPoint::new(10., 20.),
            &s,
            base,
            [0., 0., 100., 100.],
            id(),
        )
        .unwrap();
        assert_eq!(g.symbol_ranges[0].bounds, [9., 19., 11., 21.]);
        let mut h = empty();
        push_symbol(
            &mut h,
            ScreenPoint::new(10., 20.),
            &s,
            base * 2.,
            [0., 0., 100., 100.],
            id(),
        )
        .unwrap();
        assert_eq!(h.symbol_ranges[0].bounds, [8., 18., 12., 22.]);
        assert_eq!(h.symbol_indices, [0, 1, 2, 0, 2, 3]);
    }
    #[test]
    fn clipped_quad_and_native_source_range_preserve_logical_lengths() {
        let mut g = empty();
        let id = NativeRoutePickId::Leg {
            route_id: 3,
            from: 2,
            to: 1,
        };
        push_line(
            &mut g,
            ScreenPoint::new(-20., 5.),
            ScreenPoint::new(20., 5.),
            2.,
            [1., 0., 0., 1.],
            [0., 0., 10., 10.],
            id,
        )
        .unwrap();
        assert_eq!(g.line_vertices.len(), 4);
        assert_eq!(g.line_indices, [0, 1, 2, 0, 2, 3]);
        assert_eq!(g.line_ranges[0].id, id);
        assert_eq!(g.line_ranges[0].indices, 0..6);
        assert_eq!(g.line_vertices[0].position, [0., 5.]);
        assert_eq!(g.line_vertices[2].position, [10., 5.]);
    }
    #[test]
    fn wholly_offscreen_and_zero_length_have_no_pick_or_upload() {
        let mut g = empty();
        push_symbol(
            &mut g,
            ScreenPoint::new(-100., -100.),
            &symbol(),
            96. / 25.4,
            [0., 0., 10., 10.],
            id(),
        )
        .unwrap();
        push_line(
            &mut g,
            ScreenPoint::new(1., 1.),
            ScreenPoint::new(1., 1.),
            2.,
            [1.; 4],
            [0., 0., 10., 10.],
            id(),
        )
        .unwrap();
        assert!(
            g.symbol_indices.is_empty() && g.line_indices.is_empty() && g.symbol_ranges.is_empty()
        );
    }
    #[test]
    fn overflow_and_budget_fail_explicitly() {
        let mut g = empty();
        assert!(push_symbol(
            &mut g,
            ScreenPoint::new(1., 1.),
            &symbol(),
            f64::MAX,
            [0., 0., 10., 10.],
            id()
        )
        .is_err());
        g.symbol_indices = vec![0; MAX_QUADS * 6];
        assert!(quad_budget(&g).is_err());
        assert!(surface_size(0., 10.).is_err());
    }
    #[test]
    fn authored_dash_intervals_continue_across_vertices_and_visible_clipping() {
        let style = ferrite_render::LineStyle {
            dash_cycle: Some(
                ferrite_kernel::DashCycle::new(24.2, [(2.2, 10.), (14.2, 10.)]).unwrap(),
            ),
            ..Default::default()
        };
        let p = [[0., 0.], [12., 0.], [30., 0.]];
        let whole = ferrite_render::dash_projected_line_spans(&p, 1., &style, None)
            .unwrap()
            .unwrap();
        assert!((whole[0].start - 2.2 / 12.).abs() < 1e-14);
        let visible = [ferrite_render::LineSpan {
            segment: 1,
            start: 0.,
            end: 1.,
        }];
        let clipped =
            ferrite_render::dash_projected_line_spans_clipped(&p, 1., &style, &visible, 0.)
                .unwrap()
                .unwrap();
        let second: Vec<_> = whole.into_iter().filter(|s| s.segment == 1).collect();
        assert_eq!(clipped, second);
    }
}
