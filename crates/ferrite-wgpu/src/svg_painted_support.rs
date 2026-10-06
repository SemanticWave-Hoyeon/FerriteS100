//! SVG fill/stroke support adapter for independent whole motifs.
//!
//! Uses the same parsed usvg tree as raster resources, converts its absolute
//! transforms to pivot-relative physical pixels, and retains every nondegenerate
//! tessellated triangle independently (no lossy set union). The configured Lyon
//! curve tolerance is NOT a certification of combined SVG stroke/transform/AA
//! error. The eventual whole-motif renderer must satisfy that separate contract.
use ferrite_kernel::whole_symbol::{PaintedSymbolSupport, ShapeLimits};
use lyon_tessellation::{
    geometry_builder::{FillGeometryBuilder, GeometryBuilder, GeometryBuilderError},
    math::point,
    path::Path,
    FillOptions, FillRule, FillTessellator, FillVertex, VertexId,
};
use resvg::{tiny_skia, usvg};

#[derive(Debug, Clone, Copy)]
pub struct SvgSupportLimits {
    pub max_nodes: usize,
    pub max_depth: usize,
    pub max_segments: usize,
    pub max_vertices: usize,
    pub max_triangles: usize,
    pub max_dash_entries: usize,
    pub configured_curve_tolerance_px: f32,
}
impl Default for SvgSupportLimits {
    fn default() -> Self {
        Self {
            max_nodes: 4096,
            max_depth: 128,
            max_segments: 16384,
            max_vertices: 32768,
            max_triangles: 65536,
            max_dash_entries: 256,
            configured_curve_tolerance_px: 0.0625,
        }
    }
}
impl SvgSupportLimits {
    pub fn validate(self) -> Result<(), String> {
        if !self.configured_curve_tolerance_px.is_finite()
            || self.configured_curve_tolerance_px < 1e-5
            || self.max_vertices >= u32::MAX as usize
            || self.max_triangles.checked_mul(4).is_none()
        {
            return Err("Invalid SVG support quality/geometry limits".into());
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct SvgPaintedSupport {
    pub support: PaintedSymbolSupport,
    pub triangle_count: usize,
    pub path_count: usize,
    pub bounds_px: [f64; 4],
    /// Owned geometry payload/header estimate, excluding allocator/geo scratch.
    pub retained_payload_bytes: usize,
    pub configured_curve_tolerance_px: f32,
}

struct Output {
    vertices: Vec<[f64; 2]>,
    triangles: Vec<[[f64; 2]; 3]>,
    limits: SvgSupportLimits,
    overflow: bool,
    checkpoint: [usize; 2],
}
impl GeometryBuilder for Output {
    fn begin_geometry(&mut self) {
        self.checkpoint = [self.vertices.len(), self.triangles.len()];
    }
    fn add_triangle(&mut self, a: VertexId, b: VertexId, c: VertexId) {
        if self.triangles.len() >= self.limits.max_triangles {
            self.overflow = true;
            return;
        }
        let triangle = [
            self.vertices[a.0 as usize],
            self.vertices[b.0 as usize],
            self.vertices[c.0 as usize],
        ];
        let area = (triangle[1][0] - triangle[0][0]) * (triangle[2][1] - triangle[0][1])
            - (triangle[1][1] - triangle[0][1]) * (triangle[2][0] - triangle[0][0]);
        if !area.is_finite() {
            self.overflow = true;
            return;
        }
        // Exact zero-area triangles carry no filled support. No positive-area
        // epsilon silently discards a thin component.
        if area != 0. {
            self.triangles.push(triangle);
        }
    }
    fn abort_geometry(&mut self) {
        self.vertices.truncate(self.checkpoint[0]);
        self.triangles.truncate(self.checkpoint[1]);
    }
}
impl FillGeometryBuilder for Output {
    fn add_fill_vertex(&mut self, v: FillVertex) -> Result<VertexId, GeometryBuilderError> {
        if self.overflow || self.vertices.len() >= self.limits.max_vertices {
            return Err(GeometryBuilderError::TooManyVertices);
        }
        let p = v.position();
        if !p.x.is_finite() || !p.y.is_finite() {
            return Err(GeometryBuilderError::InvalidVertex);
        }
        let id = VertexId(self.vertices.len() as u32);
        self.vertices.push([f64::from(p.x), f64::from(p.y)]);
        Ok(id)
    }
}
struct State {
    output: Output,
    nodes: usize,
    segments: usize,
    paths: usize,
    pivot: [f64; 2],
    pixels_per_svg_px: f64,
}
impl State {
    fn point(
        &self,
        p: tiny_skia::Point,
        t: tiny_skia::Transform,
    ) -> Result<lyon_tessellation::math::Point, String> {
        let x =
            (f64::from(t.sx) * f64::from(p.x) + f64::from(t.kx) * f64::from(p.y) + f64::from(t.tx)
                - self.pivot[0])
                * self.pixels_per_svg_px;
        let y =
            (f64::from(t.ky) * f64::from(p.x) + f64::from(t.sy) * f64::from(p.y) + f64::from(t.ty)
                - self.pivot[1])
                * self.pixels_per_svg_px;
        let xf = x as f32;
        let yf = y as f32;
        let limit = f64::from(self.output.limits.configured_curve_tolerance_px) * 0.25;
        if !x.is_finite()
            || !y.is_finite()
            || !xf.is_finite()
            || !yf.is_finite()
            || (f64::from(xf) - x).abs() > limit
            || (f64::from(yf) - y).abs() > limit
        {
            return Err("SVG support transform precision limit exceeded".into());
        }
        Ok(point(xf, yf))
    }
    fn tessellate(
        &mut self,
        path: &tiny_skia::Path,
        t: tiny_skia::Transform,
        rule: FillRule,
    ) -> Result<(), String> {
        let count = path.segments().count();
        self.segments = self
            .segments
            .checked_add(count)
            .ok_or("SVG support segment overflow")?;
        if self.segments > self.output.limits.max_segments {
            return Err("SVG support segment budget exceeded".into());
        }
        let mut builder = Path::builder();
        let mut started = false;
        for segment in path.segments() {
            match segment {
                tiny_skia::PathSegment::MoveTo(p) => {
                    if started {
                        builder.end(true);
                    }
                    builder.begin(self.point(p, t)?);
                    started = true;
                }
                tiny_skia::PathSegment::LineTo(p) => {
                    builder.line_to(self.point(p, t)?);
                }
                tiny_skia::PathSegment::QuadTo(a, b) => {
                    builder.quadratic_bezier_to(self.point(a, t)?, self.point(b, t)?);
                }
                tiny_skia::PathSegment::CubicTo(a, b, c) => {
                    builder.cubic_bezier_to(
                        self.point(a, t)?,
                        self.point(b, t)?,
                        self.point(c, t)?,
                    );
                }
                tiny_skia::PathSegment::Close => {
                    if started {
                        builder.end(true);
                        started = false;
                    }
                }
            }
        }
        if started {
            builder.end(true);
        }
        let path = builder.build();
        let options = FillOptions::default()
            .with_fill_rule(rule)
            .with_tolerance(self.output.limits.configured_curve_tolerance_px);
        FillTessellator::new()
            .tessellate_path(&path, &options, &mut self.output)
            .map_err(|e| format!("SVG support tessellation failed: {e}"))?;
        if self.output.overflow {
            return Err("SVG support triangle budget/finite area exceeded".into());
        }
        Ok(())
    }
    fn group(&mut self, g: &usvg::Group, depth: usize) -> Result<(), String> {
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or("SVG support node overflow")?;
        if depth > self.output.limits.max_depth || self.nodes > self.output.limits.max_nodes {
            return Err("SVG support node/depth budget exceeded".into());
        }
        if g.opacity().get() == 0. {
            return Ok(());
        }
        if g.clip_path().is_some()
            || g.mask().is_some()
            || !g.filters().is_empty()
            || g.blend_mode() != usvg::BlendMode::Normal
        {
            return Err(
                "Whole SVG support requires independent clip/mask/filter/blend handling".into(),
            );
        }
        for node in g.children() {
            match node {
                usvg::Node::Group(group) => self.group(
                    group,
                    depth.checked_add(1).ok_or("SVG support depth overflow")?,
                )?,
                usvg::Node::Path(path) => {
                    self.nodes = self
                        .nodes
                        .checked_add(1)
                        .ok_or("SVG support node overflow")?;
                    if self.nodes > self.output.limits.max_nodes {
                        return Err("SVG support node budget exceeded".into());
                    }
                    if !path.is_visible() {
                        continue;
                    }
                    self.paths += 1;
                    let transform = path.abs_transform();
                    if let Some(fill) = path.fill().filter(|f| f.opacity().get() > 0.) {
                        color_paint(fill.paint())?;
                        self.tessellate(
                            path.data(),
                            transform,
                            match fill.rule() {
                                usvg::FillRule::NonZero => FillRule::NonZero,
                                usvg::FillRule::EvenOdd => FillRule::EvenOdd,
                            },
                        )?;
                    }
                    if let Some(stroke) = path.stroke().filter(|s| s.opacity().get() > 0.) {
                        color_paint(stroke.paint())?;
                        // Exact zero-length butt-capped subpaths paint nothing.
                        // The real PC has these in WRECKS04/TIDCUR02; treating
                        // tiny-skia's None as a general success would hide actual
                        // numeric failures in nonzero paths. Round/square caps
                        // must still produce their painted endpoint support.
                        if stroke.linecap() == usvg::LineCap::Butt
                            && path_length_upper(path.data()) == 0.
                        {
                            continue;
                        }
                        let count = path.data().segments().count();
                        if count > self.output.limits.max_segments {
                            return Err("SVG support source stroke segment budget exceeded".into());
                        }
                        let norm = (f64::from(transform.sx).powi(2)
                            + f64::from(transform.kx).powi(2)
                            + f64::from(transform.ky).powi(2)
                            + f64::from(transform.sy).powi(2))
                        .sqrt()
                            * self.pixels_per_svg_px;
                        let resolution = (norm
                            / (4. * f64::from(self.output.limits.configured_curve_tolerance_px)))
                        .max(1.);
                        if !resolution.is_finite() || resolution > 1e6 {
                            return Err("SVG support stroke quality limit exceeded".into());
                        }
                        if stroke
                            .dasharray()
                            .is_some_and(|d| d.len() > self.output.limits.max_dash_entries)
                        {
                            return Err("SVG support dash entry budget exceeded".into());
                        }
                        let mut style = stroke.to_tiny_skia();
                        let dashed = if let Some(dash) = style.dash.as_ref() {
                            let array = stroke
                                .dasharray()
                                .ok_or("SVG support dash source missing")?;
                            dash_admission(path.data(), array, self.output.limits)?;
                            Some(
                                path.data()
                                    .dash(dash, resolution as f32)
                                    .ok_or("SVG support dash expansion failed")?,
                            )
                        } else {
                            None
                        };
                        let input = dashed.as_ref().unwrap_or_else(|| path.data());
                        style.dash = None; // input has already been dashed once.
                        let remaining = self
                            .output
                            .limits
                            .max_segments
                            .saturating_sub(self.segments);
                        if input.segments().count() > remaining {
                            return Err("SVG support dashed segment budget exceeded".into());
                        }
                        let outlined = input
                            .stroke(&style, resolution as f32)
                            .ok_or("SVG support stroke outline failed")?;
                        if outlined.segments().count() > remaining {
                            return Err("SVG support outline segment budget exceeded".into());
                        }
                        self.tessellate(&outlined, transform, FillRule::NonZero)?;
                    }
                }
                _ => {
                    return Err("Whole SVG support requires independent image/text handling".into())
                }
            }
        }
        Ok(())
    }
}
fn color_paint(p: &usvg::Paint) -> Result<(), String> {
    if matches!(p, usvg::Paint::Color(_)) {
        Ok(())
    } else {
        Err("Whole SVG support requires independent gradient/pattern-alpha handling".into())
    }
}
fn dash_admission(
    path: &tiny_skia::Path,
    array: &[f32],
    limits: SvgSupportLimits,
) -> Result<(), String> {
    if array.is_empty() || array.len() > limits.max_dash_entries {
        return Err("SVG support dash entry budget exceeded".into());
    }
    let period: f64 = array.iter().map(|v| f64::from(*v)).sum();
    let contours = path
        .segments()
        .filter(|s| matches!(s, tiny_skia::PathSegment::MoveTo(_)))
        .count();
    // Include zero-length intervals, two partial cycles per contour and input
    // segment splits. Length/min_positive alone misses these costs entirely.
    let intervals = ((path_length_upper(path) / period).floor() + 2. * contours as f64 + 1.)
        * array.len() as f64;
    let estimates = 4. * (intervals + path.segments().count() as f64) + 3. * contours as f64;
    if !period.is_finite()
        || period <= 0.
        || array.iter().any(|v| !v.is_finite() || *v < 0.)
        || !estimates.is_finite()
        || estimates > limits.max_segments as f64
    {
        return Err("SVG support dash expansion budget exceeded".into());
    }
    Ok(())
}

fn path_length_upper(path: &tiny_skia::Path) -> f64 {
    let mut previous = [0., 0.];
    let mut start = previous;
    let mut total = 0.;
    let length = |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).hypot(a[1] - b[1]);
    let point = |p: tiny_skia::Point| [f64::from(p.x), f64::from(p.y)];
    for s in path.segments() {
        match s {
            tiny_skia::PathSegment::MoveTo(p) => {
                previous = point(p);
                start = previous;
            }
            tiny_skia::PathSegment::LineTo(p) => {
                let p = point(p);
                total += length(previous, p);
                previous = p;
            }
            tiny_skia::PathSegment::QuadTo(a, b) => {
                let a = point(a);
                let b = point(b);
                total += length(previous, a) + length(a, b);
                previous = b;
            }
            tiny_skia::PathSegment::CubicTo(a, b, c) => {
                let a = point(a);
                let b = point(b);
                let c = point(c);
                total += length(previous, a) + length(a, b) + length(b, c);
                previous = c;
            }
            tiny_skia::PathSegment::Close => {
                total += length(previous, start);
                previous = start;
            }
        }
    }
    total
}

/// `pivot` is user origin mapped through the root default xMidYMid-meet
/// viewport. `pixels_per_svg_px` converts usvg's 96DPI viewport to physical
/// display pixels; the resource owner uses its actual millimetre calibration.
/// None means no painted fill/stroke. Any unsupported component fails WHOLE SVG.
pub fn svg_painted_support(
    tree: &usvg::Tree,
    pivot: [f64; 2],
    pixels_per_svg_px: f64,
    limits: SvgSupportLimits,
) -> Result<Option<SvgPaintedSupport>, String> {
    limits.validate()?;
    if !pivot.iter().all(|c| c.is_finite())
        || !pixels_per_svg_px.is_finite()
        || pixels_per_svg_px <= 0.
    {
        return Err("Invalid SVG support physical calibration/pivot".into());
    }
    let mut state = State {
        output: Output {
            vertices: vec![],
            triangles: vec![],
            limits,
            overflow: false,
            checkpoint: [0, 0],
        },
        nodes: 0,
        segments: 0,
        paths: 0,
        pivot,
        pixels_per_svg_px,
    };
    state.group(tree.root(), 0)?;
    let triangles = &state.output.triangles;
    if triangles.is_empty() {
        return Ok(None);
    }
    let support = PaintedSymbolSupport::from_triangles(
        triangles,
        ShapeLimits {
            max_coordinates: limits
                .max_triangles
                .checked_mul(4)
                .ok_or("SVG support coordinate overflow")?,
            max_components: limits.max_triangles,
            max_rings: limits.max_triangles,
        },
    )
    .map_err(|e| e.to_string())?;
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for t in triangles {
        for p in t {
            bounds[0] = bounds[0].min(p[0]);
            bounds[1] = bounds[1].min(p[1]);
            bounds[2] = bounds[2].max(p[0]);
            bounds[3] = bounds[3].max(p[1]);
        }
    }
    let retained_payload_bytes = triangles
        .len()
        .checked_mul(112)
        .and_then(|n| n.checked_add(128))
        .ok_or("SVG support retained payload overflow")?;
    Ok(Some(SvgPaintedSupport {
        support,
        triangle_count: triangles.len(),
        path_count: state.paths,
        bounds_px: bounds,
        retained_payload_bytes,
        configured_curve_tolerance_px: limits.configured_curve_tolerance_px,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_kernel::whole_symbol::{
        select_whole_symbols, SymbolSite, WholeSymbolArea, WholeSymbolLimits,
    };
    fn parsed(body: &str) -> usvg::Tree {
        usvg::Tree::from_str(&format!("<svg xmlns='http://www.w3.org/2000/svg' width='4mm' height='4mm' viewBox='-2 -2 4 4'>{body}</svg>"),&usvg::Options::default()).unwrap()
    }
    fn load(body: &str, ppm: f64) -> Option<SvgPaintedSupport> {
        let tree = parsed(body);
        let size = tree.size();
        svg_painted_support(
            &tree,
            [f64::from(size.width()) / 2., f64::from(size.height()) / 2.],
            ppm / f64::from(96f32 / 25.4),
            SvgSupportLimits::default(),
        )
        .unwrap()
    }
    fn contains(s: &SvgPaintedSupport, outer: &[[f64; 2]], holes: &[Vec<[f64; 2]>]) -> bool {
        let area = WholeSymbolArea::from_rings(
            outer,
            holes,
            ShapeLimits {
                max_coordinates: 1000,
                max_components: 10,
                max_rings: 100,
            },
        )
        .unwrap();
        select_whole_symbols(
            &area,
            &s.support,
            &[SymbolSite {
                source_ordinal: 42,
                lattice_index: [-3, 7],
                origin: [0., 0.],
            }],
            WholeSymbolLimits {
                max_sites: 10,
                max_cross_coordinate_pairs: 10_000_000,
                max_support_coordinate_pairs: 100_000_000,
                max_decision_bytes: 1000,
                max_translation_error: 0.,
            },
        )
        .unwrap()[0]
            .completely_contained
    }
    fn near(actual: [f64; 4], expected: [f64; 4], epsilon: f64) {
        for (a, e) in actual.into_iter().zip(expected) {
            assert!((a - e).abs() <= epsilon, "{actual:?} vs {expected:?}");
        }
    }
    #[test]
    fn asymmetric_physical_pivot_transform_and_hidden_engineering_shapes() {
        let s=load("<g display='none'><rect x='-100' y='-100' width='200' height='200'/></g><rect x='.25' y='-1' width='1.25' height='1'/>",4.).unwrap();
        near(s.bounds_px, [1., -4., 6., 0.], 1e-5);
        assert_eq!(s.path_count, 1);
        assert_eq!(s.triangle_count, 2);
        let transformed = load(
            "<g transform='translate(0.5,-0.25) scale(2,0.5)'><rect width='.5' height='.5'/></g>",
            4.,
        )
        .unwrap();
        near(transformed.bounds_px, [2., -1., 6., 0.], 1e-5);
    }
    #[test]
    fn fill_rule_holes_are_geometric_support_not_viewport() {
        let body = |rule| format!("<path fill-rule='{rule}' d='M-2 -2H2V2H-2Z M-1 -1H1V1H-1Z'/>");
        let even = load(&body("evenodd"), 1.).unwrap();
        let nonzero = load(&body("nonzero"), 1.).unwrap();
        let outer = [
            [-2.1, -2.1],
            [2.1, -2.1],
            [2.1, 2.1],
            [-2.1, 2.1],
            [-2.1, -2.1],
        ];
        let holes = vec![vec![
            [-0.75, -0.75],
            [0.75, -0.75],
            [0.75, 0.75],
            [-0.75, 0.75],
            [-0.75, -0.75],
        ]];
        assert!(contains(&even, &outer, &holes));
        assert!(!contains(&nonzero, &outer, &holes));
    }
    #[test]
    fn open_fill_closes_and_overlapping_shapes_are_independent() {
        let s = load(
            "<path d='M-1 0L1 0L0 1'/><rect x='-.5' width='1' height='.5'/>",
            1.,
        )
        .unwrap();
        assert_eq!(s.path_count, 2);
        assert!(s.triangle_count >= 3);
        assert!(contains(
            &s,
            &[
                [-1.1, -0.1],
                [1.1, -0.1],
                [1.1, 1.1],
                [-1.1, 1.1],
                [-1.1, -0.1]
            ],
            &[]
        ));
        let bow = load("<path d='M-1 -1L1 1L-1 1L1 -1Z'/>", 1.).unwrap();
        assert!(bow.triangle_count >= 2);
    }
    #[test]
    fn stroke_width_butt_square_and_round_caps_have_real_extent() {
        for (cap, bounds, tolerance) in [
            ("butt", [-4., -1., 4., 1.], 1e-5),
            ("square", [-5., -1., 5., 1.], 1e-5),
            ("round", [-5., -1., 5., 1.], 0.07),
        ] {
            let s=load(&format!("<path fill='none' stroke='red' stroke-width='.5' stroke-linecap='{cap}' d='M-1 0L1 0'/>"),4.).unwrap();
            near(s.bounds_px, bounds, tolerance);
        }
    }
    #[test]
    fn zero_length_butt_is_empty_but_round_and_square_caps_remain_painted() {
        assert!(load(
            "<path fill='none' stroke='red' stroke-width='.5' d='M0 0L0 0'/>",
            4.
        )
        .is_none());
        for cap in ["round", "square"] {
            let s=load(&format!("<path fill='none' stroke='red' stroke-width='.5' stroke-linecap='{cap}' d='M0 0L0 0'/>"),4.).unwrap();
            near(s.bounds_px, [-1., -1., 1., 1.], 0.07);
        }
        let s=load("<path fill='none' stroke='red' stroke-width='.5' d='M0 0L0 0'/><path fill='none' stroke='red' stroke-width='.5' d='M-1 0L1 0'/>",4.).unwrap();
        near(s.bounds_px, [-4., -1., 4., 1.], 1e-5);
    }
    #[test]
    fn reflected_sheared_stroke_is_outlined_in_local_units_before_transform() {
        let s=load("<g transform='matrix(-1 .5 .25 2 0 0)'><path fill='none' stroke='red' stroke-width='.5' d='M-1 0L1 0'/></g>",4.).unwrap();
        near(s.bounds_px, [-4.25, -4., 4.25, 4.], 1e-5);
    }
    #[test]
    fn actual_dash_gaps_and_offset_change_complete_containment() {
        let outer = [
            [-0.1, -0.2],
            [3.1, -0.2],
            [3.1, 0.2],
            [-0.1, 0.2],
            [-0.1, -0.2],
        ];
        let holes = vec![vec![
            [1.1, -0.15],
            [1.9, -0.15],
            [1.9, 0.15],
            [1.1, 0.15],
            [1.1, -0.15],
        ]];
        let svg = |dash: &str| {
            format!("<svg xmlns='http://www.w3.org/2000/svg' width='4mm' height='2mm' viewBox='0 -1 4 2'><path fill='none' stroke='red' stroke-width='.2' {dash} d='M0 0L3 0'/></svg>")
        };
        let prepare = |dash: &str| {
            let tree = usvg::Tree::from_str(&svg(dash), &usvg::Options::default()).unwrap();
            let pivot = [0., f64::from(tree.size().height()) / 2.];
            svg_painted_support(
                &tree,
                pivot,
                1. / f64::from(96f32 / 25.4),
                SvgSupportLimits::default(),
            )
            .unwrap()
            .unwrap()
        };
        assert!(contains(&prepare("stroke-dasharray='1 1'"), &outer, &holes));
        assert!(!contains(&prepare(""), &outer, &holes));
        assert!(!contains(
            &prepare("stroke-dasharray='1 1' stroke-dashoffset='.5'"),
            &outer,
            &holes
        ));
    }
    #[test]
    fn fully_unpainted_support_is_none() {
        assert!(load(
            "<g opacity='0'><rect width='1' height='1'/></g><path fill='none' d='M0 0L1 1'/>",
            4.
        )
        .is_none());
        assert!(load("<rect width='1' height='1' fill-opacity='0'/>", 4.).is_none());
    }
    #[test]
    fn unsupported_component_and_quality_limits_fail_whole_resource() {
        for body in ["<rect width='1' height='1'/><defs><linearGradient id='g'><stop stop-color='red'/><stop offset='1' stop-color='blue'/></linearGradient></defs><rect width='1' height='1' fill='url(#g)'/>",
            "<defs><clipPath id='c'><rect width='1' height='1'/></clipPath></defs><g clip-path='url(#c)'><rect width='2' height='2'/></g>"] {
            let tree=parsed(body);
            assert!(svg_painted_support(&tree,[0.,0.],1.,SvgSupportLimits::default()).is_err());
        }
        let tree = parsed("<rect width='1' height='1'/>");
        let l = SvgSupportLimits::default();
        for bad in [
            SvgSupportLimits { max_nodes: 0, ..l },
            SvgSupportLimits {
                max_segments: 1,
                ..l
            },
            SvgSupportLimits {
                max_vertices: 1,
                ..l
            },
            SvgSupportLimits {
                max_triangles: 1,
                ..l
            },
            SvgSupportLimits {
                configured_curve_tolerance_px: 0.,
                ..l
            },
        ] {
            assert!(svg_painted_support(&tree, [0., 0.], 1., bad).is_err());
        }
        assert!(svg_painted_support(&tree, [0., 0.], f64::NAN, l).is_err());
        let huge =
            parsed("<g transform='translate(10000000,0)'><rect x='.25' width='4' height='1'/></g>");
        assert!(svg_painted_support(&huge, [0., 0.], 1., l).is_err());
    }
    #[test]
    fn zero_intervals_and_many_contours_are_admitted_before_dash_allocation() {
        let long_array = std::iter::repeat_n("0", 258)
            .chain(["1", "1"])
            .collect::<Vec<_>>()
            .join(" ");
        let tree = parsed(&format!(
            "<path fill='none' stroke='red' stroke-dasharray='{long_array}' d='M0 0L1 0'/>"
        ));
        assert!(
            svg_painted_support(&tree, [0., 0.], 1., SvgSupportLimits::default())
                .unwrap_err()
                .contains("dash entry budget")
        );
        let array = std::iter::repeat_n("0 1", 64).collect::<Vec<_>>().join(" ");
        let path = (0..100)
            .map(|i| format!("M{} 0l.0001 0", f64::from(i) * 0.001))
            .collect::<Vec<_>>()
            .join(" ");
        let tree = parsed(&format!(
            "<path fill='none' stroke='red' stroke-dasharray='{array}' d='{path}'/>"
        ));
        assert!(
            svg_painted_support(&tree, [0., 0.], 1., SvgSupportLimits::default())
                .unwrap_err()
                .contains("dash expansion budget")
        );
    }
    #[test]
    fn dash_expansion_is_checked_before_intermediate_growth() {
        let tree = parsed(
            "<path fill='none' stroke='red' stroke-dasharray='.0000001 0.0000001' d='M0 0L1 0'/>",
        );
        assert!(
            svg_painted_support(&tree, [0., 0.], 1., SvgSupportLimits::default())
                .unwrap_err()
                .contains("dash expansion budget")
        );
    }
}
