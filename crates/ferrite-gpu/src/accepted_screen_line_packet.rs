//! Diagnostic-only one-emitter capture. Never used to authorize or replace drawing.
use serde::Serialize;
use std::{cell::Cell, ffi::OsStr, io, sync::Arc, time::Instant};
const CAP: usize = 32 * 1024 * 1024 - 64 * 1024;
pub(crate) const REPLAY_POSE: usize = 199;
const CONTROL: usize = 4096;
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(crate) struct Material {
    pub ordinal: usize,
    /// 0 area hatch,1 line,2 point,3 text. Only ordinary kind1 is engine eligible.
    pub primitive_kind: u8,
    pub cell_index: Option<u32>,
    pub feature_id: Option<i64>,
    pub owner_resource_revision: u64,
    pub coverage_source: Option<usize>,
    pub plane: i32,
    pub priority: i32,
    pub ordinary_probe_eligible: bool,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct Segment {
    pub sequence: u64,
    pub vertex_base: usize,
    pub index_start: usize,
    pub path: u64,
    pub material: Material,
    // Exact emitted f32 bits, including signed zero. No path re-projection.
    pub endpoints_bits: [u32; 4],
    pub clip_rect_bits: [u32; 4],
    pub width_bits: u32,
    pub color_bits: [u32; 4],
}
#[derive(Default, Serialize)]
struct Packet {
    call: u64,
    source_geometry_revision: u64,
    view_identity: Option<[u64; 16]>,
    base_vertices: usize,
    base_indices: usize,
    final_vertices: usize,
    final_indices: usize,
    accepted_segment_count: u64,
    ordinary_segment_count: u64,
    finished: bool,
    unavailable_reason: Option<&'static str>,
    emitter_host_ns: Option<u64>,
    retained_capacity_bytes: usize,
    segments: Vec<Segment>,
}
pub(crate) struct Capture {
    enabled: bool,
    chosen: Option<u64>,
    source_identity: Option<Arc<ferrite_render::StaticInstructionOrderIdentity>>,
    exported: Cell<bool>,
    calls: u64,
    active: bool,
    clock: Option<Instant>,
    material: Material,
    path: u64,
    packet: Packet,
    cap: usize,
}
impl Capture {
    /// Copy only the already sampled policy, not any published arena or GPU resource.
    pub(crate) fn fork_cold(&self) -> Self {
        Self::new(Some(std::ffi::OsStr::new(if self.enabled {
            "1"
        } else {
            "0"
        })))
    }

    pub fn new(flag: Option<&OsStr>) -> Self {
        Self {
            enabled: flag == Some(OsStr::new("1")),
            chosen: None,
            source_identity: None,
            exported: Cell::new(false),
            calls: 0,
            active: false,
            clock: None,
            material: Material::default(),
            path: 0,
            packet: Packet::default(),
            cap: CAP,
        }
    }
    pub fn arm(&mut self, replay_pose: usize) {
        if self.enabled && replay_pose == REPLAY_POSE && self.chosen.is_none() {
            self.chosen = Some(self.calls.saturating_add(1));
        }
    }
    pub fn begin(&mut self, context: &ferrite_render::RenderContext, buffers: [usize; 2]) {
        if !self.enabled {
            return;
        }
        self.calls = self.calls.saturating_add(1);
        self.active = self.chosen == Some(self.calls);
        if !self.active {
            return;
        }
        self.clock = Some(Instant::now());
        self.packet.base_vertices = buffers[0];
        self.packet.base_indices = buffers[1];
        self.source_identity = Some(context.static_instruction_order_identity());
        self.packet.call = self.calls;
        self.packet.source_geometry_revision = context.geometry_revision();
        self.packet.view_identity = context.scaler.flat_encoded_identity();
        self.packet.unavailable_reason = self
            .packet
            .view_identity
            .is_none()
            .then_some("missing_view_identity");
    }
    pub fn retire_frame(&mut self) {
        if self.enabled && self.packet.finished && !self.exported.get() {
            self.source_identity = None;
        }
    }
    pub fn current(&self, context: &ferrite_render::RenderContext) -> bool {
        self.packet.call == self.calls
            && self.enabled
            && !self.exported.get()
            && self.packet.finished
            && self.packet.source_geometry_revision == context.geometry_revision()
            && self.packet.view_identity == context.scaler.flat_encoded_identity()
            && self
                .source_identity
                .as_ref()
                .is_some_and(|old| Arc::ptr_eq(old, &context.static_instruction_order_identity()))
    }
    pub fn active(&self) -> bool {
        self.active
    }
    pub fn material(&mut self, material: Material) {
        if self.active {
            self.material = material;
        }
    }
    pub fn path(&mut self) {
        if self.active {
            self.path = self.path.saturating_add(1);
        }
    }
    pub fn record(
        &mut self,
        endpoints: [f32; 4],
        clip: [f32; 4],
        width: f32,
        color: [f32; 4],
        buffers: [usize; 2],
    ) {
        if !self.active {
            return;
        }
        let sequence = self.packet.accepted_segment_count;
        self.packet.accepted_segment_count = sequence.saturating_add(1);
        let mut material = self.material;
        material.ordinary_probe_eligible &= material.primitive_kind == 1
            && width.is_finite()
            && width > 0.0
            && color.iter().all(|v| v.is_finite())
            && clip.iter().all(|v| v.is_finite());
        if material.ordinary_probe_eligible {
            self.packet.ordinary_segment_count =
                self.packet.ordinary_segment_count.saturating_add(1);
        }
        if self.packet.unavailable_reason.is_some() {
            return;
        }
        let expected_vertex = usize::try_from(sequence)
            .ok()
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| self.packet.base_vertices.checked_add(n));
        let expected_index = usize::try_from(sequence)
            .ok()
            .and_then(|n| n.checked_mul(6))
            .and_then(|n| self.packet.base_indices.checked_add(n));
        if expected_vertex != Some(buffers[0]) || expected_index != Some(buffers[1]) {
            self.decline("unrecorded_line_writer_or_offset_overflow");
            return;
        }
        if self.packet.segments.len() == self.packet.segments.capacity() {
            let remaining = self.cap.saturating_sub(CONTROL) / std::mem::size_of::<Segment>();
            let next = self
                .packet
                .segments
                .len()
                .saturating_add(512)
                .min(remaining);
            if next <= self.packet.segments.len() {
                self.decline("capacity_cap");
                return;
            }
            if self
                .packet
                .segments
                .try_reserve_exact(next - self.packet.segments.len())
                .is_err()
            {
                self.decline("reservation_failed");
                return;
            }
            let bytes = self
                .packet
                .segments
                .capacity()
                .checked_mul(std::mem::size_of::<Segment>())
                .and_then(|n| n.checked_add(CONTROL));
            if bytes.is_none_or(|n| n > self.cap) {
                self.decline("actual_capacity_cap");
                return;
            }
            self.packet.retained_capacity_bytes = bytes.unwrap_or(0);
        }
        self.packet.segments.push(Segment {
            sequence,
            vertex_base: buffers[0],
            index_start: buffers[1],
            path: self.path,
            material,
            endpoints_bits: endpoints.map(f32::to_bits),
            clip_rect_bits: clip.map(f32::to_bits),
            width_bits: width.to_bits(),
            color_bits: color.map(f32::to_bits),
        });
    }
    fn decline(&mut self, reason: &'static str) {
        self.packet.unavailable_reason = Some(reason);
        self.packet.segments = Vec::new(); // No misleading partial packet.
        self.packet.retained_capacity_bytes = 0;
    }
    pub fn finish(&mut self, buffers: [usize; 2]) {
        if !self.active {
            return;
        }
        self.packet.finished = true;
        self.packet.emitter_host_ns = self
            .clock
            .take()
            .map(|c| c.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        self.packet.final_vertices = buffers[0];
        self.packet.final_indices = buffers[1];
        let count = usize::try_from(self.packet.accepted_segment_count).ok();
        let vertices = buffers[0].checked_sub(self.packet.base_vertices);
        let indices = buffers[1].checked_sub(self.packet.base_indices);
        if vertices.and_then(|n| n.checked_div(4)) != count
            || indices.and_then(|n| n.checked_div(6)) != count
            || vertices.is_none_or(|n| !n.is_multiple_of(4))
            || indices.is_none_or(|n| !n.is_multiple_of(6))
        {
            self.decline("emitted_index_count_mismatch");
        }
        self.active = false;
    }
    pub fn final_lengths(&self) -> [usize; 2] {
        [self.packet.final_vertices, self.packet.final_indices]
    }
    pub fn mark_exported(&self) {
        self.exported.set(true);
    }
    pub fn statistics(&self) -> serde_json::Value {
        serde_json::json!({"enabled":self.enabled,"replay_pose":REPLAY_POSE,"chosen_call":self.chosen,
            "calls":self.calls,"finished":self.packet.finished,
            "accepted_segment_count":self.packet.accepted_segment_count,
            "ordinary_segment_count":self.packet.ordinary_segment_count,
            "retained_segment_count":self.packet.segments.len(),
            "unavailable_reason":self.packet.unavailable_reason,
            "retained_capacity_bytes":self.packet.retained_capacity_bytes,
            "cap_bytes":self.cap,"emitter_host_ns":self.packet.emitter_host_ns,
            "scope":"one emitter inclusive HOST only, never capture overhead/GPU time; capacity charge excludes allocator/RSS"})
    }
    /// Called outside timed callbacks. Caller must bind external immutable source/pose proof.
    pub fn write(
        &self,
        writer: impl io::Write,
        source_sha256: &str,
        context: &ferrite_render::RenderContext,
    ) -> io::Result<()> {
        if !self.current(context)
            || !self.packet.finished
            || self.packet.unavailable_reason.is_some()
            || self.packet.segments.is_empty()
            || self.packet.ordinary_segment_count == 0
            || source_sha256.len() != 64
            || !source_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(io::Error::other(
                "complete nonempty ordinary line packet/source binding unavailable",
            ));
        }
        #[derive(Serialize)]
        struct Export<'a> {
            schema: u8,
            source_sha256: &'a str,
            replay_pose: usize,
            scope: &'a str,
            packet: &'a Packet,
        }
        serde_json::to_writer(writer, &Export { schema: 1, source_sha256, replay_pose: REPLAY_POSE,
            scope: "diagnostic, one chosen emitter; exact clipped segment order before original quad expansion; HOST whole emitter not capture/GPU duration; clip rect and coverage source are references requiring same-pose R8/uniform/owner proof; no rendering or selection authority", packet: &self.packet })
            .map_err(io::Error::other)
    }
}
pub(crate) struct BoundedWriter {
    writer: io::BufWriter<std::fs::File>,
    bytes: usize,
}
impl BoundedWriter {
    pub fn new(file: std::fs::File) -> Self {
        Self {
            writer: io::BufWriter::with_capacity(64 * 1024, file),
            bytes: 0,
        }
    }
    pub fn sync_all(&self) -> io::Result<()> {
        self.writer.get_ref().sync_all()
    }
}
impl io::Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let total = self
            .bytes
            .checked_add(bytes.len())
            .filter(|n| *n <= 64 * 1024 * 1024)
            .ok_or_else(|| io::Error::other("packet JSON disk cap64MiB"))?;
        let n = self.writer.write(bytes)?;
        self.bytes = total - (bytes.len() - n);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> ferrite_render::RenderContext {
        ferrite_render::RenderContext::new(ferrite_render::Viewport::new(10., 10.))
    }
    fn capture() -> Capture {
        Capture::new(Some(OsStr::new("1")))
    }
    fn segment(c: &mut Capture) {
        c.record(
            [-0., 1., 2., 3.],
            [0., 0., 10., 10.],
            1.,
            [1., 0., 0., 0.5],
            [
                c.packet.base_vertices + c.packet.accepted_segment_count as usize * 4,
                c.packet.base_indices + c.packet.accepted_segment_count as usize * 6,
            ],
        );
    }
    #[test]
    fn default_off_and_no_automatic_startup_capture() {
        let ctx = context();
        let mut c = Capture::new(None);
        c.arm(REPLAY_POSE);
        c.begin(&ctx, [0, 0]);
        segment(&mut c);
        assert_eq!(c.packet.accepted_segment_count, 0);
        let mut c = capture();
        c.begin(&ctx, [0, 0]);
        segment(&mut c);
        assert_eq!(c.packet.accepted_segment_count, 0);
    }
    #[test]
    fn replay_arm_is_one_shot_and_complete_order_preserves_signed_zero() {
        let ctx = context();
        let mut c = capture();
        c.arm(198);
        c.begin(&ctx, [0, 0]);
        segment(&mut c);
        c.arm(REPLAY_POSE);
        c.begin(&ctx, [0, 0]);
        c.material(Material {
            ordinal: 19,
            primitive_kind: 1,
            ordinary_probe_eligible: true,
            ..Material::default()
        });
        c.path();
        segment(&mut c);
        c.material(Material {
            ordinal: 20,
            ..Material::default()
        });
        c.path();
        segment(&mut c);
        c.finish([8, 12]);
        assert_eq!(c.packet.segments.len(), 2);
        assert_eq!(c.packet.segments[0].endpoints_bits[0], (-0.0_f32).to_bits());
        assert_eq!(c.packet.segments[1].sequence, 1);
        assert_eq!(c.packet.segments[1].material.ordinal, 20);
        let mut bytes = Vec::new();
        c.write(&mut bytes, &"a".repeat(64), &ctx).unwrap();
        assert!(!bytes.is_empty());
        c.arm(REPLAY_POSE);
        c.begin(&ctx, [0, 0]);
        segment(&mut c);
        assert_eq!(c.packet.segments.len(), 2);
        assert!(!c.current(&ctx));
    }
    #[test]
    fn identical_foreign_context_and_changed_camera_cannot_bind() {
        let mut ctx = context();
        let foreign = context();
        let mut c = capture();
        c.arm(REPLAY_POSE);
        c.begin(&ctx, [0, 0]);
        c.material(Material {
            ordinary_probe_eligible: true,
            primitive_kind: 1,
            ..Material::default()
        });
        segment(&mut c);
        c.finish([4, 6]);
        assert!(c.current(&ctx));
        assert!(!c.current(&foreign));
        ctx.scaler
            .set_viewport(ferrite_render::Viewport::new(11., 10.));
        assert!(!c.current(&ctx));
        ctx.scaler
            .set_viewport(ferrite_render::Viewport::new(10., 10.));
        assert!(c.current(&ctx));
        c.retire_frame();
        assert!(!c.current(&ctx));
    }
    #[test]
    fn cap_discards_prefix_and_preserves_total() {
        assert!(std::mem::size_of::<Capture>() <= CONTROL);
        let ctx = context();
        let mut c = capture();
        c.cap = CONTROL + std::mem::size_of::<Segment>();
        c.arm(REPLAY_POSE);
        c.begin(&ctx, [0, 0]);
        segment(&mut c);
        segment(&mut c);
        c.finish([8, 12]);
        assert_eq!(c.packet.accepted_segment_count, 2);
        assert!(c.packet.segments.is_empty());
        assert_eq!(c.packet.unavailable_reason, Some("capacity_cap"));
    }
    #[test]
    fn nonzero_prefix_and_hatch_keep_exact_suffix_offsets_and_ownership() {
        let ctx = context();
        let mut c = capture();
        c.arm(REPLAY_POSE);
        c.begin(&ctx, [12, 18]);
        c.material(Material {
            ordinal: 3,
            primitive_kind: 1,
            ordinary_probe_eligible: true,
            ..Material::default()
        });
        segment(&mut c);
        c.material(Material {
            ordinal: 4,
            primitive_kind: 0,
            ordinary_probe_eligible: false,
            ..Material::default()
        });
        segment(&mut c);
        c.finish([20, 30]);
        assert_eq!(c.packet.unavailable_reason, None);
        assert_eq!(c.packet.ordinary_segment_count, 1);
        assert_eq!(
            (
                c.packet.segments[0].vertex_base,
                c.packet.segments[0].index_start
            ),
            (12, 18)
        );
        assert_eq!(
            (
                c.packet.segments[1].vertex_base,
                c.packet.segments[1].index_start
            ),
            (16, 24)
        );
        assert_eq!(c.packet.segments[1].material.primitive_kind, 0);
        assert_eq!(c.packet.segments[1].material.ordinal, 4);
    }
    #[test]
    fn failed_or_inconsistent_emitter_never_exports() {
        let ctx = context();
        let mut c = capture();
        c.arm(REPLAY_POSE);
        c.begin(&ctx, [0, 0]);
        segment(&mut c);
        assert!(c.write(io::sink(), &"a".repeat(64), &ctx).is_err());
        c.finish([8, 12]);
        assert_eq!(
            c.packet.unavailable_reason,
            Some("emitted_index_count_mismatch")
        );
    }
}
