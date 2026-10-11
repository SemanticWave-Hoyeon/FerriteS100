//! Completed private-font text draw preparation only. Fresh layout/authority stays outside.
use super::{ChartTextShape, CompositionPlane, GpuChartText};
use crate::referenced_chart_owner::ReferencedChartOwner;
use std::sync::{Arc, Weak};
const KEY_CAP: usize = 1024 * 1024;
const CPU_CAP: usize = 2 * 1024 * 1024;
const GPU_CAP: u64 = 16 * 1024 * 1024;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Environment {
    ppp: u32,
    extent: [u32; 2],
}
impl Environment {
    pub(super) fn new(ppp: f32, extent: [u32; 2]) -> Self {
        Self {
            ppp: ppp.to_bits(),
            extent,
        }
    }
}
fn options(o: egui::epaint::TessellationOptions) -> [u32; 14] {
    [
        o.feathering as u32,
        o.feathering_size_in_pixels.to_bits(),
        o.coarse_tessellation_culling as u32,
        o.prerasterized_discs as u32,
        o.round_text_to_pixels as u32,
        o.round_line_segments_to_pixels as u32,
        o.round_rects_to_pixels as u32,
        o.debug_paint_clip_rects as u32,
        o.debug_paint_text_rects as u32,
        o.debug_ignore_clip_rects as u32,
        o.bezier_tolerance.to_bits(),
        o.epsilon.to_bits(),
        o.parallel_tessellation as u32,
        o.validate_meshes as u32,
    ]
}
fn rect(r: egui::Rect) -> [u32; 4] {
    [
        r.min.x.to_bits(),
        r.min.y.to_bits(),
        r.max.x.to_bits(),
        r.max.y.to_bits(),
    ]
}
fn text_bits(t: &egui::epaint::TextShape) -> [u32; 5] {
    [
        t.pos.x.to_bits(),
        t.pos.y.to_bits(),
        t.underline.width.to_bits(),
        t.opacity_factor.to_bits(),
        t.angle.to_bits(),
    ]
}
enum ShapeKey {
    Text {
        galley: Weak<egui::Galley>,
        bits: [u32; 5],
        colors: [[u8; 4]; 2],
        override_color: Option<[u8; 4]>,
    },
    Line {
        points: [u32; 4],
        width: u32,
        color: [u8; 4],
    },
    Polygon {
        points: [[u32; 2]; 4],
        fill: [u8; 4],
    },
}
impl ShapeKey {
    fn capture(shape: &egui::Shape) -> Option<Self> {
        match shape {
            egui::Shape::Text(t) => Some(Self::Text {
                galley: Arc::downgrade(&t.galley),
                bits: text_bits(t),
                colors: [t.underline.color.to_array(), t.fallback_color.to_array()],
                override_color: t.override_text_color.map(|c| c.to_array()),
            }),
            egui::Shape::LineSegment { points, stroke } => Some(Self::Line {
                points: [
                    points[0].x.to_bits(),
                    points[0].y.to_bits(),
                    points[1].x.to_bits(),
                    points[1].y.to_bits(),
                ],
                width: stroke.width.to_bits(),
                color: stroke.color.to_array(),
            }),
            egui::Shape::Path(p)
                if p.closed
                    && p.points.len() == 4
                    && p.stroke.width.to_bits() == 0
                    && p.stroke.kind == egui::StrokeKind::Middle
                    && matches!(p.stroke.color,egui::epaint::ColorMode::Solid(c) if c==egui::Color32::TRANSPARENT) =>
            {
                Some(Self::Polygon {
                    points: std::array::from_fn(|i| {
                        [p.points[i].x.to_bits(), p.points[i].y.to_bits()]
                    }),
                    fill: p.fill.to_array(),
                })
            }
            _ => None, // Whole original path; no unsupported callbacks/shapes omitted.
        }
    }
    fn matches(&self, shape: &egui::Shape) -> bool {
        match (self, shape) {
            (
                Self::Text {
                    galley,
                    bits,
                    colors,
                    override_color,
                },
                egui::Shape::Text(t),
            ) =>
            // Weak holds the allocation, preventing ABA; current Arc proves it is alive.
            {
                galley.as_ptr() == Arc::as_ptr(&t.galley)
                    && *bits == text_bits(t)
                    && *colors == [t.underline.color.to_array(), t.fallback_color.to_array()]
                    && *override_color == t.override_text_color.map(|c| c.to_array())
            }
            (
                Self::Line {
                    points,
                    width,
                    color,
                },
                egui::Shape::LineSegment { points: p, stroke },
            ) => {
                *points
                    == [
                        p[0].x.to_bits(),
                        p[0].y.to_bits(),
                        p[1].x.to_bits(),
                        p[1].y.to_bits(),
                    ]
                    && *width == stroke.width.to_bits()
                    && *color == stroke.color.to_array()
            }
            (Self::Polygon { points, fill }, egui::Shape::Path(p)) => {
                p.closed
                    && p.points.len() == 4
                    && p.stroke.width.to_bits() == 0
                    && p.stroke.kind == egui::StrokeKind::Middle
                    && matches!(p.stroke.color,egui::epaint::ColorMode::Solid(c) if c==egui::Color32::TRANSPARENT)
                    && *points
                        == std::array::from_fn(|i| {
                            [p.points[i].x.to_bits(), p.points[i].y.to_bits()]
                        })
                    && *fill == p.fill.to_array()
            }
            _ => false,
        }
    }
}
struct Slot {
    group: (CompositionPlane, i32, Option<usize>, u8),
    clip: [u32; 4],
    shape: ShapeKey,
}
pub(super) struct Key {
    owner: Weak<()>,
    epoch: u64,
    environment: Environment,
    options: [u32; 14],
    slots: Vec<Slot>,
    charge: usize,
    atlas_bytes: u64,
}
impl Key {
    pub(super) fn capture(
        owner: &ReferencedChartOwner,
        environment: Environment,
        shapes: &[ChartTextShape],
    ) -> Option<Self> {
        let (identity, epoch) = owner.preparation_identity();
        let mut key = Self::capture_parts(
            identity,
            epoch,
            environment,
            options(owner.context.tessellation_options(|o| *o)),
            shapes,
        )?;
        let size = owner.context.fonts(|f| f.font_image_size());
        key.atlas_bytes = u64::try_from(size[0])
            .ok()?
            .checked_mul(u64::try_from(size[1]).ok()?)?
            .checked_mul(4)?;
        // Managed(0) is egui-wgpu 0.31 RGBA8 single-mip; account retained binding texture too.
        (key.atlas_bytes <= GPU_CAP).then_some(key)
    }
    fn capture_parts(
        identity: &Arc<()>,
        epoch: u64,
        environment: Environment,
        options: [u32; 14],
        shapes: &[ChartTextShape],
    ) -> Option<Self> {
        if shapes.is_empty() {
            return None;
        }
        // Weak references keep their Arc allocation headers alive after strong owners drop.
        // Charge each text slot conservatively even when several slots share one galley.
        let weak_charge = shapes
            .iter()
            .try_fold(2 * std::mem::size_of::<usize>(), |n, s| {
                if matches!(s.4.shape, egui::Shape::Text(_)) {
                    n.checked_add(std::mem::size_of::<egui::Galley>())?
                        .checked_add(2 * std::mem::size_of::<usize>())
                } else {
                    Some(n)
                }
            })?;
        if shapes
            .len()
            .checked_mul(std::mem::size_of::<Slot>())?
            .checked_add(std::mem::size_of::<Self>())?
            .checked_add(weak_charge)?
            > KEY_CAP
        {
            return None;
        }
        let mut slots = Vec::new();
        slots.try_reserve_exact(shapes.len()).ok()?;
        let charge = slots
            .capacity()
            .checked_mul(std::mem::size_of::<Slot>())?
            .checked_add(std::mem::size_of::<Self>())?
            .checked_add(weak_charge)?;
        if charge > KEY_CAP {
            return None;
        }
        for (plane, priority, source, wrap, s) in shapes {
            slots.push(Slot {
                group: (*plane, *priority, *source, *wrap),
                clip: rect(s.clip_rect),
                shape: ShapeKey::capture(&s.shape)?,
            });
        }
        Some(Self {
            owner: Arc::downgrade(identity),
            epoch,
            environment,
            options,
            slots,
            charge,
            atlas_bytes: 0, // CPU oracle; production capture sets exact current atlas charge.
        })
    }
    fn matches_parts(
        &self,
        identity: &Arc<()>,
        epoch: u64,
        environment: Environment,
        options: [u32; 14],
        shapes: &[ChartTextShape],
    ) -> bool {
        self.owner
            .upgrade()
            .is_some_and(|old| Arc::ptr_eq(&old, identity))
            && self.epoch == epoch
            && self.environment == environment
            && self.options == options
            && self.slots.len() == shapes.len()
            && self
                .slots
                .iter()
                .zip(shapes)
                .all(|(a, (plane, priority, source, wrap, s))| {
                    a.group == (*plane, *priority, *source, *wrap)
                        && a.clip == rect(s.clip_rect)
                        && a.shape.matches(&s.shape)
                })
    }
}
pub(super) fn record_lookup(trace: &mut Option<Vec<egui::TextureId>>, id: egui::TextureId) {
    let Some(ids) = trace.as_mut() else {
        return;
    };
    // Reserve the complete bounded trace once, never grow it during tessellation.
    if ids.len() >= 16_384
        || (ids.capacity() == 0 && ids.try_reserve_exact(16_384).is_err())
        || ids
            .capacity()
            .checked_mul(std::mem::size_of::<egui::TextureId>())
            .is_none_or(|bytes| bytes > 256 * 1024)
    {
        *trace = None;
    } else {
        ids.push(id);
    }
}
pub(super) struct Cache {
    enabled: bool,
    key: Option<Key>,
    meshes: Vec<GpuChartText>,
    lookups: Vec<egui::TextureId>,
    hits: u64,
    reused_draws: u64,
    builds: u64,
    declines: u64,
    cpu: usize,
    gpu: u64,
}
impl Cache {
    pub(super) fn new(flag: Option<&std::ffi::OsStr>) -> Self {
        Self {
            enabled: flag == Some(std::ffi::OsStr::new("1")),
            key: None,
            meshes: Vec::new(),
            lookups: Vec::new(),
            hits: 0,
            reused_draws: 0,
            builds: 0,
            declines: 0,
            cpu: 0,
            gpu: 0,
        }
    }
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }
    pub(super) fn clear(&mut self) {
        self.key = None;
        self.meshes = Vec::new();
        self.lookups = Vec::new();
        self.cpu = 0;
        self.gpu = 0;
    }
    pub(super) fn matches(
        &self,
        owner: &ReferencedChartOwner,
        environment: Environment,
        shapes: &[ChartTextShape],
    ) -> bool {
        let (identity, epoch) = owner.preparation_identity();
        self.key.as_ref().is_some_and(|key| {
            key.matches_parts(
                identity,
                epoch,
                environment,
                options(owner.context.tessellation_options(|o| *o)),
                shapes,
            )
        })
    }
    pub(super) fn replay(
        &mut self,
        owner: &ReferencedChartOwner,
        out: &mut Vec<GpuChartText>,
    ) -> crate::Result<bool> {
        out.clear();
        if out.try_reserve(self.meshes.len()).is_err() {
            self.declines = self.declines.saturating_add(1);
            return Ok(false);
        }
        // Includes nonempty meshes later rejected by scissor: preserve original lookup errors/order.
        for id in &self.lookups {
            owner.bind_group(*id)?;
        }
        // Binding namespace is exactly the same live private owner and atlas generation.
        out.extend(self.meshes.iter().cloned());
        self.hits = self.hits.saturating_add(1);
        self.reused_draws = self.reused_draws.saturating_add(self.meshes.len() as u64);
        Ok(true)
    }
    pub(super) fn store(
        &mut self,
        key: Option<Key>,
        lookups: Option<Vec<egui::TextureId>>,
        meshes: &[GpuChartText],
    ) {
        if !self.enabled {
            return;
        }
        if meshes.is_empty() {
            self.declines = self.declines.saturating_add(1);
            return;
        }
        let Some(lookups) = lookups else {
            self.declines = self.declines.saturating_add(1);
            return;
        };
        // Current private font atlas admits Managed(0) only. Unknown namespaces wholly decline.
        if lookups.iter().any(|id| *id != egui::TextureId::Managed(0))
            || meshes
                .iter()
                .any(|m| m.texture_id != egui::TextureId::Managed(0))
        {
            self.declines = self.declines.saturating_add(1);
            return;
        }
        let Some(trace_charge) = lookups
            .capacity()
            .checked_mul(std::mem::size_of::<egui::TextureId>())
        else {
            return;
        };
        let Some(key) = key else {
            self.declines = self.declines.saturating_add(1);
            return;
        };
        let Some(requested) = meshes
            .len()
            .checked_mul(std::mem::size_of::<GpuChartText>())
            .and_then(|n| n.checked_add(key.charge)?.checked_add(trace_charge))
        else {
            return;
        };
        if requested > CPU_CAP {
            self.declines = self.declines.saturating_add(1);
            return;
        }
        let Some(gpu) = meshes.iter().try_fold(key.atlas_bytes, |n, m| {
            n.checked_add(m.vertices.size())?
                .checked_add(m.indices.size())
        }) else {
            return;
        };
        if gpu > GPU_CAP {
            self.declines = self.declines.saturating_add(1);
            return;
        }
        let mut draws = Vec::new();
        if draws.try_reserve_exact(meshes.len()).is_err() {
            self.declines = self.declines.saturating_add(1);
            return;
        }
        let Some(cpu) = draws
            .capacity()
            .checked_mul(std::mem::size_of::<GpuChartText>())
            .and_then(|n| n.checked_add(key.charge)?.checked_add(trace_charge))
        else {
            return;
        };
        if cpu > CPU_CAP {
            self.declines = self.declines.saturating_add(1);
            return;
        }
        draws.extend(meshes.iter().cloned());
        self.key = Some(key);
        self.meshes = draws;
        self.lookups = lookups;
        self.cpu = cpu;
        self.gpu = gpu;
        self.builds = self.builds.saturating_add(1);
    }
    pub(super) fn statistics(&self) -> serde_json::Value {
        serde_json::json!({"enabled":self.enabled,"hits":self.hits,"reused_draws":self.reused_draws,"avoided_pool_write_calls":self.reused_draws.saturating_mul(2),"builds":self.builds,"declines":self.declines,
        "cpu_retained_bytes":self.cpu,"cpu_cap":CPU_CAP,"gpu_referenced_bytes":self.gpu,"gpu_cap":GPU_CAP,"scope":"fresh-layout private-owner exact-shape preparation only"})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn plane(n: i32) -> CompositionPlane {
        CompositionPlane::new(
            ferrite_kernel::CompositionStage::Chart,
            std::num::NonZeroI32::new(n).unwrap(),
        )
    }
    fn fixture() -> (egui::Context, Vec<ChartTextShape>) {
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput::default());
        let galley = ctx.fonts(|f| {
            f.layout_no_wrap(
                "Names 52".into(),
                egui::FontId::default(),
                egui::Color32::WHITE,
            )
        });
        let _ = ctx.end_pass();
        let shape = egui::epaint::ClippedShape {
            clip_rect: egui::Rect::from_min_max(egui::pos2(0., 0.), egui::pos2(800., 600.)),
            shape: egui::epaint::TextShape::new(egui::pos2(10., 20.), galley, egui::Color32::WHITE)
                .into(),
        };
        (ctx, vec![(plane(1), 3, Some(7), 0, shape)])
    }
    fn environment() -> Environment {
        Environment::new(1., [800, 600])
    }
    #[test]
    fn identical_producer_is_positive_and_epoch_is_not_frame_counter() {
        let (ctx, s) = fixture();
        let owner = Arc::new(());
        let o = options(ctx.tessellation_options(|o| *o));
        let key = Key::capture_parts(&owner, 8, environment(), o, &s).unwrap();
        for _ in 0..500 {
            assert!(key.matches_parts(&owner, 8, environment(), o, &s));
        }
        // Every original mesh payload is stable for exactly the admitted current producer.
        let cpu = |shapes: Vec<ChartTextShape>| {
            ctx.tessellate(shapes.into_iter().map(|s| s.4).collect(), 1.)
                .into_iter()
                .filter_map(|p| {
                    if let egui::epaint::Primitive::Mesh(m) = p.primitive {
                        Some((super::super::chart_text_vertices(&m, 1.), m.indices))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        let a = cpu(s.clone());
        let b = cpu(s.clone());
        assert_eq!(a.len(), b.len());
        for (a, b) in a.iter().zip(&b) {
            assert_eq!(
                bytemuck::cast_slice::<_, u8>(&a.0),
                bytemuck::cast_slice::<_, u8>(&b.0)
            );
            assert_eq!(a.1, b.1);
        }
    }
    #[test]
    fn owner_samebytes_atlas_dpi_resize_options_retire() {
        let (ctx, s) = fixture();
        let a = Arc::new(());
        let b = Arc::new(());
        let o = options(ctx.tessellation_options(|o| *o));
        let key = Key::capture_parts(&a, 0, environment(), o, &s).unwrap();
        assert!(!key.matches_parts(&b, 0, environment(), o, &s));
        assert!(!key.matches_parts(&a, 1, environment(), o, &s));
        assert!(!key.matches_parts(&a, 0, Environment::new(2., [800, 600]), o, &s));
        assert!(!key.matches_parts(&a, 0, Environment::new(1., [801, 600]), o, &s));
        for field in 0..o.len() {
            let mut changed = o;
            changed[field] ^= 1;
            assert!(!key.matches_parts(&a, 0, environment(), changed, &s));
        }
    }
    #[test]
    fn order_mask_subset_source_wrap_plane_priority_and_clip_are_not_count_keys() {
        let (ctx, mut s) = fixture();
        let owner = Arc::new(());
        let o = options(ctx.tessellation_options(|o| *o));
        let mut second = s[0].clone();
        second.2 = Some(8);
        s.push(second);
        let key = Key::capture_parts(&owner, 0, environment(), o, &s).unwrap();
        let mut reordered = s.clone();
        reordered.reverse();
        assert!(!key.matches_parts(&owner, 0, environment(), o, &reordered));
        assert!(!key.matches_parts(&owner, 0, environment(), o, &s[..1]));
        for change in 0..5 {
            let mut changed = s.clone();
            match change {
                0 => changed[0].0 = plane(2),
                1 => changed[0].1 += 1,
                2 => changed[0].2 = None,
                3 => changed[0].3 = 1,
                _ => changed[0].4.clip_rect.max.x += 1.,
            };
            assert!(!key.matches_parts(&owner, 0, environment(), o, &changed));
        }
    }
    #[test]
    fn glyph_ownership_and_all_text_scalars_including_signedzero_are_exact() {
        let (ctx, s) = fixture();
        let owner = Arc::new(());
        let o = options(ctx.tessellation_options(|o| *o));
        let key = Key::capture_parts(&owner, 0, environment(), o, &s).unwrap();
        for change in 0..8 {
            let mut changed = s.clone();
            let egui::Shape::Text(t) = &mut changed[0].4.shape else {
                panic!("text")
            };
            match change {
                0 => t.pos.x += 1.,
                1 => t.angle = 0.1,
                2 => t.opacity_factor = 0.5,
                3 => t.underline.width = -0.,
                4 => t.underline.color = egui::Color32::RED,
                5 => t.fallback_color = egui::Color32::RED,
                6 => t.override_text_color = Some(egui::Color32::WHITE),
                _ => t.galley = Arc::new((*t.galley).clone()),
            };
            assert!(!key.matches_parts(&owner, 0, environment(), o, &changed));
        }
    }
    #[test]
    fn unsupported_shape_and_actual_key_capacity_whole_decline() {
        let (ctx, mut s) = fixture();
        let owner = Arc::new(());
        let o = options(ctx.tessellation_options(|o| *o));
        let key = Key::capture_parts(&owner, 0, environment(), o, &s).unwrap();
        assert!(key.charge <= KEY_CAP);
        s[0].4.shape = egui::Shape::Noop;
        assert!(Key::capture_parts(&owner, 0, environment(), o, &s).is_none());
        let (_, shape) = fixture();
        let over = vec![shape[0].clone(); KEY_CAP / std::mem::size_of::<Slot>() + 1];
        assert!(Key::capture_parts(&owner, 0, environment(), o, &over).is_none());
    }
    #[test]
    fn default_off_and_clear_do_not_retain_producer_or_font_owner() {
        for flag in [None, Some("0".as_ref()), Some("invalid".as_ref())] {
            let c = Cache::new(flag);
            assert!(!c.enabled());
            assert!(c.key.is_none());
            assert_eq!(c.cpu, 0);
        }
        let (ctx, s) = fixture();
        let owner = Arc::new(());
        let weak = Arc::downgrade(&owner);
        let mut c = Cache::new(Some("1".as_ref()));
        c.key = Key::capture_parts(
            &owner,
            0,
            environment(),
            options(ctx.tessellation_options(|o| *o)),
            &s,
        );
        drop(owner);
        assert!(weak.upgrade().is_none());
        c.clear();
        assert!(c.key.is_none());
        assert!(c.meshes.is_empty());
    }
    #[test]
    fn complete_lookup_trace_retains_clipped_jobs_and_never_caches_prefix() {
        let mut trace = Some(Vec::new());
        record_lookup(&mut trace, egui::TextureId::Managed(0)); // original job later clipped
        record_lookup(&mut trace, egui::TextureId::Managed(0)); // visible job
        assert_eq!(trace.as_ref().unwrap().len(), 2);
        for _ in 2..16_384 {
            record_lookup(&mut trace, egui::TextureId::Managed(0));
        }
        assert!(trace.is_some());
        record_lookup(&mut trace, egui::TextureId::Managed(0));
        assert!(trace.is_none()); // Full original execution, no cached truncated lookup sequence.
    }
}
