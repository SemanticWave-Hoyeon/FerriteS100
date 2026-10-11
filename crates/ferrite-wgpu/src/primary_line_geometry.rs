//! Private chart-line storage; caller preserves visibility, projection and source order.
//! Never performs visibility, clipping, normals, style, owner or scale decisions.
use crate::{exact_line_quad::ExactLineQuad, LineVertex};
use std::sync::Arc;

type StorageResult<T> = std::result::Result<T, &'static str>;
type AuditBuffers<'a> = (
    std::borrow::Cow<'a, [LineVertex]>,
    std::borrow::Cow<'a, [u32]>,
);
type OriginalBuffers = (Vec<LineVertex>, Vec<u32>);

pub(crate) struct Prefix {
    owner: Arc<()>,
    generation: u64,
    vertices: usize,
    indices: usize,
}

pub(crate) struct Geometry {
    owner: Arc<()>,
    generation: u64,
    original_quad_topology: bool,
    packed_active: bool,
    packed: Vec<ExactLineQuad>,
    vertices: Vec<LineVertex>,
    indices: Vec<u32>,
    cap_bytes: usize,
    #[cfg(test)]
    refuse_materialization: bool,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            owner: Arc::new(()),
            generation: 0,
            original_quad_topology: true,
            packed_active: false,
            packed: Vec::new(),
            vertices: Vec::new(),
            indices: Vec::new(),
            cap_bytes: crate::exact_line_quad::CPU_CAP_BYTES,
            #[cfg(test)]
            refuse_materialization: false,
        }
    }
}

impl Geometry {
    pub(crate) fn with_legacy_capacity(vertices: usize, indices: usize) -> Self {
        Self {
            vertices: Vec::with_capacity(vertices),
            indices: Vec::with_capacity(indices),
            ..Self::default()
        }
    }
    pub(crate) fn new_frame(vertices: usize, indices: usize, prefer_packed: bool) -> Self {
        let mut geometry = Self::with_legacy_capacity(vertices, indices);
        geometry.begin_frame(prefer_packed);
        geometry
    }
    pub(crate) fn begin_frame(&mut self, prefer_packed: bool) {
        let prev_vertices = self.vertex_len();
        let prev_indices = self.index_len();
        if self.generation == u64::MAX {
            self.owner = Arc::new(());
            self.generation = 0;
        } else {
            self.generation += 1;
        }
        self.vertices.clear();
        self.indices.clear();
        self.original_quad_topology = true;
        self.packed.clear();
        self.packed_active = prefer_packed;
        if !prefer_packed {
            if prev_vertices > self.vertices.capacity() / 2 {
                self.vertices.reserve(prev_vertices);
            }
            if prev_indices > self.indices.capacity() / 2 {
                self.indices.reserve(prev_indices);
            }
        }
    }
    pub(crate) fn vertex_len(&self) -> usize {
        if self.packed_active {
            self.packed.len() * 4
        } else {
            self.vertices.len()
        }
    }
    pub(crate) fn index_len(&self) -> usize {
        if self.packed_active {
            self.packed.len() * 6
        } else {
            self.indices.len()
        }
    }
    /// Pure topology only. Does not authorize any draw range, owner, visibility or camera.
    pub(crate) fn original_quad_topology(&self) -> bool {
        self.original_quad_topology
            && self.vertex_len().is_multiple_of(4)
            && (self.vertex_len() / 4).checked_mul(6) == Some(self.index_len())
    }
    pub(crate) fn packed(&self) -> Option<&[ExactLineQuad]> {
        self.packed_active.then_some(self.packed.as_slice())
    }
    pub(crate) fn prefix(&self) -> Prefix {
        Prefix {
            owner: Arc::clone(&self.owner),
            generation: self.generation,
            vertices: self.vertex_len(),
            indices: self.index_len(),
        }
    }
    pub(crate) fn validate_prefix(&self, prefix: &Prefix) -> StorageResult<()> {
        if !Arc::ptr_eq(&prefix.owner, &self.owner)
            || prefix.generation != self.generation
            || prefix.vertices > self.vertex_len()
            || prefix.indices > self.index_len()
        {
            return Err("Foreign or stale primary line prefix");
        }
        if self.packed_active
            && (!prefix.vertices.is_multiple_of(4)
                || !prefix.indices.is_multiple_of(6)
                || prefix.vertices / 4 != prefix.indices / 6)
        {
            return Err("Non-quad primary line prefix");
        }
        Ok(())
    }
    pub(crate) fn truncate(&mut self, prefix: &Prefix) -> StorageResult<()> {
        self.validate_prefix(prefix)?;
        if self.packed_active {
            if !prefix.vertices.is_multiple_of(4)
                || !prefix.indices.is_multiple_of(6)
                || prefix.vertices / 4 != prefix.indices / 6
            {
                return Err("Non-quad primary line prefix");
            }
            self.packed.truncate(prefix.vertices / 4);
        } else {
            self.vertices.truncate(prefix.vertices);
            self.indices.truncate(prefix.indices);
        }
        Ok(())
    }
    // Capacity/readback charges are storage bytes, not virtual vertex capacity.
    pub(crate) fn charged_cpu_bytes(&self) -> usize {
        self.packed.capacity() * std::mem::size_of::<ExactLineQuad>()
            + self.vertices.capacity() * std::mem::size_of::<LineVertex>()
            + self.indices.capacity() * std::mem::size_of::<u32>()
    }
    pub(crate) fn active_vertex_storage_bytes(&self) -> usize {
        if self.packed_active {
            self.packed.capacity() * std::mem::size_of::<ExactLineQuad>()
        } else {
            self.vertices.capacity() * std::mem::size_of::<LineVertex>()
        }
    }
    pub(crate) fn active_index_storage_bytes(&self) -> usize {
        if self.packed_active {
            0
        } else {
            self.indices.capacity() * std::mem::size_of::<u32>()
        }
    }

    // These arguments are the ORIGINAL already-computed emitter fields. All offsets
    // retained verbatim. Caller does not move sqrt/divide/clip decisions into this API.
    pub(crate) fn append_emitted(
        &mut self,
        start: [f32; 2],
        end: [f32; 2],
        negative: [f32; 2],
        positive: [f32; 2],
        color: [f32; 4],
    ) -> StorageResult<()> {
        let compact = if self.packed_active {
            ExactLineQuad::from_emitted(start, end, [negative, positive, positive, negative], color)
        } else {
            None
        };
        if self.packed_active && compact.is_some() {
            let count = self
                .packed
                .len()
                .checked_add(1)
                .ok_or("Line count overflow")?;
            let charge = count
                .checked_mul(std::mem::size_of::<ExactLineQuad>())
                .ok_or("Line charge overflow")?;
            let max_quads = self.cap_bytes / std::mem::size_of::<ExactLineQuad>();
            let reservation = if count <= self.packed.capacity() {
                true
            } else {
                let target = self
                    .packed
                    .capacity()
                    .saturating_mul(2)
                    .max(count)
                    .max(256)
                    .min(max_quads);
                target >= count
                    && self
                        .packed
                        .try_reserve_exact(target - self.packed.len())
                        .is_ok()
            };
            if charge <= self.cap_bytes
                && reservation
                && self.packed.capacity() * std::mem::size_of::<ExactLineQuad>() <= self.cap_bytes
            {
                self.packed.push(compact.ok_or("Missing emitted quad")?);
                return Ok(());
            }
        }
        if self.packed_active {
            self.materialize()?;
        }
        let base = u32::try_from(self.vertices.len()).map_err(|_| "Line vertex index overflow")?;
        base.checked_add(3).ok_or("Line vertex index overflow")?;
        self.vertices
            .try_reserve(4)
            .map_err(|_| "Legacy line vertex reservation")?;
        self.indices
            .try_reserve(6)
            .map_err(|_| "Legacy line index reservation")?;
        self.vertices.extend([
            LineVertex::new(start[0], start[1], negative[0], negative[1], color),
            LineVertex::new(start[0], start[1], positive[0], positive[1], color),
            LineVertex::new(end[0], end[1], positive[0], positive[1], color),
            LineVertex::new(end[0], end[1], negative[0], negative[1], color),
        ]);
        self.indices
            .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        Ok(())
    }

    // Whole representation degradation is one-way for this frame. Reservation failure
    // leaves packed content authoritative; caller MUST return typed preparation error,
    // disarm readiness/skip render. Never treat this error as an empty line scene.
    pub(crate) fn materialize(&mut self) -> StorageResult<()> {
        if !self.packed_active {
            return Ok(());
        }
        #[cfg(test)]
        if self.refuse_materialization {
            return Err("Injected legacy reservation failure");
        }
        let nv = self
            .packed
            .len()
            .checked_mul(4)
            .ok_or("Line vertex count overflow")?;
        let ni = self
            .packed
            .len()
            .checked_mul(6)
            .ok_or("Line index count overflow")?;
        if nv > u32::MAX as usize {
            return Err("Line vertex index overflow");
        }
        // Legacy Vecs are empty while primary active; both reserves before any write.
        self.vertices
            .try_reserve_exact(nv)
            .map_err(|_| "Legacy vertex materialization")?;
        self.indices
            .try_reserve_exact(ni)
            .map_err(|_| "Legacy index materialization")?;
        for (ordinal, quad) in self.packed.iter().enumerate() {
            self.vertices.extend(quad.expand());
            let b = (ordinal * 4) as u32;
            self.indices.extend([b, b + 1, b + 2, b, b + 2, b + 3]);
        }
        self.packed_active = false;
        self.packed.clear();
        if self.packed.capacity() * std::mem::size_of::<ExactLineQuad>() > self.cap_bytes {
            self.packed = Vec::new();
        }
        Ok(())
    }
    pub(crate) fn legacy(&self) -> Option<(&[LineVertex], &[u32])> {
        (!self.packed_active).then_some((&self.vertices, &self.indices))
    }
    // Only a private quad-aligned suffix may stay compact. Rewrites each original
    // corner on CPU with EXACT old offset += position - anchor operation ordering.
    // Nonfinite rewrite declines the entire compact representation, preserving the
    // original legacy arithmetic rather than silently omitting that source.
    pub(crate) fn reanchor_suffix(
        &mut self,
        vertex_start: usize,
        anchor: [f32; 2],
    ) -> StorageResult<()> {
        if vertex_start > self.vertex_len() {
            return Err("Invalid line anchor suffix");
        }
        if self.packed_active && vertex_start.is_multiple_of(4) {
            let suffix = &self.packed[vertex_start / 4..];
            if suffix.iter().all(|q| q.reanchored(anchor).is_some()) {
                for q in &mut self.packed[vertex_start / 4..] {
                    *q = q.reanchored(anchor).ok_or("Anchor proof changed")?;
                }
                return Ok(());
            }
        }
        self.materialize()?;
        for v in &mut self.vertices[vertex_start..] {
            v.offset[0] += v.position[0] - anchor[0];
            v.offset[1] += v.position[1] - anchor[1];
            v.position = anchor;
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn legacy_indices_mut(&mut self) -> StorageResult<&mut Vec<u32>> {
        self.materialize()?;
        self.original_quad_topology = false;
        Ok(&mut self.indices)
    }
    pub(crate) fn audit_buffers(&self) -> StorageResult<AuditBuffers<'_>> {
        if let Some((vertices, indices)) = self.legacy() {
            return Ok((
                std::borrow::Cow::Borrowed(vertices),
                std::borrow::Cow::Borrowed(indices),
            ));
        }
        let (vertices, indices) = self.export_legacy()?;
        Ok((
            std::borrow::Cow::Owned(vertices),
            std::borrow::Cow::Owned(indices),
        ))
    }
    // Read-only audit creates one bounded original representation outside timed replay.
    // It does not retire primary storage or cause next moving frame to fall back.
    pub(crate) fn export_legacy(&self) -> StorageResult<OriginalBuffers> {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        vertices
            .try_reserve_exact(self.vertex_len())
            .map_err(|_| "Line audit vertex reservation")?;
        indices
            .try_reserve_exact(self.index_len())
            .map_err(|_| "Line audit index reservation")?;
        if self.packed_active {
            for (ordinal, quad) in self.packed.iter().enumerate() {
                vertices.extend(quad.expand());
                let b = (ordinal * 4) as u32;
                indices.extend([b, b + 1, b + 2, b, b + 2, b + 3]);
            }
        } else {
            vertices.extend(&self.vertices);
            indices.extend(&self.indices);
        }
        Ok((vertices, indices))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn append(g: &mut Geometry, n: f32) -> StorageResult<()> {
        g.append_emitted(
            [n, -0.0],
            [n + 2.0, 10.0],
            [-0.0, -1.0],
            [0.0, 1.0],
            [0.0, 0.3, 0.7, 0.5],
        )
    }
    fn equal_original(a: &Geometry, b: &Geometry) {
        let (av, ai) = a.export_legacy().unwrap();
        let (bv, bi) = b.export_legacy().unwrap();
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&av),
            bytemuck::cast_slice::<_, u8>(&bv)
        );
        assert_eq!(ai, bi);
    }
    #[test]
    fn direct_primary_matches_legacy_without_shadow() {
        let mut a = Geometry::default();
        let mut b = Geometry::default();
        a.begin_frame(false);
        b.begin_frame(true);
        for i in 0..128 {
            append(&mut a, i as f32).unwrap();
            append(&mut b, i as f32).unwrap();
        }
        assert!(b.vertices.is_empty() && b.indices.is_empty());
        assert_eq!(b.vertex_len(), 512);
        assert_eq!(b.index_len(), 768);
        equal_original(&a, &b);
        assert!(
            b.packed.capacity() * std::mem::size_of::<ExactLineQuad>()
                <= crate::exact_line_quad::CPU_CAP_BYTES
        );
    }
    #[test]
    fn independent_original_span_and_hatch_normals_all_bits() {
        for (a, b) in [
            ([0.0f32, -0.0], [7.0, 10.0]),
            ([100.0, 10.0], [-80.25, 210.5]),
            ([-0.0, 1.0], [-0.0, 90.0]),
        ] {
            for width in [0.125f32, 1.0, 8.5] {
                let dx = b[0] - a[0];
                let dy = b[1] - a[1];
                let len = (dx * dx + dy * dy).sqrt();
                let color = [0.0, 0.3, 0.7, 0.5];
                for hatch in [false, true] {
                    // Preserve two distinct original operation sequences, not algebraic merge.
                    let (nx, ny) = if hatch {
                        let half = width * 0.5;
                        (-dy / len * half, dx / len * half)
                    } else {
                        (-dy / len * width * 0.5, dx / len * width * 0.5)
                    };
                    let original = [
                        LineVertex::new(a[0], a[1], -nx, -ny, color),
                        LineVertex::new(a[0], a[1], nx, ny, color),
                        LineVertex::new(b[0], b[1], nx, ny, color),
                        LineVertex::new(b[0], b[1], -nx, -ny, color),
                    ];
                    let mut g = Geometry::default();
                    g.begin_frame(true);
                    g.append_emitted(a, b, [-nx, -ny], [nx, ny], color).unwrap();
                    let (v, i) = g.export_legacy().unwrap();
                    assert_eq!(
                        bytemuck::cast_slice::<_, u8>(&v),
                        bytemuck::cast_slice::<_, u8>(&original)
                    );
                    assert_eq!(i, [0, 1, 2, 0, 2, 3]);
                }
            }
        }
    }
    #[test]
    fn cap_whole_degrade_continues_exact_order() {
        let mut a = Geometry::default();
        let mut b = Geometry::default();
        a.begin_frame(false);
        b.begin_frame(true);
        b.cap_bytes = 128;
        for i in 0..8 {
            append(&mut a, i as f32).unwrap();
            append(&mut b, i as f32).unwrap();
        }
        assert!(b.packed().is_none());
        equal_original(&a, &b);
        assert!(b.packed.capacity() * std::mem::size_of::<ExactLineQuad>() <= b.cap_bytes);
    }
    #[test]
    fn prefix_survives_degrade_and_foreign_stale_rejected() {
        let mut g = Geometry::default();
        g.begin_frame(true);
        append(&mut g, 1.0).unwrap();
        let prefix = g.prefix();
        append(&mut g, 2.0).unwrap();
        g.materialize().unwrap();
        g.truncate(&prefix).unwrap();
        assert_eq!((g.vertex_len(), g.index_len()), (4, 6));
        let other = Geometry::default().prefix();
        assert!(g.truncate(&other).is_err());
        g.begin_frame(true);
        append(&mut g, 3.0).unwrap();
        assert!(g.truncate(&prefix).is_err());
    }
    #[test]
    fn dynamic_anchor_four_offsets_remain_compact_and_all_bits_original() {
        let mut g = Geometry::default();
        g.begin_frame(true);
        append(&mut g, 1.0).unwrap();
        let p = g.prefix();
        append(&mut g, 2.0).unwrap();
        g.truncate(&p).unwrap();
        let (mut original, _) = g.export_legacy().unwrap();
        let anchor = [100.0f32, -0.0];
        for v in &mut original {
            v.offset[0] += v.position[0] - anchor[0];
            v.offset[1] += v.position[1] - anchor[1];
            v.position = anchor;
        }
        g.reanchor_suffix(0, anchor).unwrap();
        assert!(g.packed().is_some());
        let (actual, _) = g.export_legacy().unwrap();
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&actual),
            bytemuck::cast_slice::<_, u8>(&original)
        );
        assert_ne!(
            actual[0].offset.map(f32::to_bits),
            actual[3].offset.map(f32::to_bits)
        );
    }
    #[test]
    fn unaligned_anchor_and_overflow_whole_degrade_keep_original() {
        for (start, anchor) in [(1, [0.0, 0.0]), (0, [-f32::MAX, 0.0])] {
            let mut g = Geometry::default();
            g.begin_frame(true);
            g.append_emitted(
                [f32::MAX, 0.0],
                [f32::MAX, 1.0],
                [-0.0, -1.0],
                [0.0, 1.0],
                [1.0; 4],
            )
            .unwrap();
            let (mut original, _) = g.export_legacy().unwrap();
            for v in &mut original[start..] {
                v.offset[0] += v.position[0] - anchor[0];
                v.offset[1] += v.position[1] - anchor[1];
                v.position = anchor;
            }
            g.reanchor_suffix(start, anchor).unwrap();
            assert!(g.packed().is_none());
            assert_eq!(
                bytemuck::cast_slice::<_, u8>(g.legacy().unwrap().0),
                bytemuck::cast_slice::<_, u8>(&original)
            );
        }
    }
    #[test]
    fn nonfinite_original_fields_degrade_not_omit() {
        let mut g = Geometry::default();
        g.begin_frame(true);
        append(&mut g, 1.0).unwrap();
        g.append_emitted(
            [1.0, 2.0],
            [3.0, 4.0],
            [-0.0, -1.0],
            [0.0, 1.0],
            [f32::NAN, 0.0, 0.0, 1.0],
        )
        .unwrap();
        assert!(g.packed().is_none());
        assert_eq!(g.index_len(), 12);
        assert!(g.legacy().unwrap().0[4].color[0].is_nan());
    }
    #[test]
    fn failed_fallback_keeps_primary_and_prefix_unchanged() {
        let mut g = Geometry::default();
        g.begin_frame(true);
        append(&mut g, 1.0).unwrap();
        g.refuse_materialization = true;
        assert!(g.materialize().is_err());
        assert!(g.packed().is_some());
        assert_eq!((g.vertex_len(), g.index_len()), (4, 6));
        assert!(g.vertices.is_empty() && g.indices.is_empty());
    }
    #[test]
    fn legacy_audit_borrows_without_duplicate_capacity() {
        let mut geometry = Geometry::new_frame(16, 24, false);
        append(&mut geometry, 1.).unwrap();
        let charge = geometry.charged_cpu_bytes();
        let (vertices, indices) = geometry.audit_buffers().unwrap();
        assert!(matches!(vertices, std::borrow::Cow::Borrowed(_)));
        assert!(matches!(indices, std::borrow::Cow::Borrowed(_)));
        assert_eq!(geometry.charged_cpu_bytes(), charge);
    }
    #[test]
    fn empty_clear_and_readonly_export_preserve_backend() {
        let mut g = Geometry::default();
        g.begin_frame(true);
        assert!(g.export_legacy().unwrap().0.is_empty());
        append(&mut g, 1.0).unwrap();
        let _ = g.export_legacy().unwrap();
        assert!(g.packed().is_some());
        g.begin_frame(false);
        assert_eq!((g.vertex_len(), g.index_len()), (0, 0));
        assert!(g.packed().is_none());
    }
}

#[cfg(test)]
mod immutable_topology_controls {
    use super::*;
    #[test]
    fn sealed_pattern_survives_primary_degrade_prefix_and_reset_but_not_edit() {
        for packed in [false, true] {
            let mut g = Geometry::new_frame(0, 0, packed);
            let empty = g.prefix();
            for _ in 0..32 {
                g.append_emitted([0., -0.], [3., 4.], [-0., -1.], [0., 1.], [0., 0., 0., 1.])
                    .unwrap();
                assert!(g.original_quad_topology());
            }
            g.materialize().unwrap();
            let indices = g.legacy().unwrap().1;
            for (q, six) in indices.as_chunks::<6>().0.iter().enumerate() {
                let b = q as u32 * 4;
                assert_eq!(*six, [b, b + 1, b + 2, b, b + 2, b + 3]);
            }
            g.truncate(&empty).unwrap();
            assert!(g.original_quad_topology());
            g.legacy_indices_mut().unwrap().extend([1, 2]);
            assert!(!g.original_quad_topology());
            g.begin_frame(false);
            assert!(g.original_quad_topology());
        }
    }
}
