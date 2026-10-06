//! Display geometry in portrayal/local millimetres, independent of product adapters.
use crate::{Scaler, ScreenPoint, WorldPoint};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PortrayalPath {
    Group(Vec<PortrayalPath>),
    Polyline(Vec<(f64, f64)>),
    /// WGS84 centre in chart X-longitude/Y-latitude order; radius is metres.
    /// The actual viewport resolves this retained arc, rather than freezing
    /// its pixel quality at catalogue/cell compilation time.
    GeographicArc {
        center: (f64, f64),
        radius_m: f64,
        start: f64,
        sweep: f64,
    },
    Arc {
        center: (f64, f64),
        radius: f64,
        start: f64,
        sweep: f64,
        geographic_angle: bool,
    },
    Arc3 {
        start: (f64, f64),
        median: (f64, f64),
        end: (f64, f64),
    },
    Annulus {
        center: (f64, f64),
        outer: f64,
        inner: f64,
        start: f64,
        sweep: f64,
        geographic_angle: bool,
    },
}
impl PortrayalPath {
    /// Resolve connected segments as one run, keeping disjoint boundaries separate.
    pub fn world_paths(&self, origin: WorldPoint, scaler: &Scaler) -> Vec<Vec<WorldPoint>> {
        let mut stack = vec![(self, 0usize)];
        let mut runs: Vec<Vec<WorldPoint>> = Vec::new();
        let mut previous_closed = false;
        while let Some((path, depth)) = stack.pop() {
            if depth > 32 {
                return Vec::new();
            }
            if let Self::Group(children) = path {
                stack.extend(children.iter().rev().map(|p| (p, depth + 1)));
                continue;
            }
            let points = path.world_points(origin, scaler);
            if points.len() < 2 {
                continue;
            }
            if let Some(previous) = runs.last_mut() {
                // Part 9: a closed path ends the run even when the next
                // segment starts at the same position.
                if previous_closed || previous.first() == previous.last() {
                    runs.push(points);
                    previous_closed = matches!(path, Self::GeographicArc { sweep, .. } if sweep.rem_euclid(360.) == 0.);
                    continue;
                }
                let a = scaler.world_to_screen(*previous.last().unwrap());
                let b = scaler.world_to_screen(points[0]);
                if (a.x - b.x).hypot(a.y - b.y) <= 0.001 {
                    previous.extend(points.into_iter().skip(1));
                    previous_closed = matches!(path, Self::GeographicArc { sweep, .. } if sweep.rem_euclid(360.) == 0.);
                    continue;
                }
            }
            runs.push(points);
            previous_closed = matches!(path, Self::GeographicArc { sweep, .. } if sweep.rem_euclid(360.) == 0.);
        }
        runs
    }
    /// A single connected run; disjoint groups must be consumed through world_paths.
    pub fn world_points(&self, origin: WorldPoint, scaler: &Scaler) -> Vec<WorldPoint> {
        let anchor = scaler.world_to_screen(origin);
        let ppm = scaler.pixels_per_mm();
        let local = |p: (f64, f64)| {
            scaler.screen_to_world(ScreenPoint::new(
                anchor.x + (p.0 * ppm) as f32,
                anchor.y - (p.1 * ppm) as f32,
            ))
        };
        let arc = |center: (f64, f64),
                   radius: f64,
                   start: f64,
                   sweep: f64,
                   geographic: bool|
         -> Vec<(f64, f64)> {
            if ![center.0, center.1, radius, start, sweep]
                .iter()
                .all(|v| v.is_finite())
                || radius < 0.
                || sweep.abs() > 360.
            {
                return vec![];
            }
            // Bound sagitta to a quarter physical pixel; finite cap guards hostile input.
            let r_px = radius * ppm;
            let step = if r_px > 0.25 {
                2. * (1. - 0.25 / r_px).clamp(-1., 1.).acos()
            } else {
                std::f64::consts::FRAC_PI_2
            };
            let n = ((sweep.to_radians().abs() / step).ceil() as usize).clamp(1, 4096);
            let mut points: Vec<_> = (0..=n)
                .map(|i| {
                    let a = (start + sweep * i as f64 / n as f64).to_radians();
                    let (mut x, mut y) = (a.sin(), a.cos());
                    if geographic && scaler.projection() == crate::FlatProjection::LocalGeographic {
                        x = x / origin.y.to_radians().cos().abs().max(1e-6) * scaler.scale_x();
                        y *= scaler.scale_y();
                        let d = x.hypot(y);
                        x /= d;
                        y /= d;
                    }
                    (center.0 + radius * x, center.1 + radius * y)
                })
                .collect();
            if sweep.abs() == 360. {
                points[n] = points[0];
            }
            points
        };
        let pts = match self {
            Self::GeographicArc { center, radius_m, start, sweep } => {
                return geographic_arc_world_points(*center, *radius_m, *start, *sweep, scaler);
            }
            Self::Group(_) => {
                let mut p = self.world_paths(origin, scaler);
                return if p.len() == 1 {
                    p.pop().unwrap()
                } else {
                    Vec::new()
                };
            }
            Self::Polyline(p) => p.clone(),
            Self::Arc {
                center,
                radius,
                start,
                sweep,
                geographic_angle,
            } => arc(*center, *radius, *start, *sweep, *geographic_angle),
            Self::Annulus {
                center,
                outer,
                inner,
                start,
                sweep,
                geographic_angle,
            } => {
                if *inner < 0. || inner > outer || !inner.is_finite() {
                    return vec![];
                }
                let mut p = arc(*center, *outer, *start, *sweep, *geographic_angle);
                // This describes a closed sector/annular sector. A full ring's boundaries
                // are emitted as separate paths by the product adapter.
                let q = if *inner == 0. {
                    vec![*center]
                } else {
                    arc(*center, *inner, *start + *sweep, -*sweep, *geographic_angle)
                };
                p.extend(q);
                if let Some(first) = p.first().copied() {
                    p.push(first);
                }
                p
            }
            Self::Arc3 { start, median, end } => {
                let (a, b, c) = (*start, *median, *end);
                let (bx, by, cx, cy) = (b.0 - a.0, b.1 - a.1, c.0 - a.0, c.1 - a.1);
                let d = 2. * (bx * cy - by * cx);
                if ![a.0, a.1, b.0, b.1, c.0, c.1].iter().all(|v| v.is_finite()) {
                    return vec![];
                }
                if d.abs() <= f64::EPSILON * (bx.hypot(by) * cx.hypot(cy)).max(1.) {
                    // Part 9 Arc3Points requires non-collinear points. A straight
                    // polyline would invent a different geometry.
                    return Vec::new();
                } else {
                    let b2 = bx * bx + by * by;
                    let c2 = cx * cx + cy * cy;
                    let center = (a.0 + (cy * b2 - by * c2) / d, a.1 + (bx * c2 - cx * b2) / d);
                    let angle = |p: (f64, f64)| {
                        (p.0 - center.0)
                            .atan2(p.1 - center.1)
                            .to_degrees()
                            .rem_euclid(360.)
                    };
                    let s = angle(a);
                    let m = (angle(b) - s).rem_euclid(360.);
                    let e = (angle(c) - s).rem_euclid(360.);
                    let sweep = if m <= e { e } else { e - 360. };
                    let radius = (a.0 - center.0).hypot(a.1 - center.1);
                    let first_sweep = if sweep >= 0. { m } else { m - 360. };
                    let mut first = arc(center, radius, s, first_sweep, false);
                    let mut second = arc(center, radius, s + first_sweep, sweep - first_sweep, false);
                    if first.is_empty() || second.is_empty() {
                        return Vec::new();
                    }
                    // The supplied positions define this arc. Preserve them
                    // rather than replacing them with rounded circle evaluations.
                    first[0] = a;
                    *first.last_mut().unwrap() = b;
                    second[0] = b;
                    *second.last_mut().unwrap() = c;
                    first.extend(second.into_iter().skip(1));
                    first
                }
            }
        };
        if pts.iter().any(|p| !p.0.is_finite() || !p.1.is_finite()) {
            return vec![];
        }
        pts.into_iter().map(local).collect()
    }
}

/// Refine WGS84 circle samples against the active flat projection. This uses
/// quarter/midpoint chord checks and a maximum 5-degree azimuth interval;
/// it is an explicit numerical tessellation policy, not a universal analytic
/// error theorem. Refuse budget/precision failures instead of silently capping
/// tessellation and claiming the requested quality was reached.
fn geographic_arc_world_points(center: (f64,f64), radius_m: f64, start: f64, sweep: f64, scaler: &Scaler) -> Vec<WorldPoint> {
    use ferrite_kernel::geodesy::{GeodesicRadiusArc,GeographicPosition};
    let resolve = || -> Result<Vec<WorldPoint>,String> {
        let center = GeographicPosition::new(center.1,center.0).map_err(|e| e.to_string())?;
        let arc = GeodesicRadiusArc::new(center,radius_m,start,sweep).map_err(|e|e.to_string())?;
        const MAX_VERTICES:usize=4097;
        let seed=arc.sample_drawing_copies(5.,MAX_VERTICES,center.longitude()).map_err(|e|e.to_string())?;
        let at=|t:f64,reference:f64| -> Result<WorldPoint,String> {
            let p=arc.position_at(t).map_err(|e|e.to_string())?;
            Ok(WorldPoint::new(p.longitude_near(reference).map_err(|e|e.to_string())?,p.latitude()))
        };
        let project=|p:WorldPoint| -> Result<[f64;2],String> {
            let q=scaler.world_to_screen_f64(p);
            if q[0].is_finite() && q[1].is_finite() { Ok(q) } else { Err("Non-finite geographic arc projection".into()) }
        };
        let mut output=Vec::new();
        output.push(WorldPoint::new(seed[0].longitude(),seed[0].latitude()));
        let segments=seed.len()-1;
        for i in 0..segments {
            let a=WorldPoint::new(seed[i].longitude(),seed[i].latitude());
            let b=WorldPoint::new(seed[i+1].longitude(),seed[i+1].latitude());
            let mut stack=vec![(i as f64/segments as f64,(i+1) as f64/segments as f64,a,b,0usize)];
            while let Some((ta,tb,a,b,depth))=stack.pop() {
                let pa=project(a)?;let pb=project(b)?;
                let mut error=0f64;
                let mut middle=a;
                for f in [0.25,0.5,0.75] {
                    let p=at(ta+(tb-ta)*f,a.x+(b.x-a.x)*f)?;
                    let q=project(p)?;
                    let x=pa[0]+(pb[0]-pa[0])*f;
                    let y=pa[1]+(pb[1]-pa[1])*f;
                    error=error.max((q[0]-x).hypot(q[1]-y));
                    if f==0.5 { middle=p; }
                }
                if error<=0.125 {
                    if output.len()>=MAX_VERTICES { return Err("Geographic arc vertex budget exceeded".into()); }
                    output.push(b);
                } else {
                    if depth>=20 || output.len()+stack.len()+2>MAX_VERTICES { return Err("Geographic arc refinement budget exceeded".into()); }
                    let tm=(ta+tb)*0.5;
                    stack.push((tm,tb,middle,b,depth+1));
                    stack.push((ta,tm,a,middle,depth+1));
                }
            }
        }
        Ok(output)
    };
    match resolve() { Ok(points)=>points,Err(error)=>{tracing::error!("Geographic portrayal arc: {error}");Vec::new()} }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GeoBounds, Viewport};
    #[test]
    fn geographic_metric_arc_refines_for_the_active_view_and_dense_reference() {
        use ferrite_kernel::geodesy::{GeographicPosition,GeodesicRadiusArc,inverse};
        let center=GeographicPosition::new(50.,179.9).unwrap();
        for projection in [crate::FlatProjection::LocalGeographic,crate::FlatProjection::EllipsoidalMercator] {
            let mut counts=Vec::new();
            for zoom in [1.,200.] {
                let mut scaler=Scaler::new(GeoBounds::new(179.9-1./zoom,50.-1./zoom,179.9+1./zoom,50.+1./zoom),Viewport::new(800.,600.));
                scaler.set_projection(projection);
                for sweep in [270.,-270.] {
                    let path=PortrayalPath::GeographicArc{center:(179.9,50.),radius_m:50_000.,start:0.,sweep};
                    let points=path.world_points(WorldPoint::new(0.,0.),&scaler);
                    assert!((2..=4097).contains(&points.len()));
                    let mut fractions=Vec::new();
                    for (i,p) in points.iter().enumerate() {
                        let source=GeographicPosition::new(p.y,(p.x+180.).rem_euclid(360.)-180.).unwrap();
                        let m=inverse(center,source).unwrap();
                        assert!((m.distance_m-50_000.).abs()<1e-6);
                        let f=if i==0 {0.} else if i+1==points.len() {1.} else if sweep>0. {m.initial_azimuth_deg.rem_euclid(360.)/sweep} else {(-m.initial_azimuth_deg).rem_euclid(360.)/(-sweep)};
                        fractions.push(f);
                    }
                    assert!(fractions.windows(2).all(|f| f[0]<f[1]));
                    let arc=GeodesicRadiusArc::new(center,50_000.,0.,sweep).unwrap();
                    for i in 1..5000 {
                        let t=i as f64/5000.;
                        let segment=fractions.partition_point(|f| *f<t).saturating_sub(1).min(points.len()-2);
                        let a=scaler.world_to_screen_f64(points[segment]);let b=scaler.world_to_screen_f64(points[segment+1]);
                        let p=arc.position_at(t).unwrap();
                        let lon=p.longitude_near((points[segment].x+points[segment+1].x)*0.5).unwrap();
                        let q=scaler.world_to_screen_f64(WorldPoint::new(lon,p.latitude()));
                        let d=[b[0]-a[0],b[1]-a[1]];let length=d[0]*d[0]+d[1]*d[1];
                        let f=if length>0. {(((q[0]-a[0])*d[0]+(q[1]-a[1])*d[1])/length).clamp(0.,1.)} else {0.};
                        let error=(q[0]-a[0]-f*d[0]).hypot(q[1]-a[1]-f*d[1]);
                        assert!(error<0.25,"{projection:?} zoom {zoom} sweep {sweep} error {error}");
                    }
                    if sweep>0. {counts.push(points.len());}
                }
            }
            assert!(counts[1]>counts[0],"zoom must retessellate the retained geographic arc");
        }
    }
    #[test]
    fn three_point_arc_preserves_defining_positions_in_both_directions() {
        for density in [1., 2.] {
            for zoom in [0.5, 1., 200.] {
                let mut scaler = Scaler::new(GeoBounds::new(-1./zoom,-1./zoom,1./zoom,1./zoom),Viewport::new(800.,600.));
                scaler.set_pixel_ratio(density);
                let origin=WorldPoint::new(0.,0.);
                let anchor=scaler.world_to_screen(origin);
                let local=|p:(f64,f64)| scaler.screen_to_world(ScreenPoint::new(anchor.x+(p.0*scaler.pixels_per_mm()) as f32,anchor.y-(p.1*scaler.pixels_per_mm()) as f32));
                for (a,b,c) in [((2.13,17.29),(-11.81,3.07),(14.23,-9.61)),((14.23,-9.61),(-11.81,3.07),(2.13,17.29))] {
                    let points=PortrayalPath::Arc3{start:a,median:b,end:c}.world_points(origin,&scaler);
                    assert_eq!(points.first().copied(),Some(local(a)));
                    assert_eq!(points.last().copied(),Some(local(c)));
                    assert_eq!(points.iter().filter(|&&p| p==local(b)).count(),1);
                    assert!(points.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
                }
            }
        }
    }
    #[test]
    fn physical_arc_and_three_point_circle() {
        for density in [1., 2.] {
            for zoom in [0.5, 1., 2.] {
                let mut s = Scaler::new(
                    GeoBounds::new(-1. / zoom, -1. / zoom, 1. / zoom, 1. / zoom),
                    Viewport::new(800., 600.),
                );
                s.set_pixel_ratio(density);
                let origin = WorldPoint::new(0., 0.);
                let center = s.world_to_screen(origin);
                for path in [
                    PortrayalPath::Arc {
                        center: (0., 0.),
                        radius: 25.,
                        start: 0.,
                        sweep: 180.,
                        geographic_angle: false,
                    },
                    PortrayalPath::Arc3 {
                        start: (0., 25.),
                        median: (25., 0.),
                        end: (0., -25.),
                    },
                ] {
                    let pts = path.world_points(origin, &s);
                    assert!(pts.len() > 20);
                    for p in pts {
                        let p = s.world_to_screen(p);
                        assert!(
                            (((p.x - center.x) as f64).hypot((p.y - center.y) as f64)
                                - 25. * s.pixels_per_mm())
                            .abs()
                                < 0.001
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn arc_three_points_chooses_major_sweep_through_median() {
        let s = Scaler::new(GeoBounds::new(-1., -1., 1., 1.), Viewport::new(800., 600.));
        let p = PortrayalPath::Arc3 {
            start: (0., 10.),
            median: (-10., 0.),
            end: (10., 0.),
        };
        let points = p.world_points(WorldPoint::new(0., 0.), &s);
        let center = s.world_to_screen(WorldPoint::new(0., 0.));
        assert!(points
            .iter()
            .any(|p| s.world_to_screen(*p).x < center.x - 9. * s.pixels_per_mm() as f32));
    }
    #[test]
    fn picks_the_arc_instead_of_its_source_point() {
        let s = Scaler::new(GeoBounds::new(-1., -1., 1., 1.), Viewport::new(800., 600.));
        let origin = WorldPoint::new(0., 0.);
        let center = s.world_to_screen(origin);
        let mut line = crate::LineInstruction::new(vec![origin, origin]);
        line.portrayal_path = Some(PortrayalPath::Arc {
            center: (0., 0.),
            radius: 25.,
            start: 0.,
            sweep: 180.,
            geographic_angle: false,
        });
        let instruction = crate::DrawingInstruction::Line(line);
        assert!(crate::hit_geometry(
            &instruction,
            &s,
            ScreenPoint::new(center.x + 25. * s.pixels_per_mm() as f32, center.y),
            1.
        )
        .is_some());
        assert!(crate::hit_geometry(&instruction, &s, center, 1.).is_none());
    }
}
#[cfg(test)]
mod grouped_path_tests {
    use super::*;
    use crate::{
        hit_geometry, Color, DrawingInstruction, GeoBounds, LineInstruction, LineStyle, Viewport,
    };
    #[test]
    fn connected_segments_keep_dash_phase_and_disjoint_runs_do_not_bridge() {
        for density in [1., 2.] {
            for zoom in [0.5, 1., 2.] {
                let mut scaler = Scaler::new(
                    GeoBounds::new(-1. / zoom, -1. / zoom, 1. / zoom, 1. / zoom),
                    Viewport::new(800., 600.),
                );
                scaler.set_pixel_ratio(density);
                let origin = WorldPoint::new(0., 0.);
                let p = PortrayalPath::Group(vec![
                    PortrayalPath::Polyline(vec![(0., 0.), (4., 0.)]),
                    PortrayalPath::Polyline(vec![(4., 0.), (10., 0.)]),
                    PortrayalPath::Polyline(vec![(20., 0.), (24., 0.)]),
                    PortrayalPath::Polyline(vec![(24., 0.), (30., 0.)]),
                ]);
                let runs = p.world_paths(origin, &scaler);
                assert_eq!(runs.len(), 2);
                assert_eq!(runs.iter().map(Vec::len).collect::<Vec<_>>(), vec![3, 3]);
                let mut line = LineInstruction::new(vec![origin, origin]);
                line.portrayal_path = Some(p);
                line.style = LineStyle::solid_mm(Color::BLACK, 0.32);
                line.style.dash_pattern = vec![3.6, 1.8];
                let instruction = DrawingInstruction::Line(line);
                let a = scaler.world_to_screen(origin);
                for (x, hit) in [
                    (2., true),
                    (4.2, false),
                    (6., true),
                    (15., false),
                    (22., true),
                    (24.2, false),
                    (26., true),
                ] {
                    assert_eq!(
                        hit_geometry(
                            &instruction,
                            &scaler,
                            ScreenPoint::new(a.x + (x * scaler.pixels_per_mm()) as f32, a.y),
                            0.01
                        )
                        .is_some(),
                        hit,
                        "at {x}mm density{density} zoom{zoom}"
                    );
                }
            }
        }
    }
    #[test]
    fn full_annulus_boundaries_have_no_radial_connector() {
        let scaler = Scaler::new(GeoBounds::new(-1., -1., 1., 1.), Viewport::new(800., 600.));
        let origin = WorldPoint::new(0., 0.);
        let p = PortrayalPath::Group(
            [25., 15.]
                .into_iter()
                .map(|radius| PortrayalPath::Arc {
                    center: (0., 0.),
                    radius,
                    start: 0.,
                    sweep: 360.,
                    geographic_angle: false,
                })
                .collect(),
        );
        let runs = p.world_paths(origin, &scaler);
        assert_eq!(runs.len(), 2);
        let mut line = LineInstruction::new(vec![origin, origin]);
        line.portrayal_path = Some(p);
        let instruction = DrawingInstruction::Line(line);
        let a = scaler.world_to_screen(origin);
        for (radius, hit) in [(15., true), (20., false), (25., true)] {
            assert_eq!(
                hit_geometry(
                    &instruction,
                    &scaler,
                    ScreenPoint::new(a.x, a.y - (radius * scaler.pixels_per_mm()) as f32),
                    0.01
                )
                .is_some(),
                hit
            );
        }
    }
}

#[cfg(test)]
mod arc3_degenerate_contract {
    use super::*;
    #[test]
    fn invalid_three_point_arcs_never_fabricate_straight_segments() {
        let scaler=Scaler::new(crate::GeoBounds::new(-1.,-1.,1.,1.),crate::Viewport::new(800.,600.));
        for (start,median,end) in [((0.,0.),(1.,1.),(2.,2.)), ((0.,0.),(0.,0.),(1.,2.)), ((0.,0.),(1.,2.),(0.,0.)), ((0.,0.),(f64::NAN,1.),(2.,1.))] {
            let path=PortrayalPath::Arc3 {start,median,end};
            assert!(path.world_points(WorldPoint::new(0.,0.),&scaler).is_empty());
            assert!(path.world_paths(WorldPoint::new(0.,0.),&scaler).is_empty());
        }
        let valid=PortrayalPath::Arc3 {start:(0.,10.),median:(10.,0.),end:(0.,-10.)};
        assert!(valid.world_points(WorldPoint::new(0.,0.),&scaler).len()>2);
    }
}

#[cfg(test)]
mod closed_path_contract {
    use super::*;
    #[test]
    fn a_closed_run_cannot_absorb_the_next_segment() {
        let scaler=Scaler::new(crate::GeoBounds::new(-1.,-1.,1.,1.),crate::Viewport::new(800.,600.));
        let origin=WorldPoint::new(0.,0.);
        let closed=PortrayalPath::Group(vec![
            PortrayalPath::Arc {center:(0.,0.),radius:10.,start:0.,sweep:360.,geographic_angle:false},
            PortrayalPath::Polyline(vec![(0.,10.),(0.,20.)]),
        ]);
        let runs=closed.world_paths(origin,&scaler);
        assert_eq!(runs.len(),2);
        assert_eq!(runs[0].first(),runs[0].last());
        assert_eq!(runs[0].last(),runs[1].first());
        let open=PortrayalPath::Group(vec![
            PortrayalPath::Arc {center:(0.,0.),radius:10.,start:0.,sweep:180.,geographic_angle:false},
            PortrayalPath::Polyline(vec![(0.,-10.),(0.,-20.)]),
        ]);
        assert_eq!(open.world_paths(origin,&scaler).len(),1);
        let degenerate_closed=PortrayalPath::Group(vec![
            PortrayalPath::Polyline(vec![(0.,0.),(0.,0.)]),
            PortrayalPath::Polyline(vec![(0.,0.),(0.,20.)]),
        ]);
        assert_eq!(degenerate_closed.world_paths(origin,&scaler).len(),2);
    }
}
