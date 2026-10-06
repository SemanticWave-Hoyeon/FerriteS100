//! Perspective chart-pane target shared by interactive rendering and exports.
//! This first integration intentionally diagnoses unsupported portrayal commands.
use crate::{
    globe_scene::{GlobeDepthMode, GlobeDraw, GlobeLayer, GlobeMesh, GlobeSceneRenderer},
    GpuState, RenderPipelines, TextureVertex,
};
use ferrite_kernel::{
    geodesy::{inverse, GeographicPosition},
    globe_camera::GlobeCamera,
    map_camera::MapCamera,
};
use ferrite_render::{
    AreaFillType, DrawingInstruction, RenderContext, Scaler, ScreenPoint, WorldPoint,
};
use std::collections::{BTreeMap, HashSet};

fn geometry_pool() -> &'static rayon::ThreadPool {
    static GEOMETRY_POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    GEOMETRY_POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(std::thread::available_parallelism().map_or(1, |n| n.get().min(4)))
            .thread_name(|i| format!("globe-geometry-{i}"))
            .build()
            .expect("globe geometry worker pool")
    })
}
#[derive(Debug, Default)]
pub struct GlobePreviewDiagnostics {
    pub areas: usize,
    pub lines: usize,
    pub symbols: usize,
    pub texts: usize,
    pub text_footprints_px: Vec<[f32; 4]>,
    pub symbol_footprints_px: Vec<[f32; 4]>,
    pub missing_symbol_resources: usize,
    pub unsupported_commands: usize,
    pub rejected_geometries: usize,
    pub temporal_hidden: usize,
    pub temporal_diagnostics: usize,
    pub ungrounded_commands: usize,
    pub reasons: BTreeMap<String, usize>,
    pub viewport: [f64; 4],
    pub focus: [f64; 2],
    pub range_m: f64,
    pub tilt_deg: f64,
    pub heading_deg: f64,
    pub scale_denominator: f64,
    pub principal_scale_denominators: [f64; 2],
    pub vertices: usize,
    pub triangles: usize,
    pub resources: serde_json::Value,
    pub preparation: serde_json::Value,
}
impl GlobePreviewDiagnostics {
    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({"main_app_globe_connected":true,"complete_globe_portrayal":false,
            "projection":"WGS84 perspective","areas":self.areas,"lines":self.lines,"symbols":self.symbols,"texts":self.texts,"text_footprints_px":self.text_footprints_px,"symbol_footprints_px":self.symbol_footprints_px,"missing_symbol_resources":self.missing_symbol_resources,
            "unsupported_commands":self.unsupported_commands,"rejected_geometries":self.rejected_geometries,
            "temporal_hidden":self.temporal_hidden,"temporal_diagnostics":self.temporal_diagnostics,
            "ungrounded_commands":self.ungrounded_commands,"reasons":self.reasons,
            "viewport":self.viewport,"focus_lon_lat":self.focus,"range_m":self.range_m,
            "scale_denominator":self.scale_denominator,"principal_scale_denominators":self.principal_scale_denominators,"scale_policy":"coarsest principal ground scale at camera focus","heading_deg":self.heading_deg,"tilt_deg":self.tilt_deg,"vertices":self.vertices,"triangles":self.triangles,
            "omissions":["line text placement","font reference selection","symbol/pixmap patterns","hatch repeating symbols","bathymetry","reference/local lines","3D DataCoverage"],
            "preparation":self.preparation,"resources":self.resources,"navigation":"WGS84 perspective surface-anchor navigation"})
    }
}
// Owned independently of the valid pane: failures invalidate image/picking while
// reusable allocations survive. One target per renderer; switching to flat drops it.
pub(crate) struct GlobeResources {
    diagnostic_retained_passing:bool,
    scene: GlobeSceneRenderer,
    earth: std::sync::Arc<GlobeMesh>,
    target: Option<([u32; 2], wgpu::Texture, wgpu::TextureView, wgpu::BindGroup)>,
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    quad: Option<[f32; 4]>,
    color_allocations: u64,
    quad_writes: u64,
    frames: u64,
    area_cache: std::collections::HashMap<usize, crate::globe_portrayal::CachedArea>,
    area_cache_revision: u64,
    area_source_cache: crate::globe_portrayal::AreaSourceCache,
    area_source_cache_enabled: bool,
    area_midpoint_reuse_enabled: bool,
    line_route_reuse_enabled: bool,
    line_projection_reuse_enabled: bool,
    area_chord_precheck_enabled: bool,
    area_spatial: ferrite_kernel::spatial_hierarchy::SpatialHierarchy<3>,
    area_spatial_dirty: bool,
    curve_cache: std::collections::HashMap<usize, crate::globe_curve_clip::PreparedCurve>,
    curve_cache_bytes: usize,
    area_cache_bytes: usize,
    prepared_suppression: Option<crate::globe_lines::PreparedSuppression>,
    suppression_cache: Vec<(
        f64,
        Vec<bool>,
        std::sync::Arc<ferrite_render::LineSuppressionPlan>,
        usize,
    )>,
}
impl GlobeResources {
    pub(crate) fn configure_retained_passing(&mut self,enabled:bool) {
        self.diagnostic_retained_passing=enabled;
        // Each diagnostic phase starts with cold adaptive caches, same sourceepoch.
        self.area_cache.clear();self.area_cache_bytes=0;self.area_spatial_dirty=true;
    }

    pub(crate) fn configure_source_cache(&mut self, mode:u8) {
        self.area_source_cache_enabled=mode!=0;
        self.area_source_cache.set_topology_only(mode==2);
    }

    fn new(gpu: &GpuState, pipelines: &RenderPipelines, sample_count: u32) -> Result<Self, String> {
        use wgpu::util::DeviceExt;
        let earth = std::sync::Arc::new(GlobeMesh::ellipsoid(360, 180, [0.10, 0.22, 0.30, 1.])?);
        let mut scene = GlobeSceneRenderer::new_with_texture_samples(
            &gpu.device,
            gpu.format(),
            Some(&pipelines.texture_bind_group_layout),
            sample_count,
        );
        scene.bind_retained_base(earth.clone())?;
        Ok(Self {
            diagnostic_retained_passing:std::env::var("FERRITE_GLOBE_RETAINED_PASSING_PAIRS").as_deref()==Ok("1"),
            earth,
            scene,
            target: None,
            vertices: gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("globe composite vertices"),
                size: (4 * std::mem::size_of::<TextureVertex>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            indices: gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("globe composite indices"),
                    contents: bytemuck::cast_slice(&[0u32, 1, 2, 0, 2, 3]),
                    usage: wgpu::BufferUsages::INDEX,
                }),
            quad: None,
            color_allocations: 0,
            quad_writes: 0,
            frames: 0,
            area_cache: Default::default(),
            area_cache_revision: 0,
            area_source_cache: Default::default(),
            area_chord_precheck_enabled: std::env::var("FERRITE_GLOBE_AREA_CHORD_PRECHECK")
                .is_ok_and(|v| v == "1"),
            area_midpoint_reuse_enabled: std::env::var("FERRITE_GLOBE_AREA_MIDPOINT_REUSE")
                .is_ok_and(|v| v == "1"),
            area_source_cache_enabled: std::env::var("FERRITE_GLOBE_AREA_SOURCE_CACHE")
                .ok()
                .as_deref()
                == Some("1"),
            line_route_reuse_enabled: std::env::var("FERRITE_GLOBE_LINE_ROUTE_REUSE")
                .is_ok_and(|v| v == "1"),
            line_projection_reuse_enabled: std::env::var("FERRITE_GLOBE_LINE_PROJECTION_REUSE")
                .is_ok_and(|v| v == "1"),
            area_spatial: Default::default(),
            area_spatial_dirty: true,
            curve_cache: Default::default(),
            curve_cache_bytes: 0,
            area_cache_bytes: 0,
            prepared_suppression: None,
            suppression_cache: Vec::new(),
        })
    }
}
pub(crate) struct GlobePane {
    _texture: wgpu::Texture,
    pub bind_group: wgpu::BindGroup,
    pub vertices: wgpu::Buffer,
    pub indices: wgpu::Buffer,
    camera: MapCamera,
    source: ferrite_render::FlatTransform,
    source_pose: Option<ferrite_kernel::globe_navigation::GlobePose>,
    source_pixels_per_mm: f64,
    source_range_factor: f64,
    pub source_ordinals: Vec<usize>,
    draw_source_ordinals: Vec<Option<usize>>,
    pub source_geometry_revision: u64,
    pub diagnostics: GlobePreviewDiagnostics,
}
impl GlobePane {
    pub fn matches(
        &self,
        scaler: &Scaler,
        tilt: f64,
        pose: Option<ferrite_kernel::globe_navigation::GlobePose>,
        range_factor: f64,
    ) -> bool {
        self.source == scaler.flat_transform()
            && self.diagnostics.tilt_deg == tilt
            && self.source_pose == pose
            && self.source_pixels_per_mm == scaler.pixels_per_mm()
            && self.source_range_factor == range_factor
    }
    pub fn world_at(&self, screen: ScreenPoint) -> Option<WorldPoint> {
        let v = self.diagnostics.viewport;
        if (screen.x as f64) < v[0]
            || (screen.y as f64) < v[1]
            || (screen.x as f64) >= v[0] + v[2]
            || (screen.y as f64) >= v[1] + v[3]
        {
            return None;
        }
        let p = self
            .camera
            .unproject([screen.x as f64, screen.y as f64])
            .ok()??;
        Some(WorldPoint::new(p[0], p[1]))
    }
    pub fn project_world(&self, world: [f64; 2]) -> Option<[f64; 2]> {
        self.camera.project(world).ok().flatten()
    }
    pub fn pixels_per_mm(&self) -> f64 {
        self.source_pixels_per_mm
    }
    pub fn pick_sources(
        &self,
        resources: &mut GlobeResources,
        gpu: &GpuState,
        screen: [f64; 2],
        radius: f64,
    ) -> Result<Vec<(usize, crate::globe_scene::GlobeDrawHit)>, String> {
        let v = self.diagnostics.viewport;
        let mut result = Vec::new();
        for mut hit in resources.scene.pick_draws(
            &gpu.device,
            &gpu.queue,
            [screen[0] - v[0], screen[1] - v[1]],
            radius,
        )? {
            let Some(source) = hit
                .draw_index
                .checked_sub(1)
                .and_then(|i| self.draw_source_ordinals.get(i))
                .copied().flatten()
            else {
                continue;
            };
            hit.pixel[0] += v[0];
            hit.pixel[1] += v[1];
            result.push((source, hit));
        }
        Ok(result)
    }
    pub fn prepare(
        gpu: &GpuState,
        resources: &mut Option<GlobeResources>,
        pipelines: &RenderPipelines,
        context: &mut RenderContext,
        groups: Option<&HashSet<u32>>,
        tilt: f64,
        range_factor: f64,
        pose: Option<ferrite_kernel::globe_navigation::GlobePose>,
        symbols: &std::collections::HashMap<
            ferrite_render::SymbolId,
            crate::renderer::SymbolTexture,
        >,
        patterns: &crate::globe_pattern::PatternResources,
        rasters: &[crate::globe_raster::GlobeRasterLayer<'_>],
        symbol_scale: f32,
        egui: &mut crate::egui_integration::EguiIntegration,
        sample_count: u32,
        cache_areas: bool,
        cache_curves: bool,
        cache_curve_bounds: bool,
        cache_dyadic_samples: bool,
        reuse_curve_scratch: bool,
        gpu_projection: bool,
        spatial_hierarchy: bool,
        coverage_provider: Option<&dyn ferrite_render::GlobeCoverageProvider>,
    ) -> Result<Self, String> {
        if !matches!(sample_count, 1 | 4) {
            return Err("Globe sampling must be 1 or 4".into());
        }
        let setup_start = std::time::Instant::now();
        let source_pose = pose;
        let scaler = context.scaler.clone();
        let v = scaler.viewport;
        let width = v.width.round() as u32;
        let height = v.height.round() as u32;
        if width == 0
            || height == 0
            || width > gpu.device.limits().max_texture_dimension_2d
            || height > gpu.device.limits().max_texture_dimension_2d
        {
            return Err("Invalid globe chart-pane texture extent".into());
        }
        let p = scaler.screen_to_world(v.center());
        let lon = (p.x + 180.).rem_euclid(360.) - 180.;
        let lat = p.y.clamp(-89.5, 89.5);
        let focus = GeographicPosition::new(lat, lon).map_err(|e| e.to_string())?;
        // Preserve the initial flat view's physical vertical span at the centre.
        let span = inverse(
            GeographicPosition::new(scaler.geo_bounds.min_y.clamp(-89.5, 89.5), lon)
                .map_err(|e| e.to_string())?,
            GeographicPosition::new(scaler.geo_bounds.max_y.clamp(-89.5, 89.5), lon)
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?
        .distance_m;
        let range =
            (span / (2. * 22.5_f64.to_radians().tan()) * range_factor).clamp(10., 50_000_000.);
        let pose = pose.unwrap_or(ferrite_kernel::globe_navigation::GlobePose {
            focus,
            range_m: range,
            heading_deg: 0.,
            tilt_deg: tilt,
        });
        let pose = ferrite_kernel::globe_navigation::GlobePose {
            tilt_deg: tilt,
            ..pose
        };
        let focus = pose.focus;
        let lon = focus.longitude();
        let lat = focus.latitude();
        let range = pose.range_m;
        let camera = pose
            .camera([width as f64, height as f64])
            .map_err(|e| e.to_string())?;
        let metric = camera
            .surface_scale(focus, scaler.pixels_per_mm())
            .map_err(|e| e.to_string())?
            .ok_or("Camera focus has no visible surface metric")?;
        let scale = metric.denominator_max.round().clamp(1., u32::MAX as f64) as u32;
        let mut diagnostics = GlobePreviewDiagnostics {
            viewport: [v.x as f64, v.y as f64, width as f64, height as f64],
            focus: [lon, lat],
            range_m: range,
            tilt_deg: tilt,
            heading_deg: pose.heading_deg,
            scale_denominator: metric.denominator_max,
            principal_scale_denominators: [metric.denominator_min, metric.denominator_max],
            ..Default::default()
        };
        context.get_sorted_instructions();
        let prepared_coverage = ferrite_render::GlobeCoverageView {
            camera: &camera,
            extent: [width, height],
            display_scale: metric.denominator_max,
            pixels_per_mm: scaler.pixels_per_mm(),
        }
        .bind(context, coverage_provider)
        .map_err(|e| e.to_string())?;
        if let Some(path) = std::env::var_os("FERRITE_ROOT_KEY_PROOF_COVERAGE") {
            if !crate::background_test::enabled() { return Err("Coverage proof requires hidden background mode".into()); }
            let path = std::path::PathBuf::from(path);
            std::fs::create_dir_all(&path).map_err(|e|e.to_string())?;
            let mut passes = Vec::new();
            let ids: std::collections::BTreeSet<_> = context.raw_instructions().iter().filter_map(|i|i.cell_index()).collect();
            if let Some(prepared) = &prepared_coverage {
                prepared.validate(context.geometry_revision(),context.coverage_view_revision(),context.instruction_count()).map_err(|e|e.to_string())?;
                for index in 0..prepared.pass_count() {
                    let pass = prepared.pass(index).map_err(|e|e.to_string())?;
                    let decisions = (0..context.instruction_count()).map(|i|pass.decision(i).map(|d|format!("{d:?}"))).collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
                    let mut masks = Vec::new();
                    for id in &ids {
                        if let Some(mask) = pass.frame().mask(*id as usize) {
                            let file = format!("pass-{index}-dataset-{id}.r8");
                            std::fs::write(path.join(&file),mask.pixels()).map_err(|e|e.to_string())?;
                            masks.push(serde_json::json!({"dataset":id,"origin":mask.origin(),"size":mask.size(),"file":file}));
                        } else { masks.push(serde_json::json!({"dataset":id,"absent":true})); }
                    }
                    passes.push(serde_json::json!({"pass":index,"decisions":decisions,"masks":masks}));
                }
            }
            std::fs::write(path.join("coverage.json"),serde_json::to_vec_pretty(&serde_json::json!({"bound":prepared_coverage.is_some(),"source_datasets":ids,"instruction_count":context.instruction_count(),"passes":passes})).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
        }

        let date_start = std::time::Instant::now();
        let (date, hidden, temporal_diagnostics) =
            context.portrayal_visibility().map_err(|e| e.to_string())?;
        let date_visibility_ms = date_start.elapsed().as_secs_f64() * 1000.;
        diagnostics.temporal_hidden = hidden;
        diagnostics.temporal_diagnostics = temporal_diagnostics;
        if resources.is_none() {
            *resources = Some(GlobeResources::new(gpu, pipelines, sample_count)?);
        }
        let resources = resources.as_mut().unwrap();
        if !cache_areas {
            resources.area_cache.clear();
            resources.area_spatial_dirty = true;
            resources.area_cache_bytes = 0;
        }
        if !cache_curves {
            resources.curve_cache.clear();
            resources.curve_cache_bytes = 0;
        }
        if resources.area_cache_revision != context.geometry_revision() {
            resources.curve_cache.clear();
            resources.curve_cache_bytes = 0;
            resources.area_cache.clear();
            resources.area_spatial_dirty = true;
            resources.suppression_cache.clear();
            resources.prepared_suppression = None;
            resources.area_cache_bytes = 0;
            resources.area_cache_revision = context.geometry_revision();
        }
        if spatial_hierarchy && resources.area_spatial_dirty {
            resources.area_spatial = ferrite_kernel::spatial_hierarchy::SpatialHierarchy::build(
                resources
                    .area_cache
                    .iter()
                    .map(|(&id, c)| (id, c.spatial_bounds()))
                    .collect(),
            )
            .map_err(str::to_string)?;
            resources.area_spatial_dirty = false;
        }
        // Only previously validated cached areas may bypass source validation.
        // Uncached/unsupported objects keep the original draping path.
        let mut area_spatial_candidates = vec![true; context.instruction_count()];
        let area_spatial_stats = if spatial_hierarchy {
            for id in resources.area_spatial.ids() {
                area_spatial_candidates[id] = false;
            }
            resources.area_spatial.query_classified(
                |b| {
                    use ferrite_kernel::spatial_hierarchy::SpatialRelation as R;
                    let (c, r) = b.enclosing_sphere();
                    let Ok(d) = camera.frustum_distances(c, 2.) else {
                        return R::Intersecting;
                    };
                    let scale = c
                        .iter()
                        .fold(camera.depth_range_m()[1].max(r), |s, x| s.max(x.abs()));
                    let tolerance = 128. * f64::EPSILON * scale;
                    if d.iter().any(|x| *x < -r - tolerance) {
                        R::Outside
                    } else if d.iter().all(|x| *x > r + tolerance) {
                        R::Inside
                    } else {
                        R::Intersecting
                    }
                },
                |id| area_spatial_candidates[id] = true,
            )
        } else {
            Default::default()
        };
        let instructions = context.raw_instructions();
        let visible: Vec<_> = instructions.iter().enumerate().map(|(i,item)| date[i] && ferrite_render::instruction_visible(item,scale,groups,None)).collect();
        if patterns.iter().filter(|(i,r)|visible.get(**i).copied().unwrap_or(false) && r.is_ok()).count()>65536 {
            return Err("Globe pattern per-view material budget exceeded".into());
        }
        let mut pattern_materials = std::collections::HashMap::new();
        let mut whole_decisions = std::collections::HashMap::new();
        let mut whole_commands=0usize;
        let mut whole_decision_count=0usize;
        let mut pattern_ordinals: Vec<_> = patterns.keys().copied().collect();
        pattern_ordinals.sort_unstable();
        for ordinal in pattern_ordinals {
            let resolved = &patterns[&ordinal];
            if !visible.get(ordinal).copied().unwrap_or(false) {continue;}
            let Some(DrawingInstruction::Area(area)) = instructions.get(ordinal) else {continue};
            if let Ok(resource)=resolved {
                if let Some(whole)=&resource.whole {
                    whole_commands+=1;
                    if whole_commands>256 {return Err("Whole motif view command budget exceeded".into());}
                    let result=(|| {
                        let first=*area.exterior.first().ok_or("Whole motif area has no authored origin")?;
                        let origin=crate::globe_hatch::pattern_origin(area.pattern_crs,&camera,first,WorldPoint::new(0.,0.))?;
                        let domain=crate::whole_motif_plan::authored_domain(&area.exterior,&area.interiors,&camera)?;
                        crate::whole_motif_plan::select(&domain,&whole.resource,origin,[width as f64,height as f64],resource.lattice,ordinal)
                    })();
                    match result {Ok(decisions)=>{whole_decision_count=whole_decision_count.checked_add(decisions.len()).filter(|n|*n<=65536).ok_or("Whole motif view decision budget exceeded")?;if whole.texture.has_coverage && decisions.iter().any(|d|d.completely_contained) {whole_decisions.insert(ordinal,decisions);}},Err(reason)=>{*diagnostics.reasons.entry(reason).or_default()+=1;}}
                    continue;
                }
            }
            let result = (|| -> Result<crate::globe_pattern::PatternMaterial,String> {
                let resource = resolved.as_ref().map_err(Clone::clone)?;
                let first = *area.exterior.first().ok_or("Pattern area has no authored origin")?;
                let origin = crate::globe_hatch::pattern_origin(area.pattern_crs,&camera,first,WorldPoint::new(0.,0.))?;
                let params = crate::globe_pattern::PatternParams::for_view(resource.lattice,origin,[width as f64,height as f64])?;
                Ok(resource.texture.as_ref().ok_or("Missing periodic texture")?.material(&gpu.device,resources.scene.pattern_layout(),params))
            })();
            match result {
                Ok(material) => {if resolved.as_ref().is_ok_and(|r|r.texture.as_ref().is_some_and(|t|t.has_coverage)) {pattern_materials.insert(ordinal,material);}},
                Err(reason) => {*diagnostics.reasons.entry(reason).or_default()+=1;}
            }
        }
        egui.ensure_font_metrics(&gpu.window);
        let setup_ms = setup_start.elapsed().as_secs_f64() * 1000.;
        let geometry_start = std::time::Instant::now();
        let graph = context.dependency_graph();
        // Optimistic resource availability only prunes commands that cannot run.
        // Visible geometry and post-suppression execution still decide drawing below.
        let candidates: Vec<_> = instructions
            .iter()
            .enumerate()
            .map(|(i, item)| {
                if !visible[i] {
                    return false;
                }
                match item {
                    DrawingInstruction::Area(a)
                        if matches!(
                            a.fill,
                            AreaFillType::Solid(_) | AreaFillType::HatchFill { .. }
                        ) =>
                    {
                        true
                    }
                    DrawingInstruction::Area(a) if matches!(a.fill,AreaFillType::Pattern {..}) => {
                        let present = pattern_materials.contains_key(&i) || whole_decisions.contains_key(&i);
                        if !present {diagnostics.unsupported_commands += 1;}
                        present
                    }
                    DrawingInstruction::Line(l)
                        if crate::globe_geographic_arc::supports(l) =>
                    {
                        true
                    }
                    DrawingInstruction::Text(t) => t.has_visible_content(),
                    DrawingInstruction::Point(p) => {
                        let present = symbols
                            .get(&ferrite_render::intern_symbol(&p.symbol_ref))
                            .is_some_and(|t| t.has_coverage);
                        if !present {
                            diagnostics.missing_symbol_resources += 1;
                        }
                        present
                    }
                    _ => {
                        diagnostics.unsupported_commands += 1;
                        false
                    }
                }
            })
            .collect();
        let potential = graph.resolve(&candidates).map_err(str::to_string)?.executed;
        let preflight_pruned = candidates
            .iter()
            .zip(&potential)
            .filter(|(a, b)| **a && !**b)
            .count();
        let curve_prepare_start = std::time::Instant::now();
        let mut dyadic_evicted_bytes = 0usize;
        let mut dyadic_evicted_curves = 0usize;
        if cache_curves {
            for (i, item) in instructions.iter().enumerate() {
                if !potential[i] || resources.curve_cache.contains_key(&i) {
                    continue;
                }
                if let DrawingInstruction::Line(l) = item {
                    if l.style_ref.is_some()
                        || l.screen_ray.is_some()
                        || l.portrayal_path.is_some()
                        || l.style.color.a != 1.
                        || l.points.len() < 2
                    {
                        continue;
                    }
                    let estimate =
                        crate::globe_curve_clip::PreparedCurve::estimated_bytes(l.points.len())
                            + 64;
                    let evicted = crate::globe_curve_clip::reclaim_dyadic_for_route(
                        &mut resources.curve_cache,
                        &mut resources.curve_cache_bytes,
                        &potential,
                        estimate,
                        64 * 1024 * 1024,
                    );
                    dyadic_evicted_bytes += evicted.0;
                    dyadic_evicted_curves += evicted.1;
                    if estimate
                        > (64 * 1024 * 1024usize).saturating_sub(resources.curve_cache_bytes)
                    {
                        continue;
                    }
                    if let Ok(curve) = crate::globe_curve_clip::PreparedCurve::new(&l.points) {
                        resources.curve_cache_bytes += curve.bytes() + 64;
                        resources.curve_cache.insert(i, curve);
                    }
                }
            }
        }
        if cache_dyadic_samples && cache_curves {
            for (&id, curve) in resources.curve_cache.iter_mut() {
                if !potential[id] {
                    continue;
                }
                let free = (64 * 1024 * 1024usize).saturating_sub(resources.curve_cache_bytes);
                resources.curve_cache_bytes += curve.reserve_dyadic(free);
            }
        } else {
            for curve in resources.curve_cache.values_mut() {
                let bytes = curve.evict_dyadic();
                resources.curve_cache_bytes -= bytes;
                dyadic_evicted_bytes += bytes;
                dyadic_evicted_curves += usize::from(bytes > 0);
            }
        }
        let curve_source_prepare_ms = curve_prepare_start.elapsed().as_secs_f64() * 1000.;
        let mut suppression_cache_hits = 0usize;
        let mut area_cache_hits = 0usize;
        let mut whole_area_shared_hits = 0usize;
        let mut whole_area_shared_bytes = 0usize;
        let mut initial_area_builds = 0usize;
        let mut initial_area_culled = 0usize;
        let mut initial_line_builds = 0usize;
        let mut initial_line_culled = 0usize;
        let mut unsuppressed_line_reuses = 0usize;
        let mut suppressed_line_builds = 0usize;
        let mut valid_line_sources = vec![false; instructions.len()];
        let mut texts = std::collections::HashMap::new();
        let mut meshes: Vec<Option<std::sync::Arc<GlobeMesh>>> =
            (0..instructions.len()).map(|_| None).collect();
        // CPU-only draping is independent per instruction. Keep a bounded worker
        // pool and collect in source order; GPU/font work remains on the UI thread.
        let geometry_profiling = crate::profiler::is_profiling_enabled();
        // Parallel task durations are sums, not elapsed frame time.
        let source_budget = resources.area_source_cache.begin(
            resources.area_source_cache_enabled && cache_areas,
            context.geometry_revision(),
            instructions.len(),
            &potential,
            &area_spatial_candidates,
        );
        let source_captures = std::sync::Mutex::new(Vec::new());
        let retained_before=crate::globe_portrayal::retained_passing_diagnostics();
        let area_profile = crate::globe_portrayal::AreaProfile::default();
        let area_profile_ref = geometry_profiling.then_some(&area_profile);
        let area_task_ns = std::sync::atomic::AtomicU64::new(0);
        let line_task_ns = std::sync::atomic::AtomicU64::new(0);
        let area_compact_ns = std::sync::atomic::AtomicU64::new(0);
        let line_compact_ns = std::sync::atomic::AtomicU64::new(0);
        let draping_start = std::time::Instant::now();
        let mut text_layout_ms = 0.;
        let mut point_mesh_ms = 0.;
        let mut point_compact_ms = 0.;
        let pool = geometry_pool();
        use rayon::prelude::*;
        let mut draped: Vec<_> = pool.install(|| {
            instructions
                .par_iter()
                .enumerate()
                .map(|(i, item)| {
                    if !potential[i] {
                        return None;
                    }
                    let task_timer = geometry_profiling.then(std::time::Instant::now);
                    let compact_counter = if matches!(item, DrawingInstruction::Area(_)) {
                        &area_compact_ns
                    } else {
                        &line_compact_ns
                    };
                    let result = (|| {
                        // Preserve invalid palette diagnostics even for a culled cache.
                        let valid_solid = match item {
                            DrawingInstruction::Area(a) => match a.fill {
                                AreaFillType::Solid(c) => c
                                    .to_array()
                                    .iter()
                                    .all(|x| x.is_finite() && (0. ..=1.).contains(x)),
                                _ => false,
                            },
                            _ => false,
                        };
                        if !area_spatial_candidates[i] && valid_solid {
                            return Some(Ok((
                                std::sync::Arc::new(GlobeMesh {
                                    vertices: Vec::new(),
                                    indices: Vec::new(),
                                }),
                                true,
                                None,
                                false,
                            )));
                        }
                        match item {
                            DrawingInstruction::Area(a)
                                if matches!(a.fill, AreaFillType::Solid(_)) =>
                            {
                                if let AreaFillType::Solid(color) = a.fill {
                                    if let Some(cached) = resources.area_cache.get(&i) {
                                        match cached.reusable_mesh_profiled(
                                            &camera,
                                            color.to_array(),
                                            area_profile_ref,
                                        ) {
                                            Ok(Some(mesh)) => {
                                                match cached
                                                    .shared_whole_mesh(&camera, color.to_array())
                                                {
                                                    Ok(Some(shared)) => {
                                                        return Some(Ok((
                                                            shared, false, None, true,
                                                        )))
                                                    }
                                                    Ok(None) => {}
                                                    Err(reason) => return Some(Err(reason)),
                                                }
                                                return Some(
                                                    profile_compact_view_colored(
                                                        mesh,
                                                        &camera,
                                                        Some(color.to_array()),
                                                        geometry_profiling
                                                            .then_some(compact_counter),
                                                    )
                                                    .map(|mesh| {
                                                        (
                                                            std::sync::Arc::new(mesh),
                                                            false,
                                                            None,
                                                            true,
                                                        )
                                                    }),
                                                );
                                            }
                                            Err(reason) => return Some(Err(reason)),
                                            Ok(None) => {}
                                        }
                                    }
                                }
                                if !cache_areas {
                                    return Some(profile_compact_draped(
                                        crate::globe_portrayal::drape_area(
                                            a,
                                            &camera,
                                            Default::default(),
                                        )
                                        .map(
                                            |(mesh, stats)| {
                                                (mesh, stats.frustum_culled, None, false)
                                            },
                                        ),
                                        &camera,
                                        geometry_profiling.then_some(compact_counter),
                                    ));
                                }
                                Some(profile_compact_draped(
                                    crate::globe_portrayal::drape_area_cached_source_retained_options(
                                        a,
                                        &camera,
                                        Default::default(),
                                        area_profile_ref,
                                        resources.area_source_cache.get(i),
                                        source_budget.as_ref(),
                                        resources.area_midpoint_reuse_enabled,
                                        resources.area_chord_precheck_enabled,
                                        resources.diagnostic_retained_passing,
                                    )
                                    .map(
                                        |(mesh, stats, cache, capture)| {
                                            if let Some(capture) = capture {
                                                source_captures
                                                    .lock()
                                                    .expect("source capture lock")
                                                    .push((i, capture));
                                            }
                                            (mesh, stats.frustum_culled, cache, false)
                                        },
                                    ),
                                    &camera,
                                    geometry_profiling.then_some(compact_counter),
                                ))
                            }
                            DrawingInstruction::Line(l)
                                if crate::globe_geographic_arc::supports(l) =>
                            {
                                Some(profile_compact_draped(
                                    crate::globe_lines::drape_line_prepared_reusing(
                                        l,
                                        &camera,
                                        scaler.pixels_per_mm(),
                                        None,
                                        true,
                                        resources.curve_cache.get(&i),
                                        cache_curve_bounds,
                                        reuse_curve_scratch,
                                    )
                                    .map(|(mesh, stats)| (mesh, stats.frustum_culled, None, false)),
                                    &camera,
                                    geometry_profiling.then_some(compact_counter),
                                ))
                            }
                            _ => None,
                        }
                    })();
                    if let Some(timer) = task_timer {
                        let counter = match item {
                            DrawingInstruction::Area(_) => Some(&area_task_ns),
                            DrawingInstruction::Line(_) => Some(&line_task_ns),
                            _ => None,
                        };
                        if let Some(counter) = counter {
                            counter.fetch_add(
                                timer.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                    }
                    result
                })
                .collect()
        });
        for (index, capture) in source_captures.into_inner().expect("source capture lock") {
            resources.area_source_cache.commit(index, capture);
        }
        let source_cache_diagnostics = resources.area_source_cache.finish(source_budget.as_deref());
        let parallel_draping_wall_ms = draping_start.elapsed().as_secs_f64() * 1000.;
        let mut line_projection_cache = crate::globe_line_symbol::FrameCurveProjectionCache::new(
            &camera,
            context.geometry_revision(),
            scaler.pixels_per_mm() / (96. / 25.4),
            16 * 1024 * 1024,
        )
        .with_route_reuse(resources.line_route_reuse_enabled);
        let sequential_initial_start = std::time::Instant::now();
        for (i, item) in instructions.iter().enumerate() {
            if !potential[i] {
                continue;
            }
            let result = match item {
                DrawingInstruction::Area(a) if matches!(a.fill, AreaFillType::Solid(_)) => {
                    initial_area_builds += 1;
                    draped[i]
                        .take()
                        .expect("prepared area")
                        .map(|(mesh, culled, cache, hit)| {
                            initial_area_culled += usize::from(culled);
                            area_cache_hits += usize::from(hit);
                            if resources
                                .area_cache
                                .get(&i)
                                .is_some_and(|c| c.shares_mesh(&mesh))
                            {
                                whole_area_shared_hits += 1;
                                whole_area_shared_bytes += mesh.vertices.len()
                                    * std::mem::size_of::<crate::globe_scene::GlobeVertex>()
                                    + mesh.indices.len() * 4;
                            }
                            if let Some(cache) = cache {
                                resources.area_spatial_dirty = true;
                                if let Some(old) = resources.area_cache.remove(&i) {
                                    resources.area_cache_bytes -= old.bytes();
                                }
                                let bytes = cache.bytes();
                                if resources.area_cache_bytes + bytes <= 128 * 1024 * 1024 {
                                    resources.area_cache_bytes += bytes;
                                    resources.area_cache.insert(i, cache);
                                }
                            }
                            mesh
                        })
                }
                DrawingInstruction::Area(a) if matches!(a.fill,AreaFillType::Pattern {..}) => {
                    initial_area_builds += 1;
                    crate::globe_portrayal::drape_area_geometry(&a.exterior,&a.interiors,&camera,crate::globe_portrayal::DrapingLimits::default())
                        .and_then(|(mesh,_)|compact_view(mesh,&camera))
                        .map(std::sync::Arc::new)
                }
                DrawingInstruction::Area(a) if matches!(a.fill, AreaFillType::HatchFill { .. }) => {
                    initial_area_builds += 1;
                    crate::globe_hatch::drape_hatch_area(
                        a,
                        &camera,
                        scaler.pixels_per_mm(),
                        WorldPoint::new(0., 0.),
                    )
                    .and_then(|mesh| compact_view(mesh, &camera))
                    .map(std::sync::Arc::new)
                }
                DrawingInstruction::Line(l)
                    if crate::globe_geographic_arc::supports(l) =>
                {
                    initial_line_builds += 1;
                    draped[i]
                        .take()
                        .expect("prepared line")
                        .map(|(mesh, culled, _, _)| {
                            valid_line_sources[i] = true;
                            initial_line_culled += usize::from(culled);
                            mesh
                        })
                }
                DrawingInstruction::Text(t) => {
                    let timer = geometry_profiling.then(std::time::Instant::now);
                    let result = crate::globe_text::GlobeText::layout(
                        t,
                        &camera,
                        &egui.ctx,
                        scaler.pixels_per_mm(),
                    );
                    if let Some(timer) = timer {
                        text_layout_ms += timer.elapsed().as_secs_f64() * 1000.;
                    }
                    match result {
                        Ok(Some(layout)) => {
                            texts.insert(i, layout);
                            continue;
                        }
                        Ok(None) => continue,
                        Err(reason) => Err(reason),
                    }
                }
                DrawingInstruction::Point(p) => {
                    let Some(tex) = symbols
                        .get(&ferrite_render::intern_symbol(&p.symbol_ref))
                        .filter(|t| t.has_coverage)
                    else {
                        continue;
                    };
                    let timer = geometry_profiling.then(std::time::Instant::now);
                    let result = if resources.line_projection_reuse_enabled {
                        crate::globe_line_symbol::symbol_mesh_cached(
                            p,
                            &mut line_projection_cache,
                            [tex.width, tex.height],
                            [tex.pivot_in_texture.0, tex.pivot_in_texture.1],
                            tex.render_scale,
                            symbol_scale,
                        )
                    } else {
                        crate::globe_line_symbol::symbol_mesh_routes(
                            p,
                            &camera,
                            [tex.width, tex.height],
                            [tex.pivot_in_texture.0, tex.pivot_in_texture.1],
                            tex.render_scale,
                            scaler.pixels_per_mm() / (96. / 25.4),
                            symbol_scale,
                            resources.line_route_reuse_enabled,
                        )
                    }
                    .map(std::sync::Arc::new);
                    if let Some(timer) = timer {
                        point_mesh_ms += timer.elapsed().as_secs_f64() * 1000.;
                    }
                    result
                }
                _ => {
                    continue;
                }
            };
            match result {
                Ok(mesh) => {
                    let mesh = if matches!(
                        item,
                        DrawingInstruction::Area(_) | DrawingInstruction::Line(_)
                    ) {
                        mesh
                    } else {
                        let timer = geometry_profiling.then(std::time::Instant::now);
                        let result =
                            std::sync::Arc::new(compact_view_colored(&mesh, &camera, None)?);
                        if let Some(timer) = timer {
                            point_compact_ms += timer.elapsed().as_secs_f64() * 1000.;
                        }
                        result
                    };
                    if !mesh.indices.is_empty() {
                        meshes[i] = Some(mesh);
                    }
                }
                Err(reason) => {
                    diagnostics.rejected_geometries += 1;
                    *diagnostics.reasons.entry(reason).or_default() += 1;
                }
            }
        }
        let sequential_initial_wall_ms = sequential_initial_start.elapsed().as_secs_f64() * 1000.;
        let text_mesh_start = std::time::Instant::now();
        // Allocate every required glyph before normalizing atlas UVs. Atlas growth
        // must not invalidate meshes produced earlier in this same preparation.
        egui.flush_chart_fonts(&gpu.device, &gpu.queue);
        let font_atlas = egui.chart_atlas_bind_group(egui::TextureId::default());
        if !texts.is_empty() && font_atlas.is_none() {
            return Err("Globe shared font atlas unavailable".into());
        }
        let font_atlas_prepare_ms = text_mesh_start.elapsed().as_secs_f64() * 1000.;
        let mut text_mesh_ms = 0.;
        let mut text_compact_ms = 0.;
        for (&i, layout) in &texts {
            let timer = geometry_profiling.then(std::time::Instant::now);
            let source_mesh = layout.mesh(&camera, &egui.ctx)?;
            if let Some(timer) = timer {
                text_mesh_ms += timer.elapsed().as_secs_f64() * 1000.;
            }
            let timer = geometry_profiling.then(std::time::Instant::now);
            let mut mesh = compact_view(source_mesh, &camera)?;
            if mesh.indices.is_empty() {
                if let Some(background) = &layout.background {
                    mesh = compact_view(background.clone(), &camera)?;
                }
            }
            if let Some(timer) = timer {
                text_compact_ms += timer.elapsed().as_secs_f64() * 1000.;
            }
            if !mesh.indices.is_empty() && font_atlas.is_some() {
                meshes[i] = Some(std::sync::Arc::new(mesh));
            }
        }
        // Final availability requires at least one actual accepted surface crop
        // and matching natural material. A loaded graphic alone cannot execute.
        let mut whole_geometry=std::collections::HashMap::new();
        let mut whole_materials=std::collections::HashMap::new();
        let mut whole_vertices=0usize;
        let mut whole_draws=0usize;
        let mut whole_ordinals:Vec<_>=whole_decisions.keys().copied().collect();
        whole_ordinals.sort_unstable();
        for i in whole_ordinals {
            let decisions=&whole_decisions[&i];
            let result=(|| {
                let source=meshes[i].as_ref().ok_or("Whole motif source has no visible surface")?;
                let whole=patterns[&i].as_ref().map_err(Clone::clone)?.whole.as_ref().ok_or("Missing whole motif resource")?;
                let crops=crate::whole_motif_plan::surface_meshes(source,&camera,&whole.resource,decisions)?;
                let count:usize=crops.iter().map(|(_,m)|m.vertices.len()).sum();
                let next=whole_vertices.checked_add(count).filter(|n|*n<=262144).ok_or("Whole motif view vertex budget exceeded")?;
                let next_draws=whole_draws.checked_add(crops.len()).filter(|n|*n<=4096).ok_or("Whole motif view material budget exceeded")?;
                let mut materials=Vec::with_capacity(crops.len());
                for (site,_) in &crops {materials.push(whole.texture.material(&gpu.device,resources.scene.pattern_layout(),site.origin,[width as f64,height as f64])?);}
                whole_vertices=next;whole_draws=next_draws;
                Ok::<_,String>((crops,materials))
            })();
            match result {
                Ok((crops,materials)) if !crops.is_empty()=>{whole_geometry.insert(i,crops);whole_materials.insert(i,materials);},
                Ok(_)=>{meshes[i]=None;},
                Err(reason) if reason=="Whole motif view vertex budget exceeded" || reason=="Whole motif view material budget exceeded"=>{return Err(reason);},
                Err(reason)=>{meshes[i]=None;diagnostics.rejected_geometries+=1;*diagnostics.reasons.entry(reason).or_default()+=1;}
            }
        }
        let initial_geometry_ms = geometry_start.elapsed().as_secs_f64() * 1000.;
        let execution_start = std::time::Instant::now();
        // Resource failures/unsupported parents never authorize dependent commands.
        let available: Vec<_> = meshes.iter().map(Option::is_some).collect();
        let mut permission = graph.resolve(&available).map_err(str::to_string)?.executed;
        let mut scene = Vec::new();
        let mut scene_symbols = Vec::new();
        let mut scene_texts = Vec::new();
        let mut scene_sources = Vec::new();
        let mut scene_whole_slots = Vec::new();
        let mut previous = None;
        // Match dependency permissions to actual post-suppression execution,
        // including parents whose visible line intervals become empty.
        for pass in 0..65 {
            scene.clear();
            scene_symbols.clear();
            scene_texts.clear();
            scene_sources.clear();
            scene_whole_slots.clear();
            diagnostics.areas = 0;
            diagnostics.lines = 0;
            diagnostics.symbols = 0;
            diagnostics.texts = 0;
            diagnostics.text_footprints_px.clear();
            diagnostics.symbol_footprints_px.clear();
            // Exact source revision, lifted meridian and execution eligibility
            // preserve date/group/resource/dependency-driven suppression semantics.
            let line_permission: Vec<_> = permission
                .iter()
                .zip(instructions)
                .map(|(visible, item)| *visible && matches!(item, DrawingInstruction::Line(_)))
                .collect();
            let suppression = if let Some((_, _, plan, _)) = resources
                .suppression_cache
                .iter()
                .find(|(meridian, eligible, _, _)| *meridian == lon && *eligible == line_permission)
            {
                suppression_cache_hits += 1;
                plan.clone()
            } else {
                if !resources
                    .prepared_suppression
                    .as_ref()
                    .map(|p| p.matches(instructions, &line_permission, lon))
                    .transpose()?
                    .unwrap_or(false)
                {
                    resources.prepared_suppression = crate::globe_lines::PreparedSuppression::new(
                        instructions,
                        &valid_line_sources,
                        lon,
                    )?;
                }
                let plan = if let Some(prepared) = resources.prepared_suppression.as_mut() {
                    prepared.plan(&line_permission)?
                } else {
                    crate::globe_lines::geographic_line_suppression(instructions, &permission, lon)?
                };
                let bytes = permission.len()
                    + plan.fully_suppressed.capacity() * 16
                    + plan.partial.capacity() * 64
                    + plan
                        .partial
                        .values()
                        .map(|v| v.capacity() * std::mem::size_of::<ferrite_render::LineSpan>())
                        .sum::<usize>();
                if bytes <= 16 * 1024 * 1024 {
                    while resources.suppression_cache.len() >= 4
                        || resources
                            .suppression_cache
                            .iter()
                            .map(|entry| entry.3)
                            .sum::<usize>()
                            + bytes
                            > 16 * 1024 * 1024
                    {
                        resources.suppression_cache.remove(0);
                    }
                    resources.suppression_cache.push((
                        lon,
                        permission.clone(),
                        plan.clone(),
                        bytes,
                    ));
                }
                plan
            };
            let mut accepted_text = vec![false; instructions.len()];
            let mut placement = ferrite_render::TextPlacement::default();
            let mut order: Vec<_> = texts.keys().copied().filter(|&i| permission[i]).collect();
            order.sort_by_key(|&i| {
                (
                    std::cmp::Reverse((
                        instructions[i]
                            .display_plane()
                            .composition_plane(ferrite_kernel::CompositionStage::Chart),
                        instructions[i].priority().0,
                    )),
                    i,
                )
            });
            for i in order {
                accepted_text[i] = placement.try_place(texts[&i].footprint);
            }
            let mut executed = vec![false; instructions.len()];
            for (i, item) in instructions.iter().enumerate() {
                if !permission[i] || (texts.contains_key(&i) && !accepted_text[i]) {
                    continue;
                }
                let mesh = if let DrawingInstruction::Line(line) = item {
                    if let Some(spans) = suppression.spans(i) {
                        suppressed_line_builds += 1;
                        match crate::globe_lines::drape_line_prepared_reusing(
                            line,
                            &camera,
                            scaler.pixels_per_mm(),
                            Some(spans),
                            true,
                            resources.curve_cache.get(&i),
                            cache_curve_bounds,
                            reuse_curve_scratch,
                        ) {
                            Ok((mesh, _)) => std::sync::Arc::new(compact_view(mesh, &camera)?),
                            Err(reason) => {
                                if pass == 0 {
                                    diagnostics.rejected_geometries += 1;
                                    *diagnostics.reasons.entry(reason).or_default() += 1;
                                }
                                continue;
                            }
                        }
                    } else {
                        // Same camera/calibration/source path as initial geometry.
                        // None means no suppression; retain the original dash phase.
                        unsuppressed_line_reuses += 1;
                        meshes[i].as_ref().expect("available actual line").clone()
                    }
                } else {
                    meshes[i]
                        .as_ref()
                        .expect("available actual geometry")
                        .clone()
                };
                if mesh.indices.is_empty() {
                    continue;
                }
                executed[i] = true;
                match item {
                    DrawingInstruction::Area(_) => diagnostics.areas += 1,
                    DrawingInstruction::Line(_) => diagnostics.lines += 1,
                    DrawingInstruction::Point(_) => {
                        diagnostics.symbols += mesh.indices.len() / 6;
                        for triangle in mesh.indices.chunks_exact(3) {
                            let mut bounds = [
                                f64::INFINITY,
                                f64::INFINITY,
                                f64::NEG_INFINITY,
                                f64::NEG_INFINITY,
                            ];
                            for index in triangle {
                                let vertex = &mesh.vertices[*index as usize];
                                let c =
                                    camera.clip_ecef(vertex.ecef_m).map_err(|e| e.to_string())?;
                                let x = v.x as f64 + (c[0] / c[3] + 1.) * v.width as f64 * 0.5;
                                let y = v.y as f64 + (1. - c[1] / c[3]) * v.height as f64 * 0.5;
                                bounds[0] = bounds[0].min(x);
                                bounds[1] = bounds[1].min(y);
                                bounds[2] = bounds[2].max(x);
                                bounds[3] = bounds[3].max(y);
                            }
                            diagnostics
                                .symbol_footprints_px
                                .push(bounds.map(|x| x as f32));
                        }
                    }
                    DrawingInstruction::Text(_) => {
                        diagnostics.texts += 1;
                        let text = &texts[&i];
                        let [min, max] = text.footprint.bounds();
                        let ppp = text.pixels_per_point;
                        diagnostics.text_footprints_px.push([
                            min[0] * ppp + v.x,
                            min[1] * ppp + v.y,
                            max[0] * ppp + v.x,
                            max[1] * ppp + v.y,
                        ]);
                    }
                }
                if let Some(crops)=whole_geometry.get(&i) {
                    for (slot,(_,crop)) in crops.iter().enumerate() {scene.push(crop.clone());scene_symbols.push(None);scene_texts.push(None);scene_sources.push(i);scene_whole_slots.push(Some(slot));}
                    continue;
                }
                if let Some(text) = texts.get(&i) {
                    if let Some(background) = &text.background {
                        scene.push(std::sync::Arc::new(compact_view(
                            background.clone(),
                            &camera,
                        )?));
                        scene_symbols.push(None);
                        scene_texts.push(text.background_color);
                        scene_sources.push(i);
                        scene_whole_slots.push(None);
                    }
                    if text.foreground[3] == 0 {
                        continue;
                    }
                }
                scene_texts.push(texts.get(&i).map(|t| t.foreground));
                scene_symbols.push(if let DrawingInstruction::Point(p) = item {
                    Some(ferrite_render::intern_symbol(&p.symbol_ref))
                } else {
                    None
                });
                scene.push(mesh);
                scene_sources.push(i);
                scene_whole_slots.push(None);
            }
            let grounded = graph.resolve(&executed).map_err(str::to_string)?;
            diagnostics.ungrounded_commands =
                grounded.ungrounded_count + grounded.missing_parent_count;
            let permitted = graph
                .permitted_by_executed(&grounded.executed)
                .map_err(str::to_string)?;
            let next: Vec<_> = permitted
                .iter()
                .zip(&available)
                .map(|(a, b)| *a && *b)
                .collect();
            if next == permission
                || pass == 64
                || diagnostics.reasons.contains_key(
                    "Dependency execution did not converge; dependent commands withheld",
                )
            {
                break;
            }
            if previous.as_ref() == Some(&next) || pass == 63 {
                diagnostics.reasons.insert(
                    "Dependency execution did not converge; dependent commands withheld".into(),
                    1,
                );
                permission = instructions
                    .iter()
                    .zip(&available)
                    .map(|(i, a)| *a && i.dependency().is_none_or(|d| d.parent_id.is_none()))
                    .collect();
                // The last pass retains roots only, matching flat fail-closed policy.
                previous = None;
            } else {
                previous = Some(std::mem::replace(&mut permission, next));
            }
        }
        let dyadic_bytes = resources
            .curve_cache
            .values()
            .map(|c| c.dyadic_bytes())
            .sum::<usize>();
        let scratch_stats = resources
            .curve_cache
            .values()
            .fold([0usize; 4], |mut v, c| {
                let n = c.scratch_stats();
                v[0] += n[0];
                v[1] += n[1];
                v[2] = v[2].max(n[2]);
                v[3] += n[3];
                v
            });
        let dyadic_stats = resources
            .curve_cache
            .values()
            .fold([0usize; 3], |mut total, c| {
                for (sum, n) in total.iter_mut().zip(c.dyadic_stats()) {
                    *sum += n;
                }
                total
            });
        diagnostics.preparation = serde_json::json!({"spatial_hierarchy_enabled":spatial_hierarchy,"spatial_inside_subtrees":area_spatial_stats.inside_subtrees,"spatial_inside_entries":area_spatial_stats.inside_entries,"spatial_nodes_tested":area_spatial_stats.nodes_tested,"spatial_leaves_tested":area_spatial_stats.leaves_tested,"spatial_candidates":area_spatial_stats.selected,"spatial_hierarchy_bytes":resources.area_spatial.retained_bytes(),"dependency_plan_bytes":graph.retained_bytes(),"curve_dyadic_cache_enabled":cache_dyadic_samples,"curve_dyadic_bytes":dyadic_bytes,"curve_dyadic_hits":dyadic_stats[0],"curve_dyadic_misses":dyadic_stats[1],"curve_dyadic_fallbacks":dyadic_stats[2],"curve_dyadic_evicted_bytes":dyadic_evicted_bytes,"curve_dyadic_evicted_curves":dyadic_evicted_curves,"curve_bounds_cache_enabled":cache_curve_bounds,"curve_cache_enabled":cache_curves,"curve_cache_entries":resources.curve_cache.len(),"curve_cache_bytes":resources.curve_cache_bytes,"curve_cache_budget_bytes":64*1024*1024,"curve_source_prepare_ms":curve_source_prepare_ms,"area_cache_enabled":cache_areas,"prepared_suppression_bytes":resources.prepared_suppression.as_ref().map_or(0,|p|p.retained_bytes()),"suppression_cache_hits":suppression_cache_hits,"suppression_cache_entries":resources.suppression_cache.len(),"suppression_cache_bytes":resources.suppression_cache.iter().map(|e|e.3).sum::<usize>(),"whole_area_shared_hits":whole_area_shared_hits,"whole_area_shared_bytes":whole_area_shared_bytes,"area_cache_hits":area_cache_hits,"area_cache_entries":resources.area_cache.len(),"area_cache_bytes":resources.area_cache_bytes,"area_cache_budget_bytes":128*1024*1024,"resource_preflight_pruned":preflight_pruned,
            "initial_area_culled":initial_area_culled,"initial_area_builds":initial_area_builds,"initial_line_culled":initial_line_culled,"initial_line_builds":initial_line_builds,
            "unsuppressed_line_reuses":unsuppressed_line_reuses,"suppressed_line_builds":suppressed_line_builds,
            "initial_geometry_ms":initial_geometry_ms,"execution_ms":execution_start.elapsed().as_secs_f64()*1000.});
        diagnostics.preparation["line_projection_reuse_enabled"] =
            resources.line_projection_reuse_enabled.into();
        diagnostics.preparation["line_projection_cache"] = line_projection_cache.diagnostics();
        diagnostics.preparation["line_route_reuse_enabled"] =
            resources.line_route_reuse_enabled.into();
        if let serde_json::Value::Object(stage_fields) = serde_json::json!({"area_chord_precheck_enabled":resources.area_chord_precheck_enabled,"area_midpoint_reuse_enabled":resources.area_midpoint_reuse_enabled,"curve_scratch_reuse_enabled":reuse_curve_scratch,"curve_scratch_calls":scratch_stats[0],"curve_scratch_reused_segments":scratch_stats[1],"curve_scratch_peak_retained_bytes":scratch_stats[2],"curve_scratch_discards":scratch_stats[3],"curve_scratch_retained_limit_bytes":256*1024,"geometry_stage_profiling_enabled":geometry_profiling,"parallel_draping_wall_ms":parallel_draping_wall_ms,"area_draping_task_sum_ms":area_task_ns.load(std::sync::atomic::Ordering::Relaxed) as f64/1e6,"line_draping_task_sum_ms":line_task_ns.load(std::sync::atomic::Ordering::Relaxed) as f64/1e6,"parallel_area_compact_task_sum_ms":area_compact_ns.load(std::sync::atomic::Ordering::Relaxed) as f64/1e6,"parallel_line_compact_task_sum_ms":line_compact_ns.load(std::sync::atomic::Ordering::Relaxed) as f64/1e6,"sequential_initial_wall_ms":sequential_initial_wall_ms,"text_layout_ms":text_layout_ms,"point_mesh_ms":point_mesh_ms,"point_compact_ms":point_compact_ms,"font_atlas_prepare_ms":font_atlas_prepare_ms,"text_mesh_ms":text_mesh_ms,"text_compact_ms":text_compact_ms})
        {
            diagnostics
                .preparation
                .as_object_mut()
                .expect("preparation diagnostics object")
                .extend(stage_fields);
        }
        if let serde_json::Value::Object(fields) = area_profile.diagnostics() {
            diagnostics
                .preparation
                .as_object_mut()
                .expect("preparation diagnostics object")
                .extend(fields);
        }
        if let serde_json::Value::Object(fields) = source_cache_diagnostics {
            diagnostics
                .preparation
                .as_object_mut()
                .expect("preparation diagnostics object")
                .extend(fields);
        }
        diagnostics.vertices =
            resources.earth.vertices.len() + scene.iter().map(|m| m.vertices.len()).sum::<usize>();
        diagnostics.triangles = resources.earth.indices.len() / 3
            + scene.iter().map(|m| m.indices.len() / 3).sum::<usize>();
        let raster_start=std::time::Instant::now();
        if rasters.len()>4096 {return Err("Globe raster view resource budget exceeded".into());}
        let mut raster_meshes=Vec::new();
        let mut shared_raster_meshes:std::collections::BTreeMap<[u64;6],GlobeMesh>=std::collections::BTreeMap::new();
        let mut raster_vertices=0usize;
        let mut raster_triangles=0usize;
        let mut raster_hidden_groups=0usize;
        for (i,raster) in rasters.iter().enumerate() {
            if !ferrite_render::raster_groups_visible(raster.viewing_groups,groups) {raster_hidden_groups+=1;continue;}
            let exact=crate::globe_raster::tile_bounds(raster.grid,raster.tile_size)?;
            if [raster.bounds.min_x-exact.min_x,raster.bounds.max_x-exact.max_x,raster.bounds.min_y-exact.min_y,raster.bounds.max_y-exact.max_y].iter().any(|v|!v.is_finite() || v.abs()>1e-9) {return Err("Globe raster tile bounds disagree with shared lattice".into());}
            let b=raster.grid.bounds;
            let key=[b.min_x.to_bits(),b.min_y.to_bits(),b.max_x.to_bits(),b.max_y.to_bits(),u64::from(raster.grid.width),u64::from(raster.grid.height)];
            if !shared_raster_meshes.contains_key(&key) {
                let mut whole=raster.grid;whole.column=0;whole.row=0;
                let (mesh,_)=crate::globe_raster::mesh(b,whole,[whole.width,whole.height],&camera)?;
                let mesh=compact_view(mesh,&camera)?;
                shared_raster_meshes.insert(key,mesh);
            }
            let mut mesh=shared_raster_meshes[&key].clone();
            for v in &mut mesh.vertices {
                v.color[2]=if raster.continuous {1048576.}else{raster.grid.column as f32};
                v.color[3]=if raster.continuous {0.}else{raster.grid.row as f32};
            }
            if mesh.indices.is_empty() {continue;}
            raster_vertices=raster_vertices.checked_add(mesh.vertices.len()).filter(|n|*n<=262144).ok_or("Globe raster view vertex budget exceeded")?;
            raster_triangles=raster_triangles.checked_add(mesh.indices.len()/3).ok_or("Globe raster view triangle overflow")?;
            raster_meshes.push((i,mesh));
        }
        diagnostics.preparation["raster_shared_grid_meshes"]=shared_raster_meshes.len().into();
        diagnostics.vertices+=raster_vertices;
        diagnostics.triangles+=raster_triangles;
        diagnostics.preparation["raster_surface_count"]=raster_meshes.len().into();
        diagnostics.preparation["raster_group_hidden"]=raster_hidden_groups.into();
        diagnostics.preparation["raster_surface_vertices"]=raster_vertices.into();
        diagnostics.preparation["raster_surface_ms"]=(raster_start.elapsed().as_secs_f64()*1000.).into();
        // Stable plane/priority merge: retain existing PC order among vectors,
        // and source tile order among equal-key rasters. IC Chart and product
        // Overlay stages use the same product-neutral composition key as 2D.
        let mut ordered=Vec::with_capacity(scene.len()+raster_meshes.len());
        for i in 0..scene.len() {
            let item=&instructions[scene_sources[i]];
            ordered.push(((item.display_plane().composition_plane(ferrite_kernel::CompositionStage::Chart),item.priority().0),false,i));
        }
        for (i,(source,_)) in raster_meshes.iter().enumerate() {ordered.push((rasters[*source].draw_order.render_key(),true,i));}
        ordered.sort_by_key(|v|v.0);
        let draw_source_ordinals:Vec<_>=ordered.iter().map(|(_,raster,i)|if *raster {None} else {Some(scene_sources[*i])}).collect();
        let layers: Vec<_> = std::iter::once(GlobeDraw {
            layer:GlobeLayer{mesh:&resources.earth,depth_mode:GlobeDepthMode::Occluder},texture:None,pattern:None,font_color:None,
        }).chain(ordered.iter().map(|(_,raster,i)| {
            if *raster {
                let (source,mesh)=&raster_meshes[*i];
                GlobeDraw{layer:GlobeLayer{mesh,depth_mode:GlobeDepthMode::SourceGridTexture},texture:Some(rasters[*source].bind_group),pattern:None,font_color:None}
            } else {
                let i=*i;
                GlobeDraw {
                    layer: GlobeLayer {mesh:&scene[i],depth_mode:if matches!(instructions[scene_sources[i]],DrawingInstruction::Line(_)) {GlobeDepthMode::ScreenOverlay} else {GlobeDepthMode::SurfaceOverlay}},
                    texture:if scene_texts[i].is_some() {font_atlas.as_ref()} else {scene_symbols[i].map(|id|&symbols[&id].bind_group)},
                    font_color:scene_texts[i],
                    pattern:if let Some(slot)=scene_whole_slots[i] {Some(whole_materials[&scene_sources[i]][slot].scene_material())} else {pattern_materials.get(&scene_sources[i])},
                }
            }
        })).collect();
        if resources.target.as_ref().map(|t| t.0) != Some([width, height]) {
            let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Globe chart pane"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: gpu.format(),
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let bind_group = pipelines.create_texture_bind_group(&gpu.device, &view);
            resources.target = Some(([width, height], texture, view, bind_group));
            resources.color_allocations += 1;
        }
        let retained_after=crate::globe_portrayal::retained_passing_diagnostics();
        diagnostics.preparation["retained_passing_enabled"]=resources.diagnostic_retained_passing.into();
        diagnostics.preparation["retained_passing"]=serde_json::json!({"fallback_attempts":retained_after.0[0].saturating_sub(retained_before.0[0]),"passing_accepted":retained_after.0[1].saturating_sub(retained_before.0[1]),"pairs_tested":retained_after.0[2].saturating_sub(retained_before.0[2]),"proof_refusals":retained_after.0[3].saturating_sub(retained_before.0[3]),"reserved_bytes":retained_after.1,"peak_bytes":retained_after.2,"aggregate_limit_bytes":32*1024*1024,"per_area_limit_bytes":2*1024*1024});
        let target = resources.target.as_ref().unwrap();
        let gpu_prepare_start = std::time::Instant::now();
        let retained: Vec<_> = if gpu_projection {
            let cached: rustc_hash::FxHashSet<_> = resources
                .area_cache
                .values()
                .map(|c| c.mesh_identity())
                .collect();
            scene
                .iter()
                .filter(|m| cached.contains(&(std::sync::Arc::as_ptr(m) as usize)))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        resources.scene.bind_retained_geometry(&retained);
        resources.scene.set_gpu_projection_enabled(gpu_projection);
        resources.scene.set_coverage_batching_enabled(
            std::env::var_os("FERRITE_GLOBE_COVERAGE_BATCHING").is_some(),
        );
        resources
            .scene
            .require_coverage(prepared_coverage.is_some());
        resources
            .scene
            .prepare_draws(&gpu.device, &gpu.queue, &camera, &layers)?;
        if let Some(prepared) = &prepared_coverage {
            prepared
                .validate(
                    context.geometry_revision(),
                    context.coverage_view_revision(),
                    context.instruction_count(),
                )
                .map_err(|e| e.to_string())?;
            let pass = prepared.pass(0).map_err(|e| e.to_string())?;
            let mut decisions = Vec::with_capacity(draw_source_ordinals.len() + 1);
            decisions.push(ferrite_kernel::coverage_frame::FrameCoverageDecision::Unclipped); // Earth
            for source in &draw_source_ordinals {
                decisions.push(if let Some(source)=source {pass.decision(*source).map_err(|e|e.to_string())?} else {ferrite_kernel::coverage_frame::FrameCoverageDecision::Unclipped});
            }
            let epoch = resources
                .scene
                .coverage_epoch()
                .ok_or("Globe camera was not prepared")?;
            resources.scene.bind_prepared_coverage(
                &gpu.device,
                &gpu.queue,
                epoch,
                pass.frame(),
                &decisions,
                128 * 1024 * 1024,
            )?;
        }
        let clip_and_upload_ms = gpu_prepare_start.elapsed().as_secs_f64() * 1000.;
        let encode_start = std::time::Instant::now();
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("prepare actual globe pane"),
            });
        resources.scene.render(
            &gpu.device,
            &mut encoder,
            &target.2,
            [width, height],
            wgpu::Color {
                r: 0.01,
                g: 0.02,
                b: 0.035,
                a: 1.,
            },
        )?;
        gpu.queue.submit(Some(encoder.finish()));
        diagnostics.preparation["setup_ms"] = setup_ms.into();
        diagnostics.preparation["date_visibility_ms"] = date_visibility_ms.into();
        diagnostics.preparation["clip_and_upload_ms"] = clip_and_upload_ms.into();
        diagnostics.preparation["encoding_and_submit_ms"] =
            (encode_start.elapsed().as_secs_f64() * 1000.).into();
        let quad = [
            TextureVertex::new(v.x, v.y, 0., 0., [0., 0.]),
            TextureVertex::new(v.x + v.width, v.y, 1., 0., [0., 0.]),
            TextureVertex::new(v.x + v.width, v.y + v.height, 1., 1., [0., 0.]),
            TextureVertex::new(v.x, v.y + v.height, 0., 1., [0., 0.]),
        ];
        let rect = [v.x, v.y, v.width, v.height];
        if resources.quad != Some(rect) {
            gpu.queue
                .write_buffer(&resources.vertices, 0, bytemuck::cast_slice(&quad));
            resources.quad = Some(rect);
            resources.quad_writes += 1;
        }
        resources.frames += 1;
        let mut usage = resources.scene.resource_usage();
        usage["color_allocations"] = resources.color_allocations.into();
        usage["composite_buffer_allocations"] = 2.into();
        usage["quad_writes"] = resources.quad_writes.into();
        usage["frames"] = resources.frames.into();
        usage["earth_mesh_builds"] = 1.into();
        usage["color_target_bytes"] = (width as u64 * height as u64 * 4).into();
        usage["depth_target_bytes"] =
            (width as u64 * height as u64 * 4 * sample_count as u64).into();
        usage["msaa_color_target_bytes"] = (if sample_count > 1 {
            width as u64 * height as u64 * 4 * sample_count as u64
        } else {
            0
        })
        .into();
        diagnostics.resources = usage;
        Ok(Self {
            _texture: target.1.clone(),
            bind_group: target.3.clone(),
            vertices: resources.vertices.clone(),
            indices: resources.indices.clone(),
            camera: MapCamera::Globe {
                camera,
                viewport_origin: [v.x as f64, v.y as f64],
            },
            source: scaler.flat_transform(),
            source_pose,
            source_pixels_per_mm: scaler.pixels_per_mm(),
            source_range_factor: range_factor,
            source_ordinals: draw_source_ordinals.iter().flatten().copied().collect(),
            draw_source_ordinals,
            source_geometry_revision: context.geometry_revision(),
            diagnostics,
        })
    }
}
type DrapedGeometry = (
    GlobeMesh,
    bool,
    Option<crate::globe_portrayal::CachedArea>,
    bool,
);
fn compact_draped(
    result: Result<DrapedGeometry, String>,
    camera: &GlobeCamera,
) -> Result<
    (
        std::sync::Arc<GlobeMesh>,
        bool,
        Option<crate::globe_portrayal::CachedArea>,
        bool,
    ),
    String,
> {
    let (mesh, culled, cached, hit) = result?;
    Ok((
        std::sync::Arc::new(compact_view(mesh, camera)?),
        culled,
        cached,
        hit,
    ))
}
// Homogeneous frustum rejection only; straddling triangles remain for GPU clipping.
fn compact_view(mesh: GlobeMesh, camera: &GlobeCamera) -> Result<GlobeMesh, String> {
    compact_view_colored(&mesh, camera, None)
}
fn compact_view_colored(
    mesh: &GlobeMesh,
    camera: &GlobeCamera,
    color: Option<[f32; 4]>,
) -> Result<GlobeMesh, String> {
    // Each vertex contributes one homogeneous outcode. A triangle is rejected
    // only when all three vertices lie strictly outside the same clip plane.
    // This preserves straddling triangles and uses one byte per source vertex.
    let codes: Vec<_> = mesh
        .vertices
        .iter()
        .map(|v| {
            camera
                .clip_ecef(v.ecef_m)
                .map(frustum_code)
                .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?;
    let mut indices = Vec::with_capacity(mesh.indices.len());
    for t in mesh.indices.chunks_exact(3) {
        if codes[t[0] as usize] & codes[t[1] as usize] & codes[t[2] as usize] == 0 {
            indices.extend_from_slice(t);
        }
    }
    let mut map = vec![u32::MAX; mesh.vertices.len()];
    let mut vertices = Vec::new();
    for i in &mut indices {
        let old = *i as usize;
        if map[old] == u32::MAX {
            map[old] = vertices.len() as u32;
            let mut vertex = mesh.vertices[old];
            if let Some(color) = color {
                vertex.color = color;
            }
            vertices.push(vertex);
        }
        *i = map[old];
    }
    Ok(GlobeMesh { vertices, indices })
}

fn frustum_code(p: [f64; 4]) -> u8 {
    u8::from(p[0] < -p[3])
        | (u8::from(p[0] > p[3]) << 1)
        | (u8::from(p[1] < -p[3]) << 2)
        | (u8::from(p[1] > p[3]) << 3)
        | (u8::from(p[2] < 0.) << 4)
        | (u8::from(p[2] > p[3]) << 5)
}
#[cfg(test)]
fn outside_frustum(c: [[f64; 4]; 3]) -> bool {
    frustum_code(c[0]) & frustum_code(c[1]) & frustum_code(c[2]) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn homogeneous_clip_keeps_eye_plane_and_side_straddlers() {
        assert!(outside_frustum([
            [2., 0., 0.5, 1.],
            [3., 0., 0.5, 1.],
            [4., 1., 0.5, 1.]
        ]));
        assert!(!outside_frustum([
            [-2., 0., 0.5, 1.],
            [2., 0., 0.5, 1.],
            [0., 2., 0.5, 1.]
        ]));
        assert!(!outside_frustum([
            [0., 0., -0.5, -1.],
            [-0.5, 0., 0.5, 1.],
            [0.5, 0., 0.5, 1.]
        ]));
        assert!(outside_frustum([
            [0., 0., -1., 1.],
            [0.1, 0., -2., 1.],
            [0., 0.1, -3., 1.]
        ]));
    }
}

fn profile_compact_draped(
    result: Result<DrapedGeometry, String>,
    camera: &GlobeCamera,
    counter: Option<&std::sync::atomic::AtomicU64>,
) -> Result<
    (
        std::sync::Arc<GlobeMesh>,
        bool,
        Option<crate::globe_portrayal::CachedArea>,
        bool,
    ),
    String,
> {
    let result = result?;
    let timer = counter.map(|_| std::time::Instant::now());
    let result = compact_draped(Ok(result), camera);
    if let (Some(timer), Some(counter)) = (timer, counter) {
        counter.fetch_add(
            timer.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    result
}
fn profile_compact_view_colored(
    mesh: &GlobeMesh,
    camera: &GlobeCamera,
    color: Option<[f32; 4]>,
    counter: Option<&std::sync::atomic::AtomicU64>,
) -> Result<GlobeMesh, String> {
    let timer = counter.map(|_| std::time::Instant::now());
    let result = compact_view_colored(mesh, camera, color);
    if let (Some(timer), Some(counter)) = (timer, counter) {
        counter.fetch_add(
            timer.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    result
}
