//! Resources may be reused ONLY within one synchronous renderer invocation.
//! Mutable parent permission is not an input to immutable coverage selection.
use ferrite_render::{FlatProjection, PreparedCoverage, RenderContext};
use std::sync::Arc;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    owner: usize,
    revision: u64,
    view_revision: u64,
    count: usize,
    projection: FlatProjection,
    view_bits: [u64; 16],
    extent: [u32; 2],
    wrap: usize,
    assets: [usize; 2],
    pc_revision: u64,
    format: wgpu::TextureFormat,
    samples: u32,
}
impl Key {
    #[expect(
        clippy::too_many_arguments,
        reason = "Exact independent view, coverage, device and borrowed resource inputs for invocation-only readiness"
    )]
    pub(crate) fn new(
        context: &RenderContext,
        owner: Option<&Arc<PreparedCoverage>>,
        extent: [u32; 2],
        dpi: f64,
        wrap: usize,
        assets: [usize; 2],
        pc_revision: u64,
        format: wgpu::TextureFormat,
        samples: u32,
    ) -> Self {
        let s = &context.scaler;
        let t = s.flat_transform();
        let v = s.viewport;
        let g = s.geo_bounds;
        Self {
            owner: owner.map_or(0, |p| Arc::as_ptr(p) as usize),
            revision: context.geometry_revision(),
            view_revision: context.coverage_view_revision(),
            count: context.instruction_count(),
            projection: t.projection,
            view_bits: [
                t.scale[0].to_bits(),
                t.scale[1].to_bits(),
                t.offset[0].to_bits(),
                t.offset[1].to_bits(),
                t.geographic_origin[0].to_bits(),
                t.geographic_origin[1].to_bits(),
                u64::from(v.x.to_bits()),
                u64::from(v.y.to_bits()),
                u64::from(v.width.to_bits()),
                u64::from(v.height.to_bits()),
                s.display_scale.to_bits(),
                dpi.to_bits(),
                g.min_x.to_bits(),
                g.min_y.to_bits(),
                g.max_x.to_bits(),
                g.max_y.to_bits(),
            ],
            extent,
            wrap,
            assets,
            pc_revision,
            format,
            samples,
        }
    }
}
pub(crate) fn enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Readiness {
    coverage: Option<usize>,
    annotation_storage: usize,
    annotation_count: usize,
}
impl Readiness {
    pub(crate) fn new(
        coverage: Option<&crate::coverage_gpu_frame::CoverageGpuFrame>,
        annotations: &[crate::overscale_annotation::OverscaleAnnotation],
    ) -> Self {
        Self {
            coverage: coverage.map(|f| f as *const _ as usize),
            annotation_storage: annotations.as_ptr() as usize,
            annotation_count: annotations.len(),
        }
    }
}
#[derive(Default)]
pub(crate) struct Invocation {
    ready: Option<(Key, Readiness)>,
    owner: Option<Arc<PreparedCoverage>>,
    temporal: Vec<bool>,
    resources: Option<crate::portrayal_resource_owners::TrialResourceIdentity>,
}
impl Invocation {
    pub(crate) fn matches(
        &self,
        key: &Key,
        temporal: &[bool],
        resources: Option<&crate::CellPortrayalResources>,
        readiness: Readiness,
    ) -> bool {
        self.ready
            .as_ref()
            .is_some_and(|(k, r)| k == key && *r == readiness)
            && self.temporal == temporal
            && match (resources, self.resources.as_ref()) {
                (None, None) => true,
                (Some(current), Some(previous)) => current.matches_trial_identity(previous),
                _ => false,
            }
    }
    pub(crate) fn invalidate(&mut self) {
        self.ready = None;
        self.owner = None;
        self.temporal.clear();
        self.resources = None;
    }
    pub(crate) fn complete(
        &mut self,
        key: Key,
        owner: Option<Arc<PreparedCoverage>>,
        temporal: &[bool],
        resources: Option<&crate::CellPortrayalResources>,
        readiness: Readiness,
    ) {
        // An unsealed/incomplete registry can never authorize readiness.
        self.resources = resources.and_then(crate::CellPortrayalResources::trial_identity);
        if resources.is_some() && self.resources.is_none() {
            self.invalidate();
            return;
        }
        self.owner = owner;
        self.temporal.clear();
        self.temporal.extend_from_slice(temporal);
        self.ready = Some((key, readiness));
    }
}
#[derive(Default)]
pub(crate) struct Work {
    pub invocations: u64,
    pub trials: u64,
    pub reuse_hits: u64,
    pub preparations: u64,
    pub plans: u64,
    pub uploads: u64,
    pub unique_mask_bytes: u64,
    pub annotations_ready: u64,
}
impl Work {
    pub(crate) fn snapshot(&self, enabled: bool) -> serde_json::Value {
        serde_json::json!({"enabled":enabled,"invocations":self.invocations,"trials":self.trials,"reuse_hits":self.reuse_hits,"preparations":self.preparations,"plans":self.plans,"uploads":self.uploads,"unique_mask_bytes":self.unique_mask_bytes,"annotations_ready":self.annotations_ready,"scope":"synchronous invocation resource preparation counts, HOST attempts; no GPU time or execution decision cache"})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn key() -> Key {
        Key {
            owner: 0,
            revision: 2,
            view_revision: 3,
            count: 4,
            projection: FlatProjection::LocalGeographic,
            view_bits: [0; 16],
            extent: [32, 32],
            wrap: 3,
            assets: [8, 9],
            pc_revision: 10,
            format: wgpu::TextureFormat::Bgra8Unorm,
            samples: 4,
        }
    }
    fn ready() -> Readiness {
        Readiness {
            coverage: None,
            annotation_storage: 1,
            annotation_count: 0,
        }
    }
    #[test]
    fn successful_empty_resources_and_next_invocation_cold() {
        let mut i = Invocation::default();
        let k = key();
        assert!(!i.matches(&k, &[true, false], None, ready()));
        i.complete(k.clone(), None, &[true, false], None, ready());
        assert!(i.matches(&k, &[true, false], None, ready()));
        assert!(!Invocation::default().matches(&k, &[true, false], None, ready()));
        i.invalidate();
        assert!(!i.matches(&k, &[true, false], None, ready()));
    }
    #[test]
    fn owner_view_device_palette_revision_and_wrap_changes_decline() {
        let mut i = Invocation::default();
        let k = key();
        i.complete(k.clone(), None, &[true], None, ready());
        let mut changes = Vec::new();
        let mut x = k.clone();
        x.owner += 1;
        changes.push(x);
        let mut x = k.clone();
        x.revision += 1;
        changes.push(x);
        let mut x = k.clone();
        x.view_revision += 1;
        changes.push(x);
        let mut x = k.clone();
        x.view_bits[11] += 1;
        changes.push(x);
        let mut x = k.clone();
        x.extent[0] += 1;
        changes.push(x);
        let mut x = k.clone();
        x.wrap = 1;
        changes.push(x);
        let mut x = k.clone();
        x.assets[0] += 1;
        changes.push(x);
        let mut x = k.clone();
        x.pc_revision += 1;
        changes.push(x);
        let mut x = k.clone();
        x.samples = 1;
        changes.push(x);
        let mut x = k.clone();
        x.format = wgpu::TextureFormat::Rgba8Unorm;
        changes.push(x);
        assert!(changes
            .iter()
            .all(|x| !i.matches(x, &[true], None, ready())));
    }
    #[test]
    fn temporal_changes_and_partial_annotation_list_decline() {
        let mut i = Invocation::default();
        let k = key();
        i.complete(k.clone(), None, &[true, false], None, ready());
        assert!(!i.matches(&k, &[false, true], None, ready()));
        assert!(!i.matches(
            &k,
            &[true, false],
            None,
            Readiness {
                annotation_count: 1,
                ..ready()
            }
        ));
        assert!(!i.matches(
            &k,
            &[true, false],
            None,
            Readiness {
                annotation_storage: 2,
                ..ready()
            }
        ));
    }
    #[test]
    fn startup_policy_exact_one_only() {
        assert!(enabled(Some(std::ffi::OsStr::new("1"))));
        for value in [
            None,
            Some(std::ffi::OsStr::new("0")),
            Some(std::ffi::OsStr::new("true")),
        ] {
            assert!(!enabled(value));
        }
    }
}
