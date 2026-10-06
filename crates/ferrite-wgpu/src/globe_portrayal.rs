//! Product-neutral draping of geographic drawing geometry onto WGS84.
//! Triangulation uses ellipsoidal Mercator: straight edges retain S-101
//! loxodromic interpolation, including dateline lifts and polygon holes.
use crate::globe_scene::{GlobeMesh, GlobeVertex};
use ferrite_kernel::{
    geodesy::{GeographicPosition, Mercator, WGS84_A},
    globe_camera::GlobeCamera,
};
use ferrite_render::{AreaFillType, AreaInstruction, WorldPoint};
use std::collections::HashMap;
#[derive(Debug, Clone, Copy)]
pub struct DrapingLimits {
    pub screen_error_px: f64,
    pub chord_error_m: f64,
    pub max_vertices: usize,
    pub frustum_culling: bool,
}
impl Default for DrapingLimits {
    fn default() -> Self {
        Self {
            screen_error_px: 0.25,
            chord_error_m: 5.,
            max_vertices: 262144,
            frustum_culling: true,
        }
    }
}
#[derive(Debug, Default, Clone)]
pub struct DrapingStats {
    pub source_vertices: usize,
    pub frustum_culled: bool,
    pub source_triangles: usize,
    pub refinements: usize,
    pub midpoint_ecef_reused: usize,
    pub deferred_screen_checks: usize,
    pub resolved_screen_checks: usize,
    pub final_triangles: usize,
    pub max_edge_error_px: f64,
    pub max_edge_chord_error_m: f64,
}
pub(crate) fn ecef(uv: [f64; 2]) -> Result<[f64; 3], String> {
    let longitude = ((uv[0] / WGS84_A).to_degrees() + 180.).rem_euclid(360.) - 180.;
    Mercator::World
        .unproject([WGS84_A * longitude.to_radians(), uv[1]])
        .and_then(|p| p.to_ecef(0.))
        .map_err(|e| e.to_string())
}
fn screen(camera: &GlobeCamera, p: [f64; 3]) -> Result<Option<[f64; 2]>, String> {
    let c = camera.clip_ecef(p).map_err(|e| e.to_string())?;
    if c[3] <= 0. {
        return Ok(None);
    }
    let v = camera.viewport();
    Ok(Some([
        (c[0] / c[3] + 1.) * v[0] / 2.,
        (1. - c[1] / c[3]) * v[1] / 2.,
    ]))
}
fn deviation(
    camera: &GlobeCamera,
    actual: [f64; 3],
    linear: [f64; 3],
) -> Result<(f64, f64), String> {
    let chord = (0..3)
        .map(|i| (actual[i] - linear[i]).powi(2))
        .sum::<f64>()
        .sqrt();
    deviation_with_chord(camera, actual, linear, chord)
}
fn deviation_with_chord(
    camera: &GlobeCamera,
    actual: [f64; 3],
    linear: [f64; 3],
    chord: f64,
) -> Result<(f64, f64), String> {
    // The true point and its linear approximation both lie in this sphere.
    // A separating clip plane proves their displacement cannot affect the view;
    // there is no reason to divide by a vanishing/negative eye-space depth there.
    let px = if camera
        .sphere_outside_frustum(actual, chord, 0.)
        .map_err(|e| e.to_string())?
    {
        0.
    } else {
        match (screen(camera, actual)?, screen(camera, linear)?) {
            (Some(a), Some(b)) => (a[0] - b[0]).hypot(a[1] - b[1]),
            // A potentially visible displacement crossing the eye still needs
            // subdivision. Do not count it as a zero-error approximation.
            _ => f64::INFINITY,
        }
    };
    Ok((px, chord))
}
// A decision transcript proves that a warm build takes the exact same branches
// as cold adaptive subdivision. Probes with a failing world chord need no camera
// check: their refinement decision is already independent of perspective.
#[derive(Clone)]
struct DecisionProbe {
    actual: [f64; 3],
    linear: [f64; 3],
    exceeds_screen: bool,
}
// Internal adaptive-only sentinel. A chord above the quality bound proves
// refinement independently of pixels. Final error reads resolve this marker.
const DEFERRED_SCREEN: f64 = -1.;
fn chord_precheck_view_is_safe(camera: &GlobeCamera) -> bool {
    let frame = camera.projection_frame();
    // Draped actual/linear points stay within the WGS84 radius. These generous
    // bounds keep clip arithmetic finite even for otherwise accepted extreme
    // camera inputs; unsupported view scales retain all reference evaluations.
    frame.eye_m.iter().all(|v| v.is_finite() && v.abs() <= 1e12)
        && [frame.right, frame.up, frame.forward]
            .iter()
            .flatten()
            .all(|v| v.is_finite() && v.abs() <= 2.)
        && frame
            .divisors
            .iter()
            .all(|v| v.is_finite() && (1e-12..=1e12).contains(v))
        && frame.depth.iter().all(|v| v.is_finite() && v.abs() <= 1e12)
        && camera
            .depth_range_m()
            .iter()
            .all(|v| v.is_finite() && v.abs() <= 1e12)
}
struct Evaluation<'a> {
    camera: &'a GlobeCamera,
    limits: DrapingLimits,
    probes: Vec<DecisionProbe>,
    passing: PassingCertificate,
    record: bool,
    bounded_probe_capacity: Option<usize>,
    chord_precheck: bool,
    deferred_screen_checks: usize,
    resolved_screen_checks: usize,
}
impl Evaluation<'_> {
    fn check_adaptive(&mut self, actual: [f64; 3], linear: [f64; 3]) -> Result<(f64, f64), String> {
        if !self.chord_precheck {
            return self.check(actual, linear);
        }
        // Same expression/order as deviation; compute it exactly once.
        let chord = (0..3)
            .map(|i| (actual[i] - linear[i]).powi(2))
            .sum::<f64>()
            .sqrt();
        if chord > self.limits.chord_error_m {
            self.deferred_screen_checks += 1;
            return Ok((DEFERRED_SCREEN, chord));
        }
        let error = deviation_with_chord(self.camera, actual, linear, chord)?;
        self.record_error(actual, linear, error);
        Ok(error)
    }

    fn check(&mut self, actual: [f64; 3], linear: [f64; 3]) -> Result<(f64, f64), String> {
        let error = deviation(self.camera, actual, linear)?;
        self.record_error(actual, linear, error);
        Ok(error)
    }
    fn record_error(&mut self, actual: [f64; 3], linear: [f64; 3], error: (f64, f64)) {
        if self.record && error.1 <= self.limits.chord_error_m {
            if error.0 > self.limits.screen_error_px {
                if self.bounded_probe_capacity.is_some_and(|n|self.probes.len()>=n) {
                    self.record=false;self.probes=Vec::new();
                    self.passing=PassingCertificate::new();
                    return;
                }
                self.probes.push(DecisionProbe {
                    actual,
                    linear,
                    exceeds_screen: true,
                });
            } else {
                self.passing.include(actual, linear);
            }
        }
    }
}
// One conservative perspective certificate replaces all passing probes.
// Both endpoints of every displacement lie in this ECEF box. Positive depth
// and the quotient derivative bound prove that their pixel separation stays
// below the original threshold. An inconclusive bound always causes a cold build.
const RETAINED_PASSING_AGGREGATE_LIMIT:usize=32*1024*1024;
static RETAINED_PASSING_PEAK:std::sync::atomic::AtomicUsize=std::sync::atomic::AtomicUsize::new(0);
static RETAINED_PASSING_COUNTERS:[std::sync::atomic::AtomicU64;4]=[const {std::sync::atomic::AtomicU64::new(0)};4];
pub(crate) fn retained_passing_diagnostics()->([u64;4],usize,usize) {
    use std::sync::atomic::Ordering::Relaxed;
    (std::array::from_fn(|i|RETAINED_PASSING_COUNTERS[i].load(Relaxed)),RETAINED_PASSING_RESERVED.load(Relaxed),RETAINED_PASSING_PEAK.load(Relaxed))
}
static RETAINED_PASSING_RESERVED:std::sync::atomic::AtomicUsize=std::sync::atomic::AtomicUsize::new(0);
fn passing_reservation_total(current:usize,additional:usize)->Option<usize> {
    current.checked_add(additional).filter(|n|*n<=RETAINED_PASSING_AGGREGATE_LIMIT)
}
fn reserve_passing_bytes(additional:usize)->bool {
    use std::sync::atomic::Ordering::Relaxed;
    match RETAINED_PASSING_RESERVED.fetch_update(Relaxed,Relaxed,|old|passing_reservation_total(old,additional)) {Ok(old)=>{RETAINED_PASSING_PEAK.fetch_max(old+additional,Relaxed);true},Err(_)=>false}
}
struct PassingCertificate {
    exact_pair_reserved_bytes:usize,
    exact_pairs: Option<Vec<([f64;3],[f64;3])>>,
    exact_pair_limit_bytes: usize,
    bounds: [[f64; 3]; 2],
    chord: f64,
}
impl PassingCertificate {
    fn new() -> Self { Self::new_retained(false) }
    fn new_retained(enabled:bool)->Self {
        Self {
            exact_pair_reserved_bytes:0,
            exact_pairs: enabled.then(Vec::new),
            exact_pair_limit_bytes: 2*1024*1024,
            bounds: [[f64::INFINITY; 3], [f64::NEG_INFINITY; 3]],
            chord: 0.,
        }
    }
    fn include(&mut self, actual: [f64; 3], linear: [f64; 3]) {
        // Globally reserve optional recorded-pair capacity BEFORE allocation; refusals
        // release the entire optional proof and leave the original box/cold fallback.
        let item=std::mem::size_of::<([f64;3],[f64;3])>();
        if let Some(pairs)=self.exact_pairs.as_ref() {
            if pairs.len()==pairs.capacity() {
                let maximum=self.exact_pair_limit_bytes/item;
                let next=pairs.capacity().saturating_mul(2).max(4).min(maximum);
                let delta=next.saturating_sub(pairs.capacity())*item;
                if next<=pairs.capacity() || !reserve_passing_bytes(delta) {self.disable_exact_pairs();}
                else {
                    self.exact_pair_reserved_bytes+=delta;
                    let pairs=self.exact_pairs.as_mut().unwrap();
                    let fail=pairs.try_reserve_exact(next-pairs.len()).is_err();
                    let actual=pairs.capacity()*item;
                    if fail || actual>self.exact_pair_limit_bytes {self.disable_exact_pairs();}
                    else if actual>self.exact_pair_reserved_bytes {
                        let extra=actual-self.exact_pair_reserved_bytes;
                        if reserve_passing_bytes(extra) {self.exact_pair_reserved_bytes+=extra;}else{self.disable_exact_pairs();}
                    }
                }
            }
            if let Some(pairs)=self.exact_pairs.as_mut() {pairs.push((actual,linear));}
        }
        self.chord = self.chord.max(
            (actual[0] - linear[0])
                .hypot(actual[1] - linear[1])
                .hypot(actual[2] - linear[2]),
        );
        for point in [actual, linear] {
            for axis in 0..3 {
                self.bounds[0][axis] = self.bounds[0][axis].min(point[axis]);
                self.bounds[1][axis] = self.bounds[1][axis].max(point[axis]);
            }
        }
    }
    fn disable_exact_pairs(&mut self) {
        if self.exact_pairs.is_some() {RETAINED_PASSING_COUNTERS[3].fetch_add(1,std::sync::atomic::Ordering::Relaxed);}
        self.exact_pairs=None;
        if self.exact_pair_reserved_bytes>0 {RETAINED_PASSING_RESERVED.fetch_sub(self.exact_pair_reserved_bytes,std::sync::atomic::Ordering::Relaxed);}
        self.exact_pair_reserved_bytes=0;
    }
    fn accepts(&self, camera: &GlobeCamera, threshold:f64)->Result<bool,String> {
        if self.accepts_box(camera,threshold)? { return Ok(true); }
        let Some(pairs)=self.exact_pairs.as_ref() else { return Ok(false); };
        use std::sync::atomic::Ordering::Relaxed;
        RETAINED_PASSING_COUNTERS[0].fetch_add(1,Relaxed);
        let mut tested=0u64;
        for &(actual,linear) in pairs {
            tested+=1;
            match deviation(camera,actual,linear) { Ok(error) if error.0<=threshold=>{},_=>{RETAINED_PASSING_COUNTERS[2].fetch_add(tested,Relaxed);return Ok(false);} }
        }
        RETAINED_PASSING_COUNTERS[2].fetch_add(tested,Relaxed);
        RETAINED_PASSING_COUNTERS[1].fetch_add(1,Relaxed);
        Ok(true)
    }
    fn accepts_box(&self, camera: &GlobeCamera, threshold: f64) -> Result<bool, String> {
        if self.chord == 0. {
            return Ok(true);
        }
        let mut z = f64::INFINITY;
        let mut extent = [0_f64; 2];
        for i in 0..8 {
            let p = std::array::from_fn(|axis| self.bounds[(i >> axis) & 1][axis]);
            let clip = camera.clip_ecef(p).map_err(|e| e.to_string())?;
            z = z.min(clip[3]);
            for axis in 0..2 {
                extent[axis] = extent[axis].max(clip[axis].abs());
            }
        }
        // Inflate floating-point bounds before dividing, including a generous
        // margin on the numerically recovered linear projection row norms.
        z -= 1e-5;
        if z <= 0. {
            return Ok(false);
        }
        let origin = self.bounds[0];
        let c = camera.clip_ecef(origin).map_err(|e| e.to_string())?;
        let mut rows = [[0_f64; 3]; 3];
        for axis in 0..3 {
            let mut p = origin;
            p[axis] += 1.;
            let q = camera.clip_ecef(p).map_err(|e| e.to_string())?;
            for (row, index) in [0, 1, 3].into_iter().enumerate() {
                rows[row][axis] = q[index] - c[index];
            }
        }
        let norms: [f64; 3] = rows.map(|r| r[0].hypot(r[1]).hypot(r[2]) * (1. + 1e-6) + 1e-6);
        let viewport = camera.viewport();
        let displacement = std::array::from_fn::<_, 2, _>(|axis| {
            viewport[axis] / 2.
                * (self.chord * (1. + 1e-6) + 1e-6)
                * (norms[axis] / z + (extent[axis] + 1e-5) * norms[2] / (z * z))
        });
        Ok(displacement[0].hypot(displacement[1]) < threshold)
    }
}
impl Drop for PassingCertificate {
    fn drop(&mut self) {if self.exact_pair_reserved_bytes>0 {RETAINED_PASSING_RESERVED.fetch_sub(self.exact_pair_reserved_bytes,std::sync::atomic::Ordering::Relaxed);}}
}
// Ephemeral per-frame counters. Nested parallel task durations are sums,
// not elapsed frame time; no per-vertex atomic updates are performed.
#[derive(Default)]
pub(crate) struct AreaProfile {
    values: [std::sync::atomic::AtomicU64; 17],
    midpoint_ecef_reused: std::sync::atomic::AtomicU64,
    midpoint_vertices_created: std::sync::atomic::AtomicU64,
    deferred_screen_checks: std::sync::atomic::AtomicU64,
    resolved_screen_checks: std::sync::atomic::AtomicU64,
}
impl AreaProfile {
    fn add(&self, index: usize, value: u64) {
        self.values[index].fetch_add(value, std::sync::atomic::Ordering::Relaxed);
    }
    pub(crate) fn diagnostics(&self) -> serde_json::Value {
        let labels = [
            "area_reuse_calls",
            "area_reuse_culled",
            "area_reuse_certificate_rejected",
            "area_reuse_probe_rejected",
            "area_reuse_accepted",
            "area_reuse_probes_tested",
            "area_cold_calls",
            "area_reuse_preflight_task_sum_ms",
            "area_reuse_certificate_task_sum_ms",
            "area_reuse_probes_task_sum_ms",
            "area_cold_task_sum_ms",
            "area_source_uv_task_sum_ms",
            "area_source_triangulation_task_sum_ms",
            "area_source_bound_task_sum_ms",
            "area_source_ecef_task_sum_ms",
            "area_subdivision_task_sum_ms",
            "area_cache_record_task_sum_ms",
        ];
        let mut result = serde_json::Map::new();
        for (i, label) in labels.into_iter().enumerate() {
            let n = self.values[i].load(std::sync::atomic::Ordering::Relaxed);
            result.insert(
                label.into(),
                if i < 7 {
                    serde_json::json!(n)
                } else {
                    serde_json::json!(n as f64 / 1e6)
                },
            );
        }
        result.insert(
            "area_midpoint_ecef_reused".into(),
            serde_json::json!(self
                .midpoint_ecef_reused
                .load(std::sync::atomic::Ordering::Relaxed)),
        );
        result.insert(
            "area_midpoint_vertices_created".into(),
            serde_json::json!(self
                .midpoint_vertices_created
                .load(std::sync::atomic::Ordering::Relaxed)),
        );
        result.insert(
            "area_deferred_screen_checks".into(),
            serde_json::json!(self
                .deferred_screen_checks
                .load(std::sync::atomic::Ordering::Relaxed)),
        );
        result.insert(
            "area_resolved_screen_checks".into(),
            serde_json::json!(self
                .resolved_screen_checks
                .load(std::sync::atomic::Ordering::Relaxed)),
        );
        serde_json::Value::Object(result)
    }
}
struct AreaSpan<'a> {
    profile: Option<&'a AreaProfile>,
    index: usize,
    start: Option<std::time::Instant>,
}
impl<'a> AreaSpan<'a> {
    fn new(profile: Option<&'a AreaProfile>, index: usize) -> Self {
        Self {
            profile,
            index,
            start: profile.map(|_| std::time::Instant::now()),
        }
    }
}
impl Drop for AreaSpan<'_> {
    fn drop(&mut self) {
        if let (Some(profile), Some(start)) = (self.profile, self.start) {
            profile.add(
                self.index,
                start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            );
        }
    }
}
fn count(profile: Option<&AreaProfile>, index: usize, value: u64) {
    if let Some(profile) = profile {
        profile.add(index, value);
    }
}
// Immutable source data, keyed by (RenderContext geometry revision, instruction
// index). Camera-dependent subdivisions remain reference computations.
pub(crate) struct AreaSource {
    uv: Box<[[f64; 2]]>,
    triangles: Box<[usize]>,
    ecef: Box<[[f64; 3]]>,
    bound: Option<([f64; 3], f64)>,
}
impl AreaSource {
    fn bytes(&self) -> usize {
        Self::required_bytes(self.uv.len(), self.triangles.len(), self.ecef.len())
    }
    fn required_bytes(uv: usize, triangles: usize, ecef: usize) -> usize {
        // Box slices have no spare capacity. Include Arc allocation headers and
        // a conservative 256-byte allowance for each pending capture/slot and
        // the staging Vec capacity growth; this is not a process RSS bound.
        let base = std::mem::size_of::<Self>()
            + 2 * std::mem::size_of::<usize>()
            + 256;
        base.checked_add(uv.checked_mul(std::mem::size_of::<[f64; 2]>()).unwrap_or(usize::MAX))
            .and_then(|n| n.checked_add(triangles.checked_mul(std::mem::size_of::<usize>())?))
            .and_then(|n| n.checked_add(ecef.checked_mul(std::mem::size_of::<[f64; 3]>())?))
            .unwrap_or(usize::MAX)
    }
    fn capture(
        uv: &[[f64; 2]],
        triangles: &[usize],
        bound: Option<([f64; 3], f64)>,
        vertices: &[GlobeVertex],
        budget: Option<&std::sync::Arc<AreaSourceBudget>>,
    ) -> Option<AreaSourceCapture> {
        let budget = budget?;
        let vertices = if budget.topology_only { &[][..] } else { vertices };
        let bytes = Self::required_bytes(uv.len(), triangles.len(), vertices.len());
        let reservation = budget.reserve(bytes)?;
        let source = std::sync::Arc::new(Self {
            uv: uv.to_vec().into_boxed_slice(),
            triangles: triangles.to_vec().into_boxed_slice(),
            ecef: vertices
                .iter()
                .map(|v| v.ecef_m)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            bound,
        });
        debug_assert_eq!(source.bytes(), bytes);
        Some(AreaSourceCapture {
            source,
            reservation,
        })
    }
}
pub(crate) struct AreaSourceCapture {
    source: std::sync::Arc<AreaSource>,
    reservation: SourceReservation,
}
struct SourceReservation {
    budget: std::sync::Arc<AreaSourceBudget>,
    bytes: usize,
}
impl Drop for SourceReservation {
    fn drop(&mut self) {
        self.budget
            .used
            .fetch_sub(self.bytes, std::sync::atomic::Ordering::Relaxed);
    }
}
pub(crate) struct AreaSourceBudget {
    topology_only: bool,
    base: usize,
    oversized: std::sync::atomic::AtomicUsize,
    used: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    denied: std::sync::atomic::AtomicUsize,
    captures: std::sync::atomic::AtomicUsize,
    hits: std::sync::atomic::AtomicUsize,
}
impl AreaSourceBudget {
    pub(crate) const LIMIT: usize = 16 * 1024 * 1024;
    fn reserve(self: &std::sync::Arc<Self>, bytes: usize) -> Option<SourceReservation> {
        use std::sync::atomic::Ordering::Relaxed;
        let result = self.used.fetch_update(Relaxed, Relaxed, |old| {
            old.checked_add(bytes).filter(|n| *n <= Self::LIMIT)
        });
        match result {
            Ok(old) => {
                self.peak.fetch_max(old + bytes, Relaxed);
                self.captures.fetch_add(1, Relaxed);
                Some(SourceReservation {
                    budget: self.clone(),
                    bytes,
                })
            }
            Err(_) => {
                if bytes <= Self::LIMIT - self.base {
                    self.denied.fetch_max(bytes, Relaxed);
                } else {
                    self.oversized.fetch_add(1, Relaxed);
                }
                None
            }
        }
    }
}
struct SourceCacheEntry {
    source: std::sync::Arc<AreaSource>,
    last_used: u64,
}
#[derive(Default)]
pub(crate) struct AreaSourceCache {
    topology_only: bool,
    slots: Vec<Option<SourceCacheEntry>>,
    revision: Option<u64>,
    frame: u64,
    bytes: usize,
    requested: usize,
    evictions: usize,
}
impl AreaSourceCache {
    pub(crate) fn set_topology_only(&mut self, enabled: bool) {
        if self.topology_only != enabled { self.slots=Vec::new(); self.bytes=0; self.requested=0; self.revision=None; self.topology_only=enabled; }
    }

    fn base_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.slots.capacity() * std::mem::size_of::<Option<SourceCacheEntry>>()
            + std::mem::size_of::<AreaSourceBudget>()
            + 2 * std::mem::size_of::<usize>()
            + 256
    }
    pub(crate) fn begin(
        &mut self,
        enabled: bool,
        revision: u64,
        count: usize,
        potential: &[bool],
        spatial: &[bool],
    ) -> Option<std::sync::Arc<AreaSourceBudget>> {
        if !enabled || self.revision != Some(revision) || self.slots.len() != count {
            self.slots = Vec::new();
            self.bytes = 0;
            self.requested = 0;
            self.revision = Some(revision);
        }
        if !enabled {
            return None;
        }
        if self.slots.len() != count {
            let required = count
                .checked_mul(std::mem::size_of::<Option<SourceCacheEntry>>())?
                .checked_add(self.base_bytes())?;
            if required > AreaSourceBudget::LIMIT {
                return None;
            }
            self.slots = std::iter::repeat_with(|| None).take(count).collect();
            self.bytes = self.base_bytes();
            if self.bytes > AreaSourceBudget::LIMIT {
                self.slots = Vec::new();
                self.bytes = 0;
                return None;
            }
        }
        if count == 0 {
            self.bytes = self.base_bytes();
        }
        self.frame = self.frame.saturating_add(1);
        // Demand-driven LRU eviction. Prefer entries outside the current
        // candidate set, then old frames, then source index for a stable tie.
        if self.requested <= AreaSourceBudget::LIMIT.saturating_sub(self.base_bytes()) {
            while AreaSourceBudget::LIMIT - self.bytes < self.requested {
                let victim = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter_map(|(i, e)| {
                        e.as_ref()
                            .map(|e| ((potential[i] && spatial[i], e.last_used, i), i))
                    })
                    .min_by_key(|(key, _)| *key)
                    .map(|(_, i)| i);
                let Some(i) = victim else { break };
                let old = self.slots[i].take().expect("source eviction entry");
                self.bytes -= old.source.bytes();
                self.evictions += 1;
            }
        }
        self.requested = 0;
        for (i, e) in self.slots.iter_mut().enumerate() {
            if potential[i] && spatial[i] {
                if let Some(e) = e {
                    e.last_used = self.frame;
                }
            }
        }
        Some(std::sync::Arc::new(AreaSourceBudget {
            topology_only:self.topology_only,
            base: self.base_bytes(),
            oversized: std::sync::atomic::AtomicUsize::new(0),
            used: std::sync::atomic::AtomicUsize::new(self.bytes),
            peak: std::sync::atomic::AtomicUsize::new(self.bytes),
            denied: std::sync::atomic::AtomicUsize::new(0),
            captures: std::sync::atomic::AtomicUsize::new(0),
            hits: std::sync::atomic::AtomicUsize::new(0),
        }))
    }
    pub(crate) fn get(&self, index: usize) -> Option<&AreaSource> {
        self.slots.get(index)?.as_ref().map(|e| e.source.as_ref())
    }
    pub(crate) fn commit(&mut self, index: usize, capture: AreaSourceCapture) {
        let AreaSourceCapture {
            source,
            reservation,
        } = capture;
        let slot = self.slots.get_mut(index).expect("captured source slot");
        if let Some(old) = slot.take() {
            self.bytes -= old.source.bytes();
        }
        self.bytes += source.bytes();
        *slot = Some(SourceCacheEntry {
            source,
            last_used: self.frame,
        });
        drop(reservation);
        debug_assert!(self.bytes <= AreaSourceBudget::LIMIT);
    }
    pub(crate) fn finish(&mut self, budget: Option<&AreaSourceBudget>) -> serde_json::Value {
        use std::sync::atomic::Ordering::Relaxed;
        if let Some(b) = budget {
            self.requested = b.denied.load(Relaxed);
        }
        serde_json::json!({"area_source_cache_topology_only":self.topology_only,"area_source_cache_enabled":budget.is_some(),
            "area_source_cache_revision":self.revision,
            "area_source_cache_bytes":self.bytes,
            "area_source_cache_budget_bytes":AreaSourceBudget::LIMIT,
            "area_source_cache_entries":self.slots.iter().filter(|s|s.is_some()).count(),
            "area_source_cache_evictions":self.evictions,
            "area_source_cache_peak_with_pending_bytes":budget.map_or(0,|b|b.peak.load(Relaxed)),
            "area_source_cache_rejected_max_bytes":self.requested,
            "area_source_cache_oversized":budget.map_or(0,|b|b.oversized.load(Relaxed)),
            "area_source_cache_captures":budget.map_or(0,|b|b.captures.load(Relaxed)),
            "area_source_cache_hits":budget.map_or(0,|b|b.hits.load(Relaxed))})
    }
}

pub(crate) struct CachedArea {
    mesh: std::sync::Arc<GlobeMesh>,
    all_vertices_used: bool,
    limits: DrapingLimits,
    probes: Vec<DecisionProbe>,
    passing: PassingCertificate,
    bound: ([f64; 3], f64),
}
impl CachedArea {
    pub(crate) fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + std::mem::size_of::<GlobeMesh>()
            + 2 * std::mem::size_of::<usize>()
            + self.mesh.vertices.capacity() * std::mem::size_of::<GlobeVertex>()
            + self.mesh.indices.capacity() * 4
            + self.probes.capacity() * std::mem::size_of::<DecisionProbe>()
            + self.passing.exact_pairs.as_ref().map_or(0,|p|p.capacity()*std::mem::size_of::<([f64;3],[f64;3])>())
    }
    pub(crate) fn reusable_mesh(
        &self,
        camera: &GlobeCamera,
        color: [f32; 4],
    ) -> Result<Option<&GlobeMesh>, String> {
        self.reusable_mesh_profiled(camera, color, None)
    }
    pub(crate) fn reusable_mesh_profiled(
        &self,
        camera: &GlobeCamera,
        color: [f32; 4],
        profile: Option<&AreaProfile>,
    ) -> Result<Option<&GlobeMesh>, String> {
        count(profile, 0, 1);
        let preflight = AreaSpan::new(profile, 7);
        if !color
            .iter()
            .all(|x| x.is_finite() && (0. ..=1.).contains(x))
        {
            return Err("Invalid globe fill color".into());
        }
        if color[3] == 0.
            || (self.limits.frustum_culling
                && camera
                    .sphere_outside_frustum(self.bound.0, self.bound.1, 2.)
                    .map_err(|e| e.to_string())?)
        {
            count(profile, 1, 1);
            return Ok(None);
        }
        drop(preflight);
        let certificate = AreaSpan::new(profile, 8);
        if !self.passing.accepts(camera, self.limits.screen_error_px)? {
            count(profile, 2, 1);
            return Ok(None);
        }
        drop(certificate);
        let _probes = AreaSpan::new(profile, 9);
        let mut tested = 0;
        for p in &self.probes {
            tested += 1;
            let error = deviation(camera, p.actual, p.linear)?;
            if (error.0 > self.limits.screen_error_px) != p.exceeds_screen {
                count(profile, 5, tested);
                count(profile, 3, 1);
                return Ok(None);
            }
        }
        count(profile, 5, tested);
        count(profile, 4, 1);
        Ok(Some(&self.mesh))
    }
    pub(crate) fn spatial_bounds(&self) -> ferrite_kernel::spatial_hierarchy::SpatialBounds<3> {
        let (c, r) = self.bound;
        ferrite_kernel::spatial_hierarchy::SpatialBounds {
            min: c.map(|x| x - r),
            max: c.map(|x| x + r),
        }
    }
    pub(crate) fn mesh_identity(&self) -> usize {
        std::sync::Arc::as_ptr(&self.mesh) as usize
    }
    pub(crate) fn shares_mesh(&self, mesh: &std::sync::Arc<GlobeMesh>) -> bool {
        std::sync::Arc::ptr_eq(&self.mesh, mesh)
    }
    /// Called after the adaptive decision transcript accepted this camera.
    /// The cached surface sphere bounds every vertex and triangle chord. A
    /// strictly interior sphere permits sharing all geometry without clipping.
    pub(crate) fn shared_whole_mesh(
        &self,
        camera: &GlobeCamera,
        color: [f32; 4],
    ) -> Result<Option<std::sync::Arc<GlobeMesh>>, String> {
        if !self.all_vertices_used {
            return Ok(None);
        }
        if self.mesh.vertices.first().is_some_and(|v| v.color != color) {
            return Ok(None);
        }
        let distances = camera
            .frustum_distances(self.bound.0, 0.)
            .map_err(|e| e.to_string())?;
        let tolerance = 64. * f64::EPSILON * camera.depth_range_m()[1].max(self.bound.1);
        Ok(distances
            .iter()
            .all(|d| *d > self.bound.1 + tolerance)
            .then(|| self.mesh.clone()))
    }
    #[cfg(test)]
    fn reuse(&self, camera: &GlobeCamera, color: [f32; 4]) -> Result<Option<GlobeMesh>, String> {
        Ok(self.reusable_mesh(camera, color)?.map(|m| {
            let mut m = m.clone();
            for v in &mut m.vertices {
                v.color = color;
            }
            m
        }))
    }
}
fn key(a: u32, b: u32) -> (u32, u32) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}
pub(crate) fn ring(points: &[WorldPoint], meridian: f64) -> Result<Vec<[f64; 2]>, String> {
    let mut result = Vec::with_capacity(points.len());
    let mut longitude = meridian;
    let points = if points.len() > 1 && points.first() == points.last() {
        &points[..points.len() - 1]
    } else {
        points
    };
    if points.len() < 3 {
        return Err("Degenerate globe polygon ring".into());
    }
    for p in points {
        if !p.x.is_finite() || p.x.abs() > 1e9 {
            return Err("Invalid geographic longitude".into());
        }
        let canonical = (p.x + 180.).rem_euclid(360.) - 180.;
        let position = GeographicPosition::new(p.y, canonical).map_err(|e| e.to_string())?;
        longitude = position
            .longitude_near(longitude)
            .map_err(|e| e.to_string())?;
        let mut uv = Mercator::World
            .project(position)
            .map_err(|e| e.to_string())?;
        uv[0] = WGS84_A * longitude.to_radians();
        result.push(uv);
    }
    if (result.last().unwrap()[0] - result[0][0]).abs() > WGS84_A * std::f64::consts::PI {
        return Err("Polar/winding polygon requires a polar surface partition".into());
    }
    Ok(result)
}
fn edge_error(
    a: u32,
    b: u32,
    uv: &[[f64; 2]],
    vertices: &[GlobeVertex],
    evaluation: &mut Evaluation,
    errors: &mut HashMap<(u32, u32), (f64, f64)>,
) -> Result<(f64, f64), String> {
    edge_sample(a, b, uv, vertices, evaluation, errors, false, false).map(|v| v.0)
}
fn edge_sample(
    a: u32,
    b: u32,
    uv: &[[f64; 2]],
    vertices: &[GlobeVertex],
    evaluation: &mut Evaluation,
    errors: &mut HashMap<(u32, u32), (f64, f64)>,
    retain_sample: bool,
    adaptive: bool,
) -> Result<((f64, f64), Option<[f64; 3]>), String> {
    let k = key(a, b);
    if let Some(e) = errors.get(&k) {
        if adaptive || e.0 != DEFERRED_SCREEN {
            return Ok((*e, None));
        }
        // An exact error is requested for a final mesh edge. Never expose an
        // adaptive marker in final statistics, even for an unexpected edge.
        evaluation.resolved_screen_checks += 1;
    }
    let midpoint = std::array::from_fn(|i| (uv[a as usize][i] + uv[b as usize][i]) / 2.);
    let linear = std::array::from_fn(|i| {
        (vertices[a as usize].ecef_m[i] + vertices[b as usize].ecef_m[i]) / 2.
    });
    let actual = ecef(midpoint)?;
    let e = if adaptive {
        evaluation.check_adaptive(actual, linear)?
    } else {
        evaluation.check(actual, linear)?
    };
    errors.insert(k, e);
    Ok((e, retain_sample.then_some(actual)))
}
#[allow(clippy::too_many_arguments)]
fn red_split(
    t: [u32; 3],
    depth: u8,
    pending: &mut Vec<([u32; 3], u8)>,
    uv: &mut Vec<[f64; 2]>,
    vertices: &mut Vec<GlobeVertex>,
    midpoints: &mut HashMap<(u32, u32), u32>,
    stats: &mut DrapingStats,
    color: [f32; 4],
    budget: usize,
    samples: [Option<[f64; 3]>; 3],
) -> Result<(), String> {
    if depth >= 32 {
        return Err("Globe red refinement exceeded numeric resolution".into());
    }
    let mut mids = [0; 3];
    for (i, (a, b)) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])]
        .into_iter()
        .enumerate()
    {
        mids[i] = if let Some(m) = midpoints.get(&key(a, b)) {
            *m
        } else {
            if vertices.len() >= budget {
                return Err("Adaptive globe polygon vertex budget exceeded".into());
            }
            let p = std::array::from_fn(|j| (uv[a as usize][j] + uv[b as usize][j]) / 2.);
            if p == uv[a as usize] || p == uv[b as usize] {
                return Err("Globe midpoint cannot advance at f64 resolution".into());
            }
            let id = vertices.len() as u32;
            uv.push(p);
            vertices.push(GlobeVertex {
                ecef_m: if let Some(actual) = samples[i] {
                    stats.midpoint_ecef_reused += 1;
                    actual
                } else {
                    ecef(p)?
                },
                color,
            });
            midpoints.insert(key(a, b), id);
            stats.refinements += 1;
            id
        };
    }
    let [ab, bc, ca] = mids;
    for t in [[t[0], ab, ca], [ab, t[1], bc], [ca, bc, t[2]], [ab, bc, ca]] {
        pending.push((t, depth + 1));
    }
    Ok(())
}
fn boundary(t: [u32; 3], midpoints: &HashMap<(u32, u32), u32>) -> Vec<u32> {
    if [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])]
        .iter()
        .all(|(a, b)| !midpoints.contains_key(&key(*a, *b)))
    {
        return t.to_vec();
    }
    let mut out = Vec::new();
    for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
        let mut stack = vec![(a, b)];
        while let Some((a, b)) = stack.pop() {
            if let Some(&m) = midpoints.get(&key(a, b)) {
                stack.push((m, b));
                stack.push((a, m));
            } else {
                out.push(a);
            }
        }
    }
    out
}
/// Camera-dependent conforming subdivision. Shared edge midpoint IDs are
/// propagated to neighbours so curved boundaries cannot form T-junction cracks.
fn solid_color(area: &AreaInstruction) -> Result<[f32; 4], String> {
    match area.fill {
        AreaFillType::Solid(c) => Ok(c.to_array()),
        _ => Err("Globe area is not a solid fill".into()),
    }
}
/// Borrow rings directly for non-solid fills. Returns opaque geometry only;
/// the caller must apply its physical pattern before submitting the mesh.
pub fn drape_area_geometry(
    exterior: &[WorldPoint],
    interiors: &[Vec<WorldPoint>],
    camera: &GlobeCamera,
    limits: DrapingLimits,
) -> Result<(GlobeMesh, DrapingStats), String> {
    drape_area_internal(
        exterior, interiors, [1.; 4], camera, limits, false, None, None, None, false, false,
    )
    .map(|(mesh, stats, _, _)| (mesh, stats))
}
pub fn drape_area(
    area: &AreaInstruction,
    camera: &GlobeCamera,
    limits: DrapingLimits,
) -> Result<(GlobeMesh, DrapingStats), String> {
    drape_area_internal(
        &area.exterior,
        &area.interiors,
        solid_color(area)?,
        camera,
        limits,
        false,
        None,
        None,
        None,
        false,
        false,
    )
    .map(|(mesh, stats, _, _)| (mesh, stats))
}
pub(crate) fn drape_area_cached_bounded(
    area:&AreaInstruction,camera:&GlobeCamera,limits:DrapingLimits,capture_limit:usize,
)->Result<(GlobeMesh,DrapingStats,Option<CachedArea>),String> {
    drape_area_cached_bounded_midpoints(area,camera,limits,capture_limit,false)
}
pub(crate) fn drape_area_cached_bounded_midpoints(
    area:&AreaInstruction,camera:&GlobeCamera,limits:DrapingLimits,capture_limit:usize,midpoint_reuse:bool,
)->Result<(GlobeMesh,DrapingStats,Option<CachedArea>),String> {
    drape_area_internal_retained_bounded(&area.exterior,&area.interiors,solid_color(area)?,camera,limits,true,None,None,None,midpoint_reuse,false,false,Some(capture_limit))
        .map(|(m,s,c,_)|(m,s,c))
}
pub(crate) fn drape_area_cached(
    area: &AreaInstruction,
    camera: &GlobeCamera,
    limits: DrapingLimits,
) -> Result<(GlobeMesh, DrapingStats, Option<CachedArea>), String> {
    drape_area_cached_profiled(area, camera, limits, None)
}
pub(crate) fn drape_area_cached_profiled(
    area: &AreaInstruction,
    camera: &GlobeCamera,
    limits: DrapingLimits,
    profile: Option<&AreaProfile>,
) -> Result<(GlobeMesh, DrapingStats, Option<CachedArea>), String> {
    drape_area_cached_source(area, camera, limits, profile, None, None)
        .map(|(mesh, stats, cache, _)| (mesh, stats, cache))
}
pub(crate) fn drape_area_cached_source(
    area: &AreaInstruction,
    camera: &GlobeCamera,
    limits: DrapingLimits,
    profile: Option<&AreaProfile>,
    source: Option<&AreaSource>,
    budget: Option<&std::sync::Arc<AreaSourceBudget>>,
) -> Result<
    (
        GlobeMesh,
        DrapingStats,
        Option<CachedArea>,
        Option<AreaSourceCapture>,
    ),
    String,
> {
    drape_area_cached_source_optimized(area, camera, limits, profile, source, budget, false)
}
pub(crate) fn drape_area_cached_source_optimized(
    area: &AreaInstruction,
    camera: &GlobeCamera,
    limits: DrapingLimits,
    profile: Option<&AreaProfile>,
    source: Option<&AreaSource>,
    budget: Option<&std::sync::Arc<AreaSourceBudget>>,
    midpoint_reuse: bool,
) -> Result<
    (
        GlobeMesh,
        DrapingStats,
        Option<CachedArea>,
        Option<AreaSourceCapture>,
    ),
    String,
> {
    drape_area_cached_source_options(
        area,
        camera,
        limits,
        profile,
        source,
        budget,
        midpoint_reuse,
        false,
    )
}
pub(crate) fn drape_area_cached_source_options(
    area: &AreaInstruction,
    camera: &GlobeCamera,
    limits: DrapingLimits,
    profile: Option<&AreaProfile>,
    source: Option<&AreaSource>,
    budget: Option<&std::sync::Arc<AreaSourceBudget>>,
    midpoint_reuse: bool,
    chord_precheck: bool,
) -> Result<
    (
        GlobeMesh,
        DrapingStats,
        Option<CachedArea>,
        Option<AreaSourceCapture>,
    ),
    String,
> {
    count(profile, 6, 1);
    let _cold = AreaSpan::new(profile, 10);
    drape_area_internal(
        &area.exterior,
        &area.interiors,
        solid_color(area)?,
        camera,
        limits,
        true,
        profile,
        source,
        budget,
        midpoint_reuse,
        chord_precheck,
    )
}
pub(crate) fn drape_area_cached_source_retained_options(
    area: &AreaInstruction,
    camera: &GlobeCamera,
    limits: DrapingLimits,
    profile: Option<&AreaProfile>,
    source: Option<&AreaSource>,
    budget: Option<&std::sync::Arc<AreaSourceBudget>>,
    midpoint_reuse: bool,
    chord_precheck: bool,
    retained_pairs: bool,
) -> Result<
    (
        GlobeMesh,
        DrapingStats,
        Option<CachedArea>,
        Option<AreaSourceCapture>,
    ),
    String,
> {
    count(profile, 6, 1);
    let _cold = AreaSpan::new(profile, 10);
    drape_area_internal_retained(
        &area.exterior,
        &area.interiors,
        solid_color(area)?,
        camera,
        limits,
        true,
        profile,
        source,
        budget,
        midpoint_reuse,
        chord_precheck,
        retained_pairs,
    )
}
fn drape_area_internal(
    exterior: &[WorldPoint],
    interiors: &[Vec<WorldPoint>],
    color: [f32; 4],
    camera: &GlobeCamera,
    limits: DrapingLimits,
    record: bool,
    profile: Option<&AreaProfile>,
    source: Option<&AreaSource>,
    budget: Option<&std::sync::Arc<AreaSourceBudget>>,
    midpoint_reuse: bool,
    chord_precheck: bool,
) -> Result<
    (
        GlobeMesh,
        DrapingStats,
        Option<CachedArea>,
        Option<AreaSourceCapture>,
    ),
    String,
> {
    let retained_pairs=record && std::env::var("FERRITE_GLOBE_RETAINED_PASSING_PAIRS").as_deref()==Ok("1");
    drape_area_internal_retained(exterior,interiors,color,camera,limits,record,profile,source,budget,midpoint_reuse,chord_precheck,retained_pairs)
}
fn drape_area_internal_retained(
    exterior: &[WorldPoint],
    interiors: &[Vec<WorldPoint>],
    color: [f32; 4],
    camera: &GlobeCamera,
    limits: DrapingLimits,
    record: bool,
    profile: Option<&AreaProfile>,
    source: Option<&AreaSource>,
    budget: Option<&std::sync::Arc<AreaSourceBudget>>,
    midpoint_reuse: bool,
    chord_precheck: bool,
    retained_pairs: bool,
) -> Result<
    (
        GlobeMesh,
        DrapingStats,
        Option<CachedArea>,
        Option<AreaSourceCapture>,
    ),
    String,
> {
    drape_area_internal_retained_bounded(exterior,interiors,color,camera,limits,record,profile,source,budget,midpoint_reuse,chord_precheck,retained_pairs,None)
}
fn drape_area_internal_retained_bounded(
    exterior: &[WorldPoint],
    interiors: &[Vec<WorldPoint>],
    color: [f32; 4],
    camera: &GlobeCamera,
    limits: DrapingLimits,
    record: bool,
    profile: Option<&AreaProfile>,
    source: Option<&AreaSource>,
    budget: Option<&std::sync::Arc<AreaSourceBudget>>,
    midpoint_reuse: bool,
    chord_precheck: bool,
    retained_pairs: bool,
    capture_limit: Option<usize>,
) -> Result<
    (
        GlobeMesh,
        DrapingStats,
        Option<CachedArea>,
        Option<AreaSourceCapture>,
    ),
    String,
> {
    let probe_capacity=capture_limit.map(|limit|(limit/4).min(256*1024)/std::mem::size_of::<DecisionProbe>());
    let mut evaluation = Evaluation {
        camera,
        limits,
        probes:Vec::new(),
        passing: PassingCertificate::new_retained(record && retained_pairs),
        record,
        bounded_probe_capacity:probe_capacity,
        chord_precheck: chord_precheck && chord_precheck_view_is_safe(camera),
        deferred_screen_checks: 0,
        resolved_screen_checks: 0,
    };
    if !limits.screen_error_px.is_finite()
        || limits.screen_error_px <= 0.
        || !limits.chord_error_m.is_finite()
        || limits.chord_error_m <= 0.
        || !(3..=262144).contains(&limits.max_vertices)
    {
        return Err("Invalid globe draping limits".into());
    }
    if !color
        .iter()
        .all(|x| x.is_finite() && (0. ..=1.).contains(x))
    {
        return Err("Invalid globe fill color".into());
    }
    if color[3] == 0. || exterior.len() < 3 {
        return Ok((
            GlobeMesh {
                vertices: Vec::new(),
                indices: Vec::new(),
            },
            DrapingStats::default(),
            None,
            None,
        ));
    }
    if let Some(n)=probe_capacity {
        let accepted=evaluation.probes.try_reserve_exact(n).is_ok()
            && evaluation.probes.capacity().checked_mul(std::mem::size_of::<DecisionProbe>()).is_some_and(|bytes|bytes<=capture_limit.unwrap()/4);
        if !accepted {evaluation.probes=Vec::new();evaluation.record=false;}
    }
    let (uv, original, cache_bound) = if let Some(source) = source {
        if source.uv.len() > limits.max_vertices {
            return Err("Source globe polygon vertex budget exceeded".into());
        }
        if let Some(budget) = budget {
            budget
                .hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        (
            std::borrow::Cow::Borrowed(source.uv.as_ref()),
            std::borrow::Cow::Borrowed(source.triangles.as_ref()),
            source.bound,
        )
    } else {
        let source_uv = AreaSpan::new(profile, 11);
        let meridian = exterior.first().ok_or("Empty globe area")?.x;
        let mut uv = ring(&exterior, meridian)?;
        let outer_center = uv.iter().map(|p| p[0]).sum::<f64>() / uv.len() as f64 / WGS84_A * 180.
            / std::f64::consts::PI;
        let mut holes = Vec::new();
        for h in interiors {
            holes.push(uv.len());
            uv.extend(ring(h, outer_center)?);
        }
        if uv.len() > limits.max_vertices {
            return Err("Source globe polygon vertex budget exceeded".into());
        }
        drop(source_uv);
        let source_topology = AreaSpan::new(profile, 12);
        let coords: Vec<_> = uv.iter().flat_map(|p| *p).collect();
        let original = ferrite_kernel::triangulation::triangulate(&coords, &holes)
            .map_err(|e| e.to_string())?;
        drop(source_topology);
        let source_bound = AreaSpan::new(profile, 13);
        // Validate rings/holes and triangulation first. A visibility optimization
        // must not conceal malformed source topology. Mercator interpolation is
        // monotone in latitude and continuous lifted longitude, so this rectangle
        // encloses every adaptive vertex and the convex chords of every triangle.
        let mut cache_bound = None;
        {
            let lon = uv
                .iter()
                .map(|p| (p[0] / WGS84_A).to_degrees())
                .fold([f64::INFINITY, f64::NEG_INFINITY], |a, x| {
                    [a[0].min(x), a[1].max(x)]
                });
            let y = uv
                .iter()
                .map(|p| p[1])
                .fold([f64::INFINITY, f64::NEG_INFINITY], |a, x| {
                    [a[0].min(x), a[1].max(x)]
                });
            let lat = [
                Mercator::World
                    .unproject([0., y[0]])
                    .map_err(|e| e.to_string())?
                    .latitude(),
                Mercator::World
                    .unproject([0., y[1]])
                    .map_err(|e| e.to_string())?
                    .latitude(),
            ];
            if let Ok(bound) =
                ferrite_kernel::surface_bounds::GeographicSurfaceBounds::new(lat, lon)
            {
                let (centre, radius) = bound.sphere();
                cache_bound = Some((centre, radius));
            }
        }
        drop(source_bound);
        (
            std::borrow::Cow::Owned(uv),
            std::borrow::Cow::Owned(original),
            cache_bound,
        )
    };
    let source_bound = AreaSpan::new(profile, 13);
    if let Some((centre, radius)) = cache_bound {
        if limits.frustum_culling
            && camera
                .sphere_outside_frustum(centre, radius, 2.)
                .map_err(|e| e.to_string())?
        {
            let capture = if source.is_none() {
                AreaSource::capture(&uv, &original, cache_bound, &[], budget)
            } else {
                None
            };
            return Ok((
                GlobeMesh {
                    vertices: Vec::new(),
                    indices: Vec::new(),
                },
                DrapingStats {
                    source_vertices: uv.len(),
                    source_triangles: original.len() / 3,
                    frustum_culled: true,
                    ..Default::default()
                },
                None,
                capture,
            ));
        }
    }
    drop(source_bound);
    let copy_uv = AreaSpan::new(profile, 11);
    let mut uv = uv.into_owned();
    drop(copy_uv);
    let source_ecef = AreaSpan::new(profile, 14);
    let mut vertices: Vec<GlobeVertex> =
        if let Some(source) = source.filter(|s| s.ecef.len() == uv.len()) {
            source
                .ecef
                .iter()
                .map(|ecef_m| GlobeVertex {
                    ecef_m: *ecef_m,
                    color,
                })
                .collect()
        } else {
            uv.iter()
                .map(|p| {
                    Ok(GlobeVertex {
                        ecef_m: ecef(*p)?,
                        color,
                    })
                })
                .collect::<Result<_, String>>()?
        };
    drop(source_ecef);
    let captured_source = if source.is_none() || (source.is_some_and(|s| s.ecef.is_empty()) && !budget.is_some_and(|b| b.topology_only)) {
        AreaSource::capture(&uv, &original, cache_bound, &vertices, budget)
    } else {
        None
    };
    let subdivision = AreaSpan::new(profile, 15);
    let mut stats = DrapingStats {
        source_vertices: uv.len(),
        source_triangles: original.len() / 3,
        ..Default::default()
    };
    let mut pending: Vec<([u32; 3], u8)> = original
        .chunks_exact(3)
        .map(|t| ([t[0] as u32, t[1] as u32, t[2] as u32], 0))
        .collect();
    let mut leaves = Vec::new();
    let mut midpoints = HashMap::<(u32, u32), u32>::new();
    let mut errors = HashMap::new();
    loop {
        while let Some((t, depth)) = pending.pop() {
            let mut refine = false;
            // Stack-only samples are valid for this exact edge evaluation. Cache
            // hits provide no sample and retain the original split calculation.
            let mut samples = [None; 3];
            for (i, (a, b)) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])]
                .into_iter()
                .enumerate()
            {
                let (e, actual) = edge_sample(
                    a,
                    b,
                    &uv,
                    &vertices,
                    &mut evaluation,
                    &mut errors,
                    midpoint_reuse,
                    true,
                )?;
                samples[i] = actual;
                refine |= e.0 > limits.screen_error_px || e.1 > limits.chord_error_m;
            }
            let center = std::array::from_fn(|i| {
                (uv[t[0] as usize][i] + uv[t[1] as usize][i] + uv[t[2] as usize][i]) / 3.
            });
            let linear = std::array::from_fn(|i| {
                (vertices[t[0] as usize].ecef_m[i]
                    + vertices[t[1] as usize].ecef_m[i]
                    + vertices[t[2] as usize].ecef_m[i])
                    / 3.
            });
            let e = evaluation.check_adaptive(ecef(center)?, linear)?;
            refine |= e.0 > limits.screen_error_px || e.1 > limits.chord_error_m;
            if refine {
                red_split(
                    t,
                    depth,
                    &mut pending,
                    &mut uv,
                    &mut vertices,
                    &mut midpoints,
                    &mut stats,
                    color,
                    limits.max_vertices,
                    samples,
                )?;
            } else {
                leaves.push((t, depth));
            }
            if pending.len() + leaves.len() > 524288 {
                return Err("Globe triangle budget exceeded".into());
            }
        }
        // Red refinement always halves every edge. Conformity is completed by
        // a fan from the leaf centre to all shared boundary subdivisions, rather
        // than recursively bisecting arbitrary short edges into sliver cascades.
        let mut stable = Vec::with_capacity(leaves.len());
        for (t, depth) in leaves.drain(..) {
            let boundary = boundary(t, &midpoints);
            let mut refine = false;
            if boundary.len() > 3 {
                let center = std::array::from_fn(|i| {
                    (uv[t[0] as usize][i] + uv[t[1] as usize][i] + uv[t[2] as usize][i]) / 3.
                });
                let position = ecef(center)?;
                for &id in &boundary {
                    let actual = ecef(std::array::from_fn(|i| {
                        (center[i] + uv[id as usize][i]) / 2.
                    }))?;
                    let linear = std::array::from_fn(|i| {
                        (position[i] + vertices[id as usize].ecef_m[i]) / 2.
                    });
                    let e = evaluation.check_adaptive(actual, linear)?;
                    refine |= e.0 > limits.screen_error_px || e.1 > limits.chord_error_m;
                }
            }
            if refine {
                red_split(
                    t,
                    depth,
                    &mut pending,
                    &mut uv,
                    &mut vertices,
                    &mut midpoints,
                    &mut stats,
                    color,
                    limits.max_vertices,
                    [None; 3],
                )?;
            } else {
                stable.push((t, depth));
            }
        }
        leaves = stable;
        if pending.is_empty() {
            break;
        }
    }
    let mut indices = Vec::new();
    for (t, _) in leaves {
        let boundary = boundary(t, &midpoints);
        if boundary.len() == 3 {
            indices.extend_from_slice(&t);
        } else {
            if vertices.len() >= limits.max_vertices {
                return Err("Conforming globe polygon vertex budget exceeded".into());
            }
            let center = std::array::from_fn(|i| {
                (uv[t[0] as usize][i] + uv[t[1] as usize][i] + uv[t[2] as usize][i]) / 3.
            });
            let id = vertices.len() as u32;
            uv.push(center);
            vertices.push(GlobeVertex {
                ecef_m: ecef(center)?,
                color,
            });
            for i in 0..boundary.len() {
                indices.extend_from_slice(&[id, boundary[i], boundary[(i + 1) % boundary.len()]]);
            }
        }
        if indices.len() > 1572864 {
            return Err("Conforming globe triangle budget exceeded".into());
        }
    }
    stats.final_triangles = indices.len() / 3;
    for t in indices.chunks_exact(3) {
        for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
            let e = edge_error(a, b, &uv, &vertices, &mut evaluation, &mut errors)?;
            stats.max_edge_error_px = stats.max_edge_error_px.max(e.0);
            stats.max_edge_chord_error_m = stats.max_edge_chord_error_m.max(e.1);
        }
    }
    let mesh = GlobeMesh { vertices, indices };
    mesh.validate()?;
    drop(subdivision);
    stats.deferred_screen_checks = evaluation.deferred_screen_checks;
    stats.resolved_screen_checks = evaluation.resolved_screen_checks;
    if let Some(profile) = profile {
        profile.deferred_screen_checks.fetch_add(
            stats.deferred_screen_checks as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        profile.resolved_screen_checks.fetch_add(
            stats.resolved_screen_checks as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        profile.midpoint_vertices_created.fetch_add(
            stats.refinements as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        profile.midpoint_ecef_reused.fetch_add(
            stats.midpoint_ecef_reused as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    let _cache_record = AreaSpan::new(profile, 16);
    // Check optional allocation BEFORE the mesh clone and used-vertex bitmap.
    // The returned cold mesh is baseline work, not optional retained storage.
    let capture_allowed=record && evaluation.record && capture_limit.is_none_or(|limit| {
        let base=std::mem::size_of::<CachedArea>()+std::mem::size_of::<GlobeMesh>()+2*std::mem::size_of::<usize>();
        base.checked_add(evaluation.probes.capacity().checked_mul(std::mem::size_of::<DecisionProbe>()).unwrap_or(usize::MAX))
            .and_then(|n|n.checked_add(mesh.vertices.len().checked_mul(std::mem::size_of::<GlobeVertex>())?))
            .and_then(|n|n.checked_add(mesh.indices.len().checked_mul(4)?))
            .and_then(|n|n.checked_add(mesh.vertices.len()))
            .is_some_and(|n|n<=limit)
    });
    // Sharing must not submit unused ring vertices or increase scene budgets.
    let all_vertices_used = if capture_allowed {
        let mut used = vec![false; mesh.vertices.len()];
        for index in &mesh.indices {
            used[*index as usize] = true;
        }
        used.into_iter().all(|u| u)
    } else {
        false
    };
    let cached = cache_bound.filter(|_| capture_allowed).map(|bound| CachedArea {
        mesh: std::sync::Arc::new(mesh.clone()),
        all_vertices_used,
        limits,
        probes: evaluation.probes,
        passing: evaluation.passing,
        bound,
    });
    Ok((mesh, stats, cached, captured_source))
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_render::Color;
    #[test]
    fn curved_dateline_area_keeps_hole_and_shared_edges() {
        let mut a = AreaInstruction::new(vec![
            WorldPoint::new(179., 60.),
            WorldPoint::new(-179., 60.),
            WorldPoint::new(-179., 62.),
            WorldPoint::new(179., 62.),
        ])
        .with_solid_fill(Color::BLACK);
        a.interiors.push(vec![
            WorldPoint::new(179.5, 60.5),
            WorldPoint::new(179.5, 61.5),
            WorldPoint::new(-179.5, 61.5),
            WorldPoint::new(-179.5, 60.5),
        ]);
        let c = GlobeCamera::orbit(
            GeographicPosition::new(61., 180.).unwrap(),
            500000.,
            0.,
            35.,
            [1000., 800.],
            45.,
            1.,
            1e9,
        )
        .unwrap();
        let (m, s) = drape_area(&a, &c, DrapingLimits::default()).unwrap();
        assert!(
            s.refinements > 0
                && s.max_edge_error_px <= 0.25 + 1e-9
                && s.max_edge_chord_error_m <= 5. + 1e-8
        );
        assert_eq!(m.indices.len() / 3, s.final_triangles);
        // Every singly used edge must be on an authored outer/hole boundary.
        // A T-junction leaves an unmatched interior edge and fails this check.
        let projected: Vec<_> = m
            .vertices
            .iter()
            .map(|v| {
                let p = ferrite_kernel::geocentric::from_ecef(v.ecef_m)
                    .unwrap()
                    .surface;
                let mut xy = Mercator::World.project(p).unwrap();
                xy[0] = WGS84_A * p.longitude_near(180.).unwrap().to_radians();
                xy
            })
            .collect();
        let boundaries: Vec<_> = std::iter::once(&a.exterior)
            .chain(a.interiors.iter())
            .map(|r| ring(r, 180.).unwrap())
            .collect();
        let mut edges = HashMap::<(u32, u32), usize>::new();
        for t in m.indices.chunks(3) {
            for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
                *edges.entry(key(a, b)).or_default() += 1;
            }
        }
        for ((a, b), count) in edges {
            assert!(count <= 2);
            if count == 1 {
                assert!(
                    boundaries
                        .iter()
                        .any(|r| (0..r.len()).any(|i| [a, b].iter().all(|id| {
                            ferrite_kernel::closest_on_path(
                                [r[i], r[(i + 1) % r.len()]],
                                projected[*id as usize],
                                false,
                            )
                            .unwrap()
                            .0 < 1e-3
                        }))),
                    "unmatched interior edge {a}-{b}"
                );
            }
        }

        // A ray through the centre hole must intersect no source triangle in
        // Mercator coordinates; subdivision has not filled the excluded interior.
        let q = Mercator::World
            .project(GeographicPosition::new(61., 180.).unwrap())
            .unwrap();
        for t in m.indices.chunks(3) {
            let xy: Vec<_> = t
                .iter()
                .map(|i| {
                    let p = ferrite_kernel::geocentric::from_ecef(m.vertices[*i as usize].ecef_m)
                        .unwrap()
                        .surface;
                    let mut xy = Mercator::World.project(p).unwrap();
                    xy[0] = WGS84_A * p.longitude_near(180.).unwrap().to_radians();
                    xy
                })
                .collect();
            assert!(!ferrite_kernel::inside_ring(xy, q));
        }
        assert!(drape_area(
            &a,
            &c,
            DrapingLimits {
                max_vertices: 8,
                ..Default::default()
            }
        )
        .is_err());
    }
}

#[cfg(test)]
mod near_plane_tests {
    use super::*;
    use ferrite_render::Color;
    #[test]
    fn low_tilted_area_crossing_eye_keeps_hole_and_finite_visible_error() {
        let mut a = AreaInstruction::new(vec![
            WorldPoint::new(-0.1, -0.1),
            WorldPoint::new(0.1, -0.1),
            WorldPoint::new(0.1, 0.1),
            WorldPoint::new(-0.1, 0.1),
        ])
        .with_solid_fill(Color::BLACK);
        a.interiors.push(vec![
            WorldPoint::new(-0.001, -0.001),
            WorldPoint::new(-0.001, 0.001),
            WorldPoint::new(0.001, 0.001),
            WorldPoint::new(0.001, -0.001),
        ]);
        let c = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            500.,
            0.,
            45.,
            [1000., 800.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        let (m, s) = drape_area(&a, &c, Default::default()).unwrap();
        assert!(m
            .vertices
            .iter()
            .any(|v| c.clip_ecef(v.ecef_m).unwrap()[3] < 0.));
        assert!(m
            .vertices
            .iter()
            .any(|v| c.clip_ecef(v.ecef_m).unwrap()[3] > 1.));
        assert!(s.max_edge_error_px <= 0.25 + 1e-8 && s.max_edge_chord_error_m <= 5. + 1e-8);
        assert!(s.refinements > 0 && s.final_triangles > 10);
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use ferrite_render::Color;
    #[test]
    fn whole_visible_area_shares_geometry_but_recolor_and_close_view_do_not() {
        let area = AreaInstruction::new(vec![
            WorldPoint::new(-0.02, 49.98),
            WorldPoint::new(0.02, 49.98),
            WorldPoint::new(0.02, 50.02),
            WorldPoint::new(-0.02, 50.02),
        ])
        .with_solid_fill(Color::BLACK);
        let camera = |range| {
            GlobeCamera::orbit(
                GeographicPosition::new(50., 0.).unwrap(),
                range,
                0.,
                30.,
                [1000., 800.],
                45.,
                1.,
                1e9,
            )
            .unwrap()
        };
        let c = camera(250000.);
        let (cold, _, cache) = drape_area_cached(&area, &c, DrapingLimits::default()).unwrap();
        let cache = cache.unwrap();
        assert!(cache
            .reusable_mesh(&c, Color::BLACK.to_array())
            .unwrap()
            .is_some());
        let a = cache
            .shared_whole_mesh(&c, Color::BLACK.to_array())
            .unwrap()
            .unwrap();
        let b = cache
            .shared_whole_mesh(&camera(251000.), Color::BLACK.to_array())
            .unwrap()
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&a, &b));
        assert_eq!(a.indices, cold.indices);
        assert!(cache
            .shared_whole_mesh(&c, [1., 0., 0., 1.])
            .unwrap()
            .is_none());
        assert!(cache
            .shared_whole_mesh(&camera(1000.), Color::BLACK.to_array())
            .unwrap()
            .is_none());
        drop(cache);
        assert_eq!(a.vertices.len(), cold.vertices.len());
    }
    #[test]
    fn decision_cache_matches_cold_geometry_and_recolors() {
        let area = AreaInstruction::new(vec![
            WorldPoint::new(-3., 48.5),
            WorldPoint::new(-2.8, 48.5),
            WorldPoint::new(-2.8, 48.7),
            WorldPoint::new(-3., 48.7),
        ])
        .with_solid_fill(Color::BLACK);
        let camera = |range| {
            GlobeCamera::orbit(
                GeographicPosition::new(48.6, -2.9).unwrap(),
                range,
                0.,
                35.,
                [1000., 800.],
                45.,
                1.,
                1e9,
            )
            .unwrap()
        };
        let (_, _, cached) =
            drape_area_cached(&area, &camera(500000.), DrapingLimits::default()).unwrap();
        let cached = cached.unwrap();
        let mut hits = 0;
        for range in [505000., 490000., 250000., 10000.] {
            let c = camera(range);
            let cold = drape_area(&area, &c, DrapingLimits::default()).unwrap();
            if let Some(warm) = cached.reuse(&c, Color::BLACK.to_array()).unwrap() {
                hits += 1;
                assert_eq!(warm.indices, cold.0.indices);
                assert_eq!(warm.vertices.len(), cold.0.vertices.len());
                for (a, b) in warm.vertices.iter().zip(&cold.0.vertices) {
                    assert_eq!(a.ecef_m, b.ecef_m);
                    assert_eq!(a.color, b.color);
                }
                assert!(cold.1.max_edge_error_px <= 0.25 + 1e-9);
                assert!(cold.1.max_edge_chord_error_m <= 5. + 1e-8);
            }
        }
        assert!(hits >= 2);
        let color = [0.2, 0.3, 0.4, 0.5];
        let warm = cached.reuse(&camera(500000.), color).unwrap().unwrap();
        assert!(warm.vertices.iter().all(|v| v.color == color));
        assert!(cached.reuse(&camera(500000.), [0.; 4]).unwrap().is_none());
    }
}
#[cfg(test)]
mod area_source_tests {
    use super::*;
    use ferrite_render::Color;
    #[test]
    fn midpoint_sample_reuse_preserves_exact_geometry_and_decision_transcript() {
        let rectangle = AreaInstruction::new(vec![
            WorldPoint::new(-3., 48.),
            WorldPoint::new(-2., 48.),
            WorldPoint::new(-2., 49.),
            WorldPoint::new(-3., 49.),
        ])
        .with_solid_fill(Color::BLACK);
        let mut dateline = AreaInstruction::new(vec![
            WorldPoint::new(179., 60.),
            WorldPoint::new(-179., 60.),
            WorldPoint::new(-179., 62.),
            WorldPoint::new(179., 62.),
        ])
        .with_solid_fill(Color::BLACK);
        dateline.interiors.push(vec![
            WorldPoint::new(179.5, 60.5),
            WorldPoint::new(179.5, 61.5),
            WorldPoint::new(-179.5, 61.5),
            WorldPoint::new(-179.5, 60.5),
        ]);
        let mut eye_crossing = AreaInstruction::new(vec![
            WorldPoint::new(-0.1, -0.1),
            WorldPoint::new(0.1, -0.1),
            WorldPoint::new(0.1, 0.1),
            WorldPoint::new(-0.1, 0.1),
        ])
        .with_solid_fill(Color::BLACK);
        eye_crossing.interiors.push(vec![
            WorldPoint::new(-0.001, -0.001),
            WorldPoint::new(-0.001, 0.001),
            WorldPoint::new(0.001, 0.001),
            WorldPoint::new(0.001, -0.001),
        ]);
        let mut reused = 0;
        for (area, lat, lon) in [
            (rectangle, 48.6, -2.9),
            (dateline, 61., 180.),
            (eye_crossing, 0., 0.),
        ] {
            for range in [500000., 10000., 500.] {
                let c = camera(lat, lon, range);
                for max_vertices in [262144, 3] {
                    let limits = DrapingLimits {
                        max_vertices,
                        ..Default::default()
                    };
                    let off = drape_area_cached_source_optimized(
                        &area, &c, limits, None, None, None, false,
                    );
                    let on = drape_area_cached_source_optimized(
                        &area, &c, limits, None, None, None, true,
                    );
                    match (off, on) {
                        (Ok((a, sa, ca, _)), Ok((b, sb, cb, _))) => {
                            assert_mesh_bits(&a, &b);
                            assert_eq!(sa.refinements, sb.refinements);
                            assert_eq!(
                                sa.max_edge_error_px.to_bits(),
                                sb.max_edge_error_px.to_bits()
                            );
                            assert_eq!(
                                sa.max_edge_chord_error_m.to_bits(),
                                sb.max_edge_chord_error_m.to_bits()
                            );
                            assert_eq!(ca.is_some(), cb.is_some());
                            if let (Some(a), Some(b)) = (ca, cb) {
                                assert_eq!(a.probes.len(), b.probes.len());
                                for (a, b) in a.probes.iter().zip(&b.probes) {
                                    assert_eq!(
                                        a.actual.map(f64::to_bits),
                                        b.actual.map(f64::to_bits)
                                    );
                                    assert_eq!(
                                        a.linear.map(f64::to_bits),
                                        b.linear.map(f64::to_bits)
                                    );
                                    assert_eq!(a.exceeds_screen, b.exceeds_screen);
                                }
                                assert_eq!(
                                    a.passing.bounds.map(|v| v.map(f64::to_bits)),
                                    b.passing.bounds.map(|v| v.map(f64::to_bits))
                                );
                                assert_eq!(a.passing.chord.to_bits(), b.passing.chord.to_bits());
                            }
                            reused += sb.midpoint_ecef_reused;
                        }
                        (Err(a), Err(b)) => assert_eq!(a, b),
                        _ => panic!("midpoint reuse changed outcome"),
                    }
                }
            }
        }
        assert!(reused > 0);
    }
    #[test]
    fn chord_precheck_preserves_exact_final_errors_geometry_and_transcript() {
        let rectangle = AreaInstruction::new(vec![
            WorldPoint::new(-3., 48.),
            WorldPoint::new(-2., 48.),
            WorldPoint::new(-2., 49.),
            WorldPoint::new(-3., 49.),
        ])
        .with_solid_fill(Color::BLACK);
        let mut dateline = AreaInstruction::new(vec![
            WorldPoint::new(179., 60.),
            WorldPoint::new(-179., 60.),
            WorldPoint::new(-179., 62.),
            WorldPoint::new(179., 62.),
        ])
        .with_solid_fill(Color::BLACK);
        dateline.interiors.push(vec![
            WorldPoint::new(179.5, 60.5),
            WorldPoint::new(179.5, 61.5),
            WorldPoint::new(-179.5, 61.5),
            WorldPoint::new(-179.5, 60.5),
        ]);
        let mut eye_crossing = AreaInstruction::new(vec![
            WorldPoint::new(-0.1, -0.1),
            WorldPoint::new(0.1, -0.1),
            WorldPoint::new(0.1, 0.1),
            WorldPoint::new(-0.1, 0.1),
        ])
        .with_solid_fill(Color::BLACK);
        eye_crossing.interiors.push(vec![
            WorldPoint::new(-0.001, -0.001),
            WorldPoint::new(-0.001, 0.001),
            WorldPoint::new(0.001, 0.001),
            WorldPoint::new(0.001, -0.001),
        ]);
        let mut reused = 0;
        for (area, lat, lon) in [
            (rectangle, 48.6, -2.9),
            (dateline, 61., 180.),
            (eye_crossing, 0., 0.),
        ] {
            for range in [500000., 10000., 500.] {
                let c = camera(lat, lon, range);
                for max_vertices in [262144, 3] {
                    let limits = DrapingLimits {
                        max_vertices,
                        ..Default::default()
                    };
                    let off = drape_area_cached_source_options(
                        &area, &c, limits, None, None, None, false, false,
                    );
                    let on = drape_area_cached_source_options(
                        &area, &c, limits, None, None, None, false, true,
                    );
                    match (off, on) {
                        (Ok((a, sa, ca, _)), Ok((b, sb, cb, _))) => {
                            assert_mesh_bits(&a, &b);
                            assert_eq!(sa.refinements, sb.refinements);
                            assert_eq!(
                                sa.max_edge_error_px.to_bits(),
                                sb.max_edge_error_px.to_bits()
                            );
                            assert_eq!(
                                sa.max_edge_chord_error_m.to_bits(),
                                sb.max_edge_chord_error_m.to_bits()
                            );
                            assert_eq!(ca.is_some(), cb.is_some());
                            if let (Some(a), Some(b)) = (ca, cb) {
                                assert_eq!(a.probes.len(), b.probes.len());
                                for (a, b) in a.probes.iter().zip(&b.probes) {
                                    assert_eq!(
                                        a.actual.map(f64::to_bits),
                                        b.actual.map(f64::to_bits)
                                    );
                                    assert_eq!(
                                        a.linear.map(f64::to_bits),
                                        b.linear.map(f64::to_bits)
                                    );
                                    assert_eq!(a.exceeds_screen, b.exceeds_screen);
                                }
                                assert_eq!(
                                    a.passing.bounds.map(|v| v.map(f64::to_bits)),
                                    b.passing.bounds.map(|v| v.map(f64::to_bits))
                                );
                                assert_eq!(a.passing.chord.to_bits(), b.passing.chord.to_bits());
                            }
                            reused += sb.deferred_screen_checks;
                        }
                        (Err(a), Err(b)) => assert_eq!(a, b),
                        _ => panic!("chord precheck changed outcome"),
                    }
                }
            }
        }
        assert!(reused > 0);
    }
    #[test]
    fn chord_precheck_keeps_strict_threshold_and_reference_recording() {
        let c = camera(0., 1., 500000.);
        let actual = ecef([0., 0.]).unwrap();
        let linear = [actual[0] + 17., actual[1] - 2., actual[2] + 3.];
        let chord = deviation(&c, actual, linear).unwrap().1;
        for bound in [
            f64::from_bits(chord.to_bits() - 1),
            chord,
            f64::from_bits(chord.to_bits() + 1),
        ] {
            let make = |enabled| Evaluation {
                camera: &c,
                limits: DrapingLimits {
                    chord_error_m: bound,
                    ..Default::default()
                },
                probes: Vec::new(),
                passing: PassingCertificate::new(),
                record: true,
                bounded_probe_capacity:None,
                chord_precheck: enabled,
                deferred_screen_checks: 0,
                resolved_screen_checks: 0,
            };
            let mut off = make(false);
            let mut on = make(true);
            let a = off.check(actual, linear).unwrap();
            let b = on.check_adaptive(actual, linear).unwrap();
            assert_eq!(a.1.to_bits(), b.1.to_bits());
            if chord > bound {
                assert_eq!(b.0, DEFERRED_SCREEN);
                assert_eq!(on.deferred_screen_checks, 1);
            } else {
                assert_eq!(a.0.to_bits(), b.0.to_bits());
                assert_eq!(on.deferred_screen_checks, 0);
            }
            assert_eq!(off.probes.len(), on.probes.len());
            for (a, b) in off.probes.iter().zip(&on.probes) {
                assert_eq!(a.actual.map(f64::to_bits), b.actual.map(f64::to_bits));
                assert_eq!(a.linear.map(f64::to_bits), b.linear.map(f64::to_bits));
                assert_eq!(a.exceeds_screen, b.exceeds_screen);
            }
            assert_eq!(
                off.passing.bounds.map(|v| v.map(f64::to_bits)),
                on.passing.bounds.map(|v| v.map(f64::to_bits))
            );
            assert_eq!(off.passing.chord.to_bits(), on.passing.chord.to_bits());
        }
    }
    #[test]
    fn deferred_edge_error_resolves_exactly_before_final_statistics() {
        let c = camera(0., 1., 500000.);
        let uv = [
            Mercator::World
                .project(GeographicPosition::new(0., 0.).unwrap())
                .unwrap(),
            Mercator::World
                .project(GeographicPosition::new(0., 2.).unwrap())
                .unwrap(),
        ];
        let vertices: Vec<_> = uv
            .iter()
            .map(|p| GlobeVertex {
                ecef_m: ecef(*p).unwrap(),
                color: [1.; 4],
            })
            .collect();
        let mut evaluation = Evaluation {
            camera: &c,
            limits: Default::default(),
            probes: Vec::new(),
            passing: PassingCertificate::new(),
            record: true,
            bounded_probe_capacity:None,
            chord_precheck: true,
            deferred_screen_checks: 0,
            resolved_screen_checks: 0,
        };
        let mut errors = HashMap::new();
        let (adaptive, _) = edge_sample(
            0,
            1,
            &uv,
            &vertices,
            &mut evaluation,
            &mut errors,
            false,
            true,
        )
        .unwrap();
        assert_eq!(adaptive.0, DEFERRED_SCREEN);
        assert!(adaptive.1 > 5.);
        let full = edge_error(0, 1, &uv, &vertices, &mut evaluation, &mut errors).unwrap();
        let actual = ecef(std::array::from_fn(|i| (uv[0][i] + uv[1][i]) / 2.)).unwrap();
        let linear = std::array::from_fn(|i| (vertices[0].ecef_m[i] + vertices[1].ecef_m[i]) / 2.);
        let reference = deviation(&c, actual, linear).unwrap();
        assert_eq!(full.0.to_bits(), reference.0.to_bits());
        assert_eq!(full.1.to_bits(), reference.1.to_bits());
        assert_eq!(errors[&key(0, 1)].0.to_bits(), reference.0.to_bits());
        assert_eq!(evaluation.deferred_screen_checks, 1);
        assert_eq!(evaluation.resolved_screen_checks, 1);
        assert!(evaluation.probes.is_empty());
    }
    #[test]
    fn extreme_view_retains_reference_error_evaluation() {
        let c = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            1e13,
            0.,
            0.,
            [1200., 800.],
            45.,
            1.,
            1e15,
        )
        .unwrap();
        assert!(!chord_precheck_view_is_safe(&c));
        let area = AreaInstruction::new(vec![
            WorldPoint::new(-1., -1.),
            WorldPoint::new(1., -1.),
            WorldPoint::new(1., 1.),
            WorldPoint::new(-1., 1.),
        ])
        .with_solid_fill(Color::BLACK);
        let (a, sa, _, _) = drape_area_cached_source_options(
            &area,
            &c,
            Default::default(),
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        let (b, sb, _, _) = drape_area_cached_source_options(
            &area,
            &c,
            Default::default(),
            None,
            None,
            None,
            false,
            true,
        )
        .unwrap();
        assert_mesh_bits(&a, &b);
        assert_eq!(
            sa.max_edge_error_px.to_bits(),
            sb.max_edge_error_px.to_bits()
        );
        assert_eq!(
            sa.max_edge_chord_error_m.to_bits(),
            sb.max_edge_chord_error_m.to_bits()
        );
        assert_eq!(sb.deferred_screen_checks, 0);
    }
    #[test]
    fn chord_precheck_preserves_horizon_polar_views_and_quality_bounds() {
        let mut count = 0;
        for (latitude, longitude) in [(0., 0.), (48., -3.), (80., 179.8), (-80., -179.8)] {
            let mut area = AreaInstruction::new(vec![
                WorldPoint::new(longitude - 0.2, latitude - 0.2),
                WorldPoint::new(longitude + 0.2, latitude - 0.2),
                WorldPoint::new(longitude + 0.2, latitude + 0.2),
                WorldPoint::new(longitude - 0.2, latitude + 0.2),
            ])
            .with_solid_fill(Color::BLACK);
            area.interiors.push(vec![
                WorldPoint::new(longitude - 0.05, latitude - 0.05),
                WorldPoint::new(longitude - 0.05, latitude + 0.05),
                WorldPoint::new(longitude + 0.05, latitude + 0.05),
                WorldPoint::new(longitude + 0.05, latitude - 0.05),
            ]);
            for (offset, range, heading, tilt) in [
                (0., 100000., 0., 0.),
                (0., 10000., 179., 75.),
                (2., 50000., 91., 89.),
                (45., 500000., 13., 35.),
            ] {
                let c = GlobeCamera::orbit(
                    GeographicPosition::new(
                        latitude,
                        (longitude + offset + 180.).rem_euclid(360.) - 180.,
                    )
                    .unwrap(),
                    range,
                    heading,
                    tilt,
                    [1237., 811.],
                    47.,
                    0.1,
                    1e9,
                )
                .unwrap();
                for (screen, chord, culling) in
                    [(0.25, 5., true), (2., 50., false), (0.05, 0.1, true)]
                {
                    let limits = DrapingLimits {
                        screen_error_px: screen,
                        chord_error_m: chord,
                        max_vertices: 20000,
                        frustum_culling: culling,
                    };
                    let off = drape_area_cached_source_options(
                        &area, &c, limits, None, None, None, false, false,
                    );
                    let on = drape_area_cached_source_options(
                        &area, &c, limits, None, None, None, false, true,
                    );
                    match (off, on) {
                        (Ok((a, sa, ca, _)), Ok((b, sb, cb, _))) => {
                            assert_mesh_bits(&a, &b);
                            assert_eq!(sa.refinements, sb.refinements);
                            assert_eq!(sa.frustum_culled, sb.frustum_culled);
                            assert_eq!(
                                sa.max_edge_error_px.to_bits(),
                                sb.max_edge_error_px.to_bits()
                            );
                            assert_eq!(
                                sa.max_edge_chord_error_m.to_bits(),
                                sb.max_edge_chord_error_m.to_bits()
                            );
                            assert_eq!(ca.is_some(), cb.is_some());
                            if let (Some(a), Some(b)) = (ca, cb) {
                                assert_eq!(a.probes.len(), b.probes.len());
                                for (a, b) in a.probes.iter().zip(&b.probes) {
                                    assert_eq!(
                                        a.actual.map(f64::to_bits),
                                        b.actual.map(f64::to_bits)
                                    );
                                    assert_eq!(
                                        a.linear.map(f64::to_bits),
                                        b.linear.map(f64::to_bits)
                                    );
                                    assert_eq!(a.exceeds_screen, b.exceeds_screen);
                                }
                                assert_eq!(
                                    a.passing.bounds.map(|v| v.map(f64::to_bits)),
                                    b.passing.bounds.map(|v| v.map(f64::to_bits))
                                );
                                assert_eq!(a.passing.chord.to_bits(), b.passing.chord.to_bits());
                            }
                            count += 1;
                        }
                        (Err(a), Err(b)) => {
                            assert_eq!(a, b);
                            count += 1;
                        }
                        _ => panic!("view/quality changed outcome"),
                    }
                }
            }
        }
        assert_eq!(count, 48);
    }
    fn camera(latitude: f64, longitude: f64, range: f64) -> GlobeCamera {
        GlobeCamera::orbit(
            GeographicPosition::new(latitude, longitude).unwrap(),
            range,
            13.,
            35.,
            [1200., 800.],
            45.,
            1.,
            1e9,
        )
        .unwrap()
    }
    #[test]
    fn retained_pointwise_certificate_is_independent_when_box_is_inconclusive() {
        let c=camera(48.6,-2.9,500000.);let mut proof=PassingCertificate::new_retained(true);
        for (lat,lon) in [(48.6,-2.9),(-80.,180.),(80.,0.),(0.,-90.),(0.,90.)] {
            let a=GeographicPosition::new(lat,lon).unwrap().to_ecef(0.).unwrap();let b=[a[0]+0.0001,a[1],a[2]];
            assert!(deviation(&c,a,b).unwrap().0<=0.25);proof.include(a,b);
        }
        assert!(!proof.accepts_box(&c,0.25).unwrap());assert!(proof.accepts(&c,0.25).unwrap());
        for (a,b) in proof.exact_pairs.as_ref().unwrap() {assert!(deviation(&c,*a,*b).unwrap().0<=0.25);}
    }
    #[test]
    fn retained_passing_capacity_failure_discards_entire_optional_proof() {
        let mut proof=PassingCertificate::new_retained(true);proof.exact_pair_limit_bytes=48;
        proof.include([1.,0.,0.],[1.1,0.,0.]);assert_eq!(proof.exact_pairs.as_ref().unwrap().len(),1);
        proof.include([0.,1.,0.],[0.,1.1,0.]);assert!(proof.exact_pairs.is_none());assert_eq!(proof.exact_pair_reserved_bytes,0);
        assert_eq!(passing_reservation_total(RETAINED_PASSING_AGGREGATE_LIMIT,1),None);assert_eq!(passing_reservation_total(usize::MAX,1),None);
        assert_eq!(passing_reservation_total(RETAINED_PASSING_AGGREGATE_LIMIT-48,48),Some(RETAINED_PASSING_AGGREGATE_LIMIT));
        let reference=PassingCertificate::new();assert!(reference.exact_pairs.is_none());
    }
    #[test]
    fn retained_mesh_acceptance_matches_original_cold_full_bits_and_decisions() {
        let mut area=AreaInstruction::new(vec![WorldPoint::new(-3.,48.5),WorldPoint::new(-2.8,48.5),WorldPoint::new(-2.8,48.7),WorldPoint::new(-3.,48.7)]).with_solid_fill(Color::BLACK);
        area.interiors.push(vec![WorldPoint::new(-2.97,48.54),WorldPoint::new(-2.97,48.66),WorldPoint::new(-2.83,48.66),WorldPoint::new(-2.83,48.54)]);
        let c=camera(48.6,-2.9,500000.);
        let build=|c:&GlobeCamera,retain| drape_area_internal_retained(&area.exterior,&area.interiors,[0.,0.,0.,1.],c,Default::default(),true,None,None,None,false,false,retain);
        let (a,sa,ca,_)=build(&c,false).unwrap();let (b,sb,cb,_)=build(&c,true).unwrap();
        assert_mesh_bits(&a,&b);assert_eq!(format!("{sa:?}"),format!("{sb:?}"));assert_cache_transcript(&ca,&cb);
        let cached=cb.unwrap();let pairs=cached.passing.exact_pairs.as_ref().expect("bounded fixture complete");assert!(!pairs.is_empty());
        assert!(pairs.capacity()*std::mem::size_of::<([f64;3],[f64;3])>()<=2*1024*1024);
        let mut accepted=0;
        for range in [500000.,505000.,450000.,600000.,250000.,80000.] {
            let view=camera(48.6,-2.9,range);
            if let Some(mesh)=cached.reusable_mesh(&view,[0.,0.,0.,1.]).unwrap() {
                for &(actual,linear) in pairs {assert!(deviation(&view,actual,linear).unwrap().0<=0.25);}
                let cold=build(&view,false).unwrap();assert_mesh_bits(mesh,&cold.0);accepted+=1;
            }
        }
        assert!(accepted>0);
    }
    fn assert_cache_transcript(a:&Option<CachedArea>,b:&Option<CachedArea>) {
        assert_eq!(a.is_some(),b.is_some());
        if let (Some(a),Some(b))=(a,b) {
            assert_eq!(a.probes.len(),b.probes.len());
            for (a,b) in a.probes.iter().zip(&b.probes) {assert_eq!(a.actual.map(f64::to_bits),b.actual.map(f64::to_bits));assert_eq!(a.linear.map(f64::to_bits),b.linear.map(f64::to_bits));assert_eq!(a.exceeds_screen,b.exceeds_screen);}
            assert_eq!(a.passing.bounds.map(|x|x.map(f64::to_bits)),b.passing.bounds.map(|x|x.map(f64::to_bits)));assert_eq!(a.passing.chord.to_bits(),b.passing.chord.to_bits());
        }
    }
    #[test]
    fn topology_only_matches_full_cold_bits_and_probes_without_hit_recapture() {
        for dateline in [false,true] {
            let (lat,lon)=if dateline {(61.,180.)}else{(48.6,-2.9)};
            let (l,r)=if dateline {(179.,181.)}else{(-3.,-2.8)};
            let mut area=AreaInstruction::new(vec![WorldPoint::new(l,lat-0.1),WorldPoint::new(r,lat-0.1),WorldPoint::new(r,lat+0.1),WorldPoint::new(l,lat+0.1)]).with_solid_fill(Color::BLACK);
            area.interiors.push(vec![WorldPoint::new(l+0.02,lat-0.04),WorldPoint::new(l+0.02,lat+0.04),WorldPoint::new(r-0.02,lat+0.04),WorldPoint::new(r-0.02,lat-0.04)]);
            let mut cache=AreaSourceCache::default();cache.set_topology_only(true);
            let budget=cache.begin(true,7,1,&[true],&[true]).unwrap();
            let (_,_,_,cap)=drape_area_cached_source(&area,&camera(lat,lon,500000.),Default::default(),None,None,Some(&budget)).unwrap();cache.commit(0,cap.unwrap());
            assert!(cache.get(0).unwrap().ecef.is_empty());
            for range in [505000.,250000.,80000.,10000.] {
                let c=camera(lat,lon,range);
                let a=drape_area_cached_source(&area,&c,Default::default(),None,None,None);
                let b=drape_area_cached_source(&area,&c,Default::default(),None,cache.get(0),Some(&budget));
                match(a,b) {(Ok((a,sa,ca,_)),Ok((b,sb,cb,cap)))=>{assert_mesh_bits(&a,&b);assert_eq!(format!("{sa:?}"),format!("{sb:?}"));assert_cache_transcript(&ca,&cb);assert!(cap.is_none());},(Err(a),Err(b))=>assert_eq!(a,b),_=>panic!("topology outcome mismatch")}
            }
            assert_eq!(budget.captures.load(std::sync::atomic::Ordering::Relaxed),1);
            let bad=DrapingLimits{max_vertices:3,..Default::default()};let c=camera(lat,lon,500000.);
            assert_eq!(drape_area_cached_source(&area,&c,bad,None,None,None).err(),drape_area_cached_source(&area,&c,bad,None,cache.get(0),Some(&budget)).err());
            area.fill=AreaFillType::Solid(Color::rgba(f32::NAN,0.,0.,1.));
            assert_eq!(drape_area_cached_source(&area,&c,Default::default(),None,None,None).err(),drape_area_cached_source(&area,&c,Default::default(),None,cache.get(0),Some(&budget)).err());
        }
    }
    #[test]
    fn topology_policy_epoch_count_and_ordinal_changes_drop_owned_sources() {
        let mut cache=AreaSourceCache::default();cache.set_topology_only(true);
        let area=AreaInstruction::new(vec![WorldPoint::new(-3.,48.5),WorldPoint::new(-2.8,48.5),WorldPoint::new(-2.8,48.7),WorldPoint::new(-3.,48.7)]).with_solid_fill(Color::BLACK);
        let c=camera(48.6,-2.9,500000.);let budget=cache.begin(true,11,2,&[true,true],&[true,true]).unwrap();
        let (_,_,_,cap)=drape_area_cached_source(&area,&c,Default::default(),None,None,Some(&budget)).unwrap();cache.commit(1,cap.unwrap());assert!(cache.get(0).is_none());assert!(cache.get(1).is_some());
        cache.begin(true,12,2,&[true,true],&[true,true]).unwrap();assert!(cache.get(1).is_none());
        cache.begin(true,12,1,&[true],&[true]).unwrap();assert_eq!(cache.slots.len(),1);
        cache.set_topology_only(false);assert!(cache.slots.is_empty());assert_eq!(cache.bytes,0);
        cache.begin(false,12,1,&[true],&[true]);assert!(cache.slots.is_empty());
        assert!(cache.begin(true,13,usize::MAX,&[],&[]).is_none());assert_eq!(cache.bytes,0);
    }
    #[test]
    fn topology_memory_reservation_refusal_and_eviction_keep_cold_fallback() {
        let mut cache=AreaSourceCache::default();cache.set_topology_only(true);let budget=cache.begin(true,1,1,&[true],&[true]).unwrap();
        assert_eq!(AreaSource::required_bytes(usize::MAX,1,0),usize::MAX);
        assert!(budget.reserve(usize::MAX).is_none());assert!(budget.reserve(AreaSourceBudget::LIMIT).is_none());
        let remaining=AreaSourceBudget::LIMIT-budget.used.load(std::sync::atomic::Ordering::Relaxed);let reservation=budget.reserve(remaining).unwrap();assert!(budget.reserve(1).is_none());drop(reservation);
        let area=AreaInstruction::new(vec![WorldPoint::new(-3.,48.5),WorldPoint::new(-2.8,48.5),WorldPoint::new(-2.8,48.7),WorldPoint::new(-3.,48.7)]).with_solid_fill(Color::BLACK);let c=camera(48.6,-2.9,500000.);
        let (a,_,_,cap)=drape_area_cached_source(&area,&c,Default::default(),None,None,Some(&budget)).unwrap();cache.commit(0,cap.unwrap());
        cache.requested=AreaSourceBudget::LIMIT-cache.base_bytes();cache.begin(true,1,1,&[true],&[true]).unwrap();assert!(cache.get(0).is_none());assert_eq!(cache.evictions,1);
        assert_mesh_bits(&a,&drape_area_cached_source(&area,&c,Default::default(),None,cache.get(0),None).unwrap().0);
        assert!(budget.peak.load(std::sync::atomic::Ordering::Relaxed)<=AreaSourceBudget::LIMIT);
    }
    fn assert_mesh_bits(a: &GlobeMesh, b: &GlobeMesh) {
        assert_eq!(a.indices, b.indices);
        assert_eq!(a.vertices.len(), b.vertices.len());
        for (a, b) in a.vertices.iter().zip(&b.vertices) {
            assert_eq!(a.ecef_m.map(f64::to_bits), b.ecef_m.map(f64::to_bits));
            assert_eq!(a.color.map(f32::to_bits), b.color.map(f32::to_bits));
        }
    }
    #[test]
    fn immutable_source_matches_reference_bits_across_views_colors_and_holes() {
        let mut fixtures = vec![
            AreaInstruction::new(vec![
                WorldPoint::new(-3., 48.5),
                WorldPoint::new(-2.8, 48.5),
                WorldPoint::new(-2.8, 48.7),
                WorldPoint::new(-3., 48.7),
            ])
            .with_solid_fill(Color::BLACK),
            AreaInstruction::new(vec![
                WorldPoint::new(179., 60.),
                WorldPoint::new(-179., 60.),
                WorldPoint::new(-179., 62.),
                WorldPoint::new(179., 62.),
            ])
            .with_solid_fill(Color::BLACK),
        ];
        fixtures[1].interiors.push(vec![
            WorldPoint::new(179.5, 60.5),
            WorldPoint::new(179.5, 61.5),
            WorldPoint::new(-179.5, 61.5),
            WorldPoint::new(-179.5, 60.5),
        ]);
        for (id, mut area) in fixtures.into_iter().enumerate() {
            let (lat, lon) = if id == 0 { (48.6, -2.9) } else { (61., 180.) };
            let mut cache = AreaSourceCache::default();
            let budget = cache.begin(true, 7, 1, &[true], &[true]).unwrap();
            let c = camera(lat, lon, 500000.);
            let (first, _, _, capture) =
                drape_area_cached_source(&area, &c, Default::default(), None, None, Some(&budget))
                    .unwrap();
            assert_mesh_bits(
                &first,
                &drape_area(&area, &c, Default::default()).unwrap().0,
            );
            cache.commit(0, capture.unwrap());
            for range in [505000., 250000., 80000., 10000.] {
                area.fill = AreaFillType::Solid(Color::rgba(0.2, 0.3, 0.4, 0.5));
                let c = camera(lat, lon, range);
                let reference = drape_area(&area, &c, Default::default());
                let actual = drape_area_cached_source(
                    &area,
                    &c,
                    Default::default(),
                    None,
                    cache.get(0),
                    Some(&budget),
                );
                match (reference, actual) {
                    (Ok((mesh, stats)), Ok((warm, wstats, _, capture))) => {
                        assert!(capture.is_none());
                        assert_mesh_bits(&mesh, &warm);
                        assert_eq!(stats.refinements, wstats.refinements);
                        assert_eq!(
                            stats.max_edge_error_px.to_bits(),
                            wstats.max_edge_error_px.to_bits()
                        );
                        assert_eq!(
                            stats.max_edge_chord_error_m.to_bits(),
                            wstats.max_edge_chord_error_m.to_bits()
                        );
                    }
                    (Err(a), Err(b)) => assert_eq!(a, b),
                    _ => panic!("source cache changed outcome"),
                }
            }
            let limits = DrapingLimits {
                max_vertices: 3,
                ..Default::default()
            };
            assert_eq!(
                drape_area(&area, &c, limits).err(),
                drape_area_cached_source(&area, &c, limits, None, cache.get(0), Some(&budget))
                    .err()
            );
            assert!(
                budget.peak.load(std::sync::atomic::Ordering::Relaxed) <= AreaSourceBudget::LIMIT
            );
        }
    }
    #[test]
    fn culled_source_retains_topology_then_upgrades_only_when_visible() {
        let area = AreaInstruction::new(vec![
            WorldPoint::new(-3., 48.5),
            WorldPoint::new(-2.8, 48.5),
            WorldPoint::new(-2.8, 48.7),
            WorldPoint::new(-3., 48.7),
        ])
        .with_solid_fill(Color::BLACK);
        let mut cache = AreaSourceCache::default();
        let budget = cache.begin(true, 1, 1, &[true], &[true]).unwrap();
        let c = camera(48.6, -4., 1000.);
        let (_, stats, _, capture) =
            drape_area_cached_source(&area, &c, Default::default(), None, None, Some(&budget))
                .unwrap();
        assert!(stats.frustum_culled);
        cache.commit(0, capture.unwrap());
        assert!(cache.get(0).unwrap().ecef.is_empty());
        let c = camera(48.6, -2.9, 500000.);
        let (warm, _, _, capture) = drape_area_cached_source(
            &area,
            &c,
            Default::default(),
            None,
            cache.get(0),
            Some(&budget),
        )
        .unwrap();
        assert_mesh_bits(&warm, &drape_area(&area, &c, Default::default()).unwrap().0);
        cache.commit(0, capture.unwrap());
        assert!(!cache.get(0).unwrap().ecef.is_empty());
        cache.finish(Some(&budget));
        cache.begin(true, 2, 1, &[true], &[true]).unwrap();
        assert!(cache.get(0).is_none());
        cache.begin(false, 2, 1, &[true], &[true]);
        assert_eq!(cache.bytes, 0);
        assert_eq!(cache.slots.capacity(), 0);
    }
    #[test]
    fn source_reservations_bound_concurrent_pending_and_release_on_drop() {
        use std::sync::atomic::Ordering::Relaxed;
        let mut cache = AreaSourceCache::default();
        let budget = cache.begin(true, 1, 1, &[true], &[true]).unwrap();
        let base = budget.used.load(Relaxed);
        let first = budget.reserve(AreaSourceBudget::LIMIT - base).unwrap();
        assert!(budget.reserve(1).is_none());
        assert_eq!(budget.peak.load(Relaxed), AreaSourceBudget::LIMIT);
        drop(first);
        assert_eq!(budget.used.load(Relaxed), base);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles = (0..8)
            .map(|_| {
                let b = budget.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let r = b.reserve((AreaSourceBudget::LIMIT - base) / 4);
                    barrier.wait();
                    drop(r);
                })
            })
            .collect::<Vec<_>>();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(budget.used.load(Relaxed), base);
        assert!(budget.peak.load(Relaxed) <= AreaSourceBudget::LIMIT);
    }
    #[test]
    fn demand_eviction_prefers_nonvisible_and_oversize_does_not_flush_cache() {
        let mut cache = AreaSourceCache::default();
        let budget = cache
            .begin(true, 1, 2, &[true, true], &[true, true])
            .unwrap();
        let uv = [[1., 2.], [3., 4.], [5., 6.]];
        for i in 0..2 {
            cache.commit(
                i,
                AreaSource::capture(&uv, &[0, 1, 2], None, &[], Some(&budget)).unwrap(),
            );
        }
        let entry_bytes = cache.get(0).unwrap().bytes();
        cache.requested = AreaSourceBudget::LIMIT - cache.base_bytes() - entry_bytes;
        let budget = cache
            .begin(true, 1, 2, &[true, false], &[true, true])
            .unwrap();
        assert!(cache.get(0).is_some());
        assert!(cache.get(1).is_none());
        assert_eq!(cache.evictions, 1);
        assert!(budget.reserve(AreaSourceBudget::LIMIT + 1).is_none());
        cache.finish(Some(&budget));
        assert_eq!(cache.requested, 0);
        cache.begin(true, 1, 2, &[true, true], &[true, true]);
        assert!(cache.get(0).is_some());
        assert_eq!(cache.evictions, 1);
    }
}

#[cfg(test)]
mod bounded_coverage_capture_tests {
    use super::*;
    fn camera()->GlobeCamera {
        GlobeCamera::orbit(ferrite_kernel::geodesy::GeographicPosition::new(48.,0.).unwrap(),50000.,0.,35.,[640.,480.],45.,3.,1e9).unwrap()
    }
    fn area()->AreaInstruction {
        AreaInstruction::new(vec![WorldPoint::new(-0.01,47.99),WorldPoint::new(0.01,47.99),WorldPoint::new(0.01,48.01),WorldPoint::new(-0.01,48.01)]).with_solid_fill(ferrite_render::Color::WHITE)
    }
    fn exact(a:&GlobeMesh,b:&GlobeMesh) {
        assert_eq!(a.indices,b.indices);assert_eq!(a.vertices.len(),b.vertices.len());
        for (a,b) in a.vertices.iter().zip(&b.vertices){assert_eq!(a.ecef_m.map(f64::to_bits),b.ecef_m.map(f64::to_bits));assert_eq!(a.color.map(f32::to_bits),b.color.map(f32::to_bits));}
    }
    #[test]
    fn bounded_capture_refusal_preserves_original_mesh_and_errors() {
        let a=area();let c=camera();let limits=DrapingLimits::default();let original=drape_area(&a,&c,limits).unwrap().0;
        for budget in [0,1,96,4096] {
            let (m,_,cached)=drape_area_cached_bounded(&a,&c,limits,budget).unwrap();exact(&m,&original);
            if let Some(cached)=cached {assert!(cached.bytes()<=budget);}
        }
        let bad=DrapingLimits{screen_error_px:f64::NAN,..limits};
        assert_eq!(drape_area(&a,&c,bad).unwrap_err(),drape_area_cached_bounded(&a,&c,bad,0).err().unwrap());
    }
    #[test]
    fn admitted_capture_is_under_reserved_cap_and_full_bits_original() {
        let a=area();let c=camera();let budget=16*1024*1024;
        let (m,_,cache)=drape_area_cached_bounded(&a,&c,DrapingLimits::default(),budget).unwrap();
        exact(&m,&drape_area(&a,&c,DrapingLimits::default()).unwrap().0);
        if let Some(cache)=cache {assert!(cache.bytes()<=budget);}
    }
}

#[cfg(test)]
mod coverage_midpoint_reuse_tests {
    use super::*;
    fn exact(a:&GlobeMesh,b:&GlobeMesh) {
        assert_eq!(a.indices,b.indices);assert_eq!(a.vertices.len(),b.vertices.len());
        for (a,b) in a.vertices.iter().zip(&b.vertices){assert_eq!(a.ecef_m.map(f64::to_bits),b.ecef_m.map(f64::to_bits));assert_eq!(a.color.map(f32::to_bits),b.color.map(f32::to_bits));}
    }
    #[test]
    fn original_edge_sample_reuse_preserves_mesh_errors_holes_and_refusal() {
        let mut a=AreaInstruction::new(vec![WorldPoint::new(-1.,47.),WorldPoint::new(1.,47.),WorldPoint::new(1.,49.),WorldPoint::new(-1.,49.)]).with_solid_fill(ferrite_render::Color::WHITE);
        a.interiors.push(vec![WorldPoint::new(-0.2,47.8),WorldPoint::new(-0.2,48.2),WorldPoint::new(0.2,48.2),WorldPoint::new(0.2,47.8)]);
        let mut reused=0;
        for (range,heading,extent) in [(200000.,0.,[640.,480.]),(50000.,45.,[1280.,960.]),(200000.,90.,[640.,480.])] {
            let c=GlobeCamera::orbit(GeographicPosition::new(48.,0.).unwrap(),range,heading,35.,extent,45.,3.,1e9).unwrap();
            for cap in [0,16*1024*1024] {
                let (off,a_stats,_)=drape_area_cached_bounded_midpoints(&a,&c,DrapingLimits::default(),cap,false).unwrap();
                let (on,b_stats,_)=drape_area_cached_bounded_midpoints(&a,&c,DrapingLimits::default(),cap,true).unwrap();
                exact(&off,&on);assert_eq!(a_stats.refinements,b_stats.refinements);assert_eq!(a_stats.source_triangles,b_stats.source_triangles);assert_eq!(a_stats.max_edge_error_px.to_bits(),b_stats.max_edge_error_px.to_bits());assert_eq!(a_stats.max_edge_chord_error_m.to_bits(),b_stats.max_edge_chord_error_m.to_bits());reused+=b_stats.midpoint_ecef_reused;
            }
        }
        assert!(reused>0);
    }
    #[test]
    fn midpoint_reuse_invalid_limits_preserve_original_error_order() {
        let a=AreaInstruction::new(vec![WorldPoint::new(0.,48.),WorldPoint::new(1.,48.),WorldPoint::new(0.,49.)]).with_solid_fill(ferrite_render::Color::WHITE);
        let c=GlobeCamera::orbit(GeographicPosition::new(48.,0.).unwrap(),200000.,0.,35.,[640.,480.],45.,3.,1e9).unwrap();
        for limits in [DrapingLimits{screen_error_px:f64::NAN,..Default::default()},DrapingLimits{max_vertices:2,..Default::default()}] {
            assert_eq!(drape_area_cached_bounded_midpoints(&a,&c,limits,0,false).err(),drape_area_cached_bounded_midpoints(&a,&c,limits,0,true).err());
        }
    }
}
