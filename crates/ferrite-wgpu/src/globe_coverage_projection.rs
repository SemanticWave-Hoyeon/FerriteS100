//! Bridge geographic coverage geometry to the same conforming WGS84 draper
//! used by chart areas. Product identity and S-98 selection stay in the adapter.
use crate::globe_portrayal::{drape_area, DrapingLimits, DrapingStats};
use ferrite_kernel::{
    coverage_selection::Region,
    globe_camera::GlobeCamera,
    globe_coverage_projection::{project_coverage_triangles, CoverageProjectionLimits},
};
use ferrite_render::{AreaInstruction, Color};

/// `area` contains geographic exterior and interior rings, in longitude/latitude
/// degrees. Coverage must never be supplied as Portrayal/Local device geometry.
/// An entirely hidden footprint returns an empty region. Geometry/projection
/// errors remain errors; the caller must not substitute a flat projection.
/// Only exterior/interior geometry is read; portrayal fill/filter metadata does
/// not control the coverage footprint. Source copies are bounded before cloning.
/// Region accuracy inherits the existing WGS84 draper's explicit bounds.
pub fn project_coverage_area(
    area: &AreaInstruction,
    camera: &GlobeCamera,
    draping_limits: DrapingLimits,
    projection_limits: CoverageProjectionLimits,
) -> Result<(Region, DrapingStats), String> {
    let source_count = area
        .interiors
        .iter()
        .try_fold(area.exterior.len(), |n, ring| n.checked_add(ring.len()))
        .ok_or("Coverage source vertex count overflow")?;
    if source_count > draping_limits.max_vertices {
        return Err("Coverage source vertex budget exceeded".into());
    }
    let attribution=std::env::var("FERRITE_GLOBE_COVERAGE_ATTRIBUTION").as_deref()==Ok("1");
    let copy_start=attribution.then(std::time::Instant::now);
    // A coverage footprint has geometry regardless of its portrayal fill or
    // viewing settings. Do not let a default None fill silently erase it.
    let mut footprint = AreaInstruction::new(area.exterior.clone()).with_solid_fill(Color::WHITE);
    footprint.interiors = area.interiors.clone();
    let copy_ms=copy_start.map_or(0.,|s|s.elapsed().as_secs_f64()*1000.);
    let drape_start=attribution.then(std::time::Instant::now);
    let (mesh, stats) = drape_area(&footprint, camera, draping_limits)?;
    let drape_ms=drape_start.map_or(0.,|s|s.elapsed().as_secs_f64()*1000.);
    mesh.validate()?;
    let triangles = mesh
        .indices
        .chunks_exact(3)
        .map(|ids| std::array::from_fn(|i| mesh.vertices[ids[i] as usize].ecef_m));
    let region_start=attribution.then(std::time::Instant::now);
    let region = project_coverage_triangles(camera, triangles, projection_limits)
        .map_err(|e| e.to_string())?;
    if attribution {tracing::info!("COVERAGE_MESH_STAGE {}",serde_json::json!({"cache":false,"copy_ms":copy_ms,"drape_ms":drape_ms,"project_union_ms":region_start.map_or(0.,|s|s.elapsed().as_secs_f64()*1000.),"source_vertices":source_count,"vertices":mesh.vertices.len(),"triangles":mesh.indices.len()/3}));}
    Ok((region, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_kernel::coverage_raster::rasterize;
    use ferrite_kernel::geodesy::GeographicPosition;
    use ferrite_render::WorldPoint;
    #[test]
    fn actual_draper_preserves_dateline_hole_under_tilt() {
        let c = GlobeCamera::orbit(
            GeographicPosition::new(48., 179.99).unwrap(),
            30000.,
            35.,
            50.,
            [640., 480.],
            45.,
            3.,
            1e9,
        )
        .unwrap();
        let ring = |lo: f64, hi: f64| {
            vec![
                WorldPoint::new(179.99 + lo, 48. + lo),
                WorldPoint::new(179.99 + hi, 48. + lo),
                WorldPoint::new(179.99 + hi, 48. + hi),
                WorldPoint::new(179.99 + lo, 48. + hi),
                WorldPoint::new(179.99 + lo, 48. + lo),
            ]
        };
        let mut area = AreaInstruction::new(ring(-0.05, 0.05));
        area.interiors.push(ring(-0.01, 0.01));
        let (region, stats) = project_coverage_area(
            &area,
            &c,
            DrapingLimits::default(),
            CoverageProjectionLimits::default(),
        )
        .unwrap();
        assert!(stats.final_triangles > 0);
        assert!(region.polygons().iter().any(|p| !p.interiors().is_empty()));
        let mask = rasterize(&region, [640, 480], 640 * 480).unwrap();
        for (lat, lon, expected) in [(48_f64, 179.99_f64, false), (48.025, 180.015, true)] {
            let point = c
                .project_visible(
                    GeographicPosition::new(lat, (lon + 180.).rem_euclid(360.) - 180.)
                        .unwrap()
                        .to_ecef(0.)
                        .unwrap(),
                )
                .unwrap()
                .unwrap()
                .screen_px;
            assert_eq!(
                mask.contains_pixel(point[0].floor() as u32, point[1].floor() as u32),
                expected
            );
        }
    }
    #[test]
    fn fully_rear_coverage_is_empty_without_flat_fallback() {
        let c = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            1000000.,
            0.,
            0.,
            [640., 480.],
            45.,
            3.,
            1e9,
        )
        .unwrap();
        let area = AreaInstruction::new(vec![
            WorldPoint::new(179., -0.1),
            WorldPoint::new(179.1, -0.1),
            WorldPoint::new(179.1, 0.1),
            WorldPoint::new(179., 0.1),
            WorldPoint::new(179., -0.1),
        ]);
        let (region, _) = project_coverage_area(
            &area,
            &c,
            DrapingLimits::default(),
            CoverageProjectionLimits::default(),
        )
        .unwrap();
        assert!(region.is_empty());
    }
}

/// Provider-owned source epoch. No Region/mask is retained across cameras.
/// Retained footprint+certificate capacity is bounded; optional cold capture is
/// governed by the original draper vertex limit and rejected before admission.
#[derive(Default)]
pub struct CoverageGeometryCache {
    enabled: bool,
    midpoint_reuse:bool,
    midpoint_ecef_reused:usize,
    midpoint_vertices_created:usize,
    entries: Vec<CoverageGeometryEntry>,
    bytes: usize,
    hits: usize,
    cold: usize,
    rejected: usize,
}
struct CoverageGeometryEntry {
    id: (usize, i64, usize),
    footprint: AreaInstruction,
    limits: DrapingLimits,
    mesh: crate::globe_portrayal::CachedArea,
    bytes: usize,
}
impl CoverageGeometryCache {
    const LIMIT: usize = 16 * 1024 * 1024;
    const ENTRIES: usize = 4096;
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled=enabled; self.entries=Vec::new(); self.bytes=0;
        self.hits=0;self.cold=0;self.rejected=0;self.midpoint_ecef_reused=0;self.midpoint_vertices_created=0;
    }
    pub fn set_midpoint_reuse(&mut self,enabled:bool) {
        if self.midpoint_reuse!=enabled {self.set_enabled(self.enabled);self.midpoint_reuse=enabled;}
    }
    fn same_limits(a:DrapingLimits,b:DrapingLimits)->bool {
        a.screen_error_px.to_bits()==b.screen_error_px.to_bits()
            && a.chord_error_m.to_bits()==b.chord_error_m.to_bits()
            && a.max_vertices==b.max_vertices && a.frustum_culling==b.frustum_culling
    }
    fn same_source(area:&AreaInstruction, exterior:&[[f64;2]], holes:&[Vec<[f64;2]>])->bool {
        let same=|a:&[ferrite_render::WorldPoint],b:&[[f64;2]]| a.len()==b.len()&&a.iter().zip(b).all(|(a,b)|a.x.to_bits()==b[0].to_bits()&&a.y.to_bits()==b[1].to_bits());
        same(&area.exterior,exterior)&&area.interiors.len()==holes.len()
            &&area.interiors.iter().zip(holes).all(|(a,b)|same(a,b))
    }
    fn footprint(exterior:&[[f64;2]],holes:&[Vec<[f64;2]>])->AreaInstruction {
        let points=|p:&[[f64;2]]|p.iter().map(|p|ferrite_render::WorldPoint::new(p[0],p[1])).collect();
        let mut a=AreaInstruction::new(points(exterior)).with_solid_fill(Color::WHITE);
        a.interiors=holes.iter().map(|p|points(p)).collect();a
    }
    fn source_footprint_bound(points:usize,holes:usize)->Option<usize> {
        points.checked_mul(std::mem::size_of::<ferrite_render::WorldPoint>())?
            .checked_add(holes.checked_mul(std::mem::size_of::<Vec<ferrite_render::WorldPoint>>())?)?
            .checked_add(std::mem::size_of::<CoverageGeometryEntry>())
    }
    fn admissible(bytes:usize)->bool {bytes.checked_add(Self::LIMIT/2).is_some_and(|n|n<=Self::LIMIT)}
    fn footprint_bytes(area:&AreaInstruction)->usize {
        let p=std::mem::size_of::<ferrite_render::WorldPoint>();
        std::mem::size_of::<CoverageGeometryEntry>()
            .saturating_add(area.exterior.capacity().saturating_mul(p))
            .saturating_add(area.interiors.capacity().saturating_mul(std::mem::size_of::<Vec<ferrite_render::WorldPoint>>()))
            .saturating_add(area.interiors.iter().map(|r|r.capacity().saturating_mul(p)).fold(0usize,usize::saturating_add))

    }
    fn retained_bytes(area:&AreaInstruction, mesh:&crate::globe_portrayal::CachedArea)->usize {Self::footprint_bytes(area).saturating_add(mesh.bytes())}
    pub fn diagnostics(&self)->serde_json::Value {
        serde_json::json!({"enabled":self.enabled,"midpoint_reuse":self.midpoint_reuse,"midpoint_ecef_reused":self.midpoint_ecef_reused,"midpoint_vertices_created":self.midpoint_vertices_created,"hits":self.hits,"cold":self.cold,"rejected":self.rejected,"retained_bytes":self.bytes.saturating_add(self.entries.capacity()*std::mem::size_of::<CoverageGeometryEntry>()),"budget_bytes":Self::LIMIT,"entries":self.entries.len(),"scope":"retained capacities; original cold draper transient allocations separate"})
    }
    pub fn project_rings(&mut self,id:(usize,i64,usize),exterior:&[[f64;2]],holes:&[Vec<[f64;2]>],camera:&GlobeCamera,limits:DrapingLimits,projection_limits:CoverageProjectionLimits)->Result<Region,String> {
        let n=holes.iter().try_fold(exterior.len(),|n,r|n.checked_add(r.len())).ok_or("Coverage source vertex count overflow")?;
        if n>limits.max_vertices{return Err("Coverage source vertex budget exceeded".into());}
        if !self.enabled {
            let a=Self::footprint(exterior,holes);
            return project_coverage_area(&a,camera,limits,projection_limits).map(|(r,_)|r);
        }
        let hit=self.entries.iter().position(|e|e.id==id&&Self::same_limits(e.limits,limits)&&Self::same_source(&e.footprint,exterior,holes));
        if let Some(i)=hit {
            if let Some(mesh)=self.entries[i].mesh.reusable_mesh(camera,Color::WHITE.to_array())? {
                mesh.validate()?;self.hits+=1;
                let triangles=mesh.indices.chunks_exact(3).map(|ids|std::array::from_fn(|i|mesh.vertices[ids[i] as usize].ecef_m));
                return project_coverage_triangles(camera,triangles,projection_limits).map_err(|e|e.to_string());
            }
        }
        // Source/limits replacement cannot leave another stale slot for this id.
        if let Some(i)=self.entries.iter().position(|e|e.id==id) {let e=self.entries.remove(i);self.bytes-=e.bytes;}
        self.cold+=1;
        let planned=Self::source_footprint_bound(n,holes.len());
        while !self.entries.is_empty() && !planned.and_then(|p|p.checked_add(self.bytes)).and_then(|p|p.checked_add(self.entries.capacity().checked_mul(std::mem::size_of::<CoverageGeometryEntry>())?)).is_some_and(|p|p<=Self::LIMIT) {
            let old=self.entries.remove(0);self.bytes-=old.bytes;
        }
        if !planned.is_some_and(|p|p<=Self::LIMIT) {
            // Oversize source remains baseline cold work, never a retained or
            // pending optional record. Release slot storage before constructing it.
            self.entries=Vec::new();self.bytes=0;self.rejected+=1;
            let footprint=Self::footprint(exterior,holes);
            return project_coverage_area(&footprint,camera,limits,projection_limits).map(|(r,_)|r);
        }
        if self.entries.is_empty() && planned.and_then(|p|p.checked_add(self.entries.capacity().checked_mul(std::mem::size_of::<CoverageGeometryEntry>())?)).is_none_or(|p|p>Self::LIMIT) {self.entries=Vec::new();}
        let footprint=Self::footprint(exterior,holes);
        // Reserve slot capacity and optional capture budget before allocating
        // a duplicate mesh. Any refusal keeps the original cold result.
        if self.entries.len()>=Self::ENTRIES {let old=self.entries.remove(0);self.bytes-=old.bytes;}
        let slot_size=std::mem::size_of::<CoverageGeometryEntry>();
        let growth=self.entries.len()==self.entries.capacity();
        let desired=self.entries.len().checked_add(1).unwrap_or(usize::MAX);
        let reserve_peak=desired.checked_add(self.entries.capacity()).and_then(|n|n.checked_mul(slot_size)).and_then(|n|n.checked_add(self.bytes)).and_then(|n|n.checked_add(Self::footprint_bytes(&footprint)));
        let can_allocate_slot=!growth || reserve_peak.is_some_and(|n|n<=Self::LIMIT)&&self.entries.try_reserve_exact(1).is_ok();
        let retained=self.bytes.saturating_add(self.entries.capacity().saturating_mul(slot_size));
        let capture_limit=Self::LIMIT.checked_sub(retained).and_then(|n|n.checked_sub(Self::footprint_bytes(&footprint)));
        let (mesh,stats,capture)=if can_allocate_slot {
            crate::globe_portrayal::drape_area_cached_bounded_midpoints(&footprint,camera,limits,capture_limit.unwrap_or(0),self.midpoint_reuse)?
        } else {let (m,s)=drape_area(&footprint,camera,limits)?;(m,s,None)};
        self.midpoint_ecef_reused+=stats.midpoint_ecef_reused;self.midpoint_vertices_created+=stats.refinements;
        mesh.validate()?;
        let triangles=mesh.indices.chunks_exact(3).map(|ids|std::array::from_fn(|i|mesh.vertices[ids[i] as usize].ecef_m));
        let result=project_coverage_triangles(camera,triangles,projection_limits).map_err(|e|e.to_string())?;
        if let Some(capture)=capture {
            let bytes=Self::retained_bytes(&footprint,&capture);
            if Self::admissible(bytes) {
                while self.entries.len()>=Self::ENTRIES||self.bytes.saturating_add(bytes)>Self::LIMIT/2 {
                    if self.entries.is_empty(){break;}
                    let old=self.entries.remove(0);self.bytes-=old.bytes;
                }
                self.bytes+=bytes;
                self.entries.push(CoverageGeometryEntry{id,footprint,limits,mesh:capture,bytes});
                // Include capacity allocated for the slots; reserve half the
                // budget for slot slack, reject if total could exceed the cap.
                if self.bytes.saturating_add(self.entries.capacity()*std::mem::size_of::<CoverageGeometryEntry>())>Self::LIMIT {self.entries=Vec::new();self.bytes=0;self.rejected+=1;}
            } else {self.rejected+=1;}
        }
        Ok(result)
    }
}

#[cfg(test)]
mod coverage_cache_tests {
    use super::*;
    fn camera(range:f64,heading:f64,extent:[f64;2])->GlobeCamera {
        GlobeCamera::orbit(ferrite_kernel::geodesy::GeographicPosition::new(48.,0.).unwrap(),range,heading,35.,extent,45.,3.,1e9).unwrap()
    }
    fn rings()->(Vec<[f64;2]>,Vec<Vec<[f64;2]>>) {
        (vec![[-0.01,47.99],[0.01,47.99],[0.01,48.01],[-0.01,48.01],[-0.01,47.99]],vec![vec![[-0.002,47.998],[-0.002,48.002],[0.002,48.002],[0.002,47.998],[-0.002,47.998]]])
    }
    fn bits(r:&Region)->Vec<u64> {
        let mut b=Vec::new();for p in r.polygons(){b.push(p.exterior().0.len() as u64);for c in &p.exterior().0{b.extend([c.x.to_bits(),c.y.to_bits()]);}b.push(p.interiors().len() as u64);for ring in p.interiors(){b.push(ring.0.len() as u64);for c in &ring.0{b.extend([c.x.to_bits(),c.y.to_bits()]);}}}b
    }
    #[test]
    fn coverage_cached_regions_preserve_holes_camera_dpi_and_cold_bits() {
        let (e,h)=rings();let a=CoverageGeometryCache::footprint(&e,&h);let mut cache=CoverageGeometryCache::default();cache.set_enabled(true);
        for (range,heading,extent) in [(50000.,0.,[640.,480.]),(50000.,0.,[640.,480.]),(50000.,0.01,[640.,480.]),(25000.,60.,[1280.,960.]),(50000.,0.,[640.,480.])] {
            let c=camera(range,heading,extent);let expected=project_coverage_area(&a,&c,DrapingLimits::default(),CoverageProjectionLimits::default()).unwrap().0;
            let got=cache.project_rings((1,2,0),&e,&h,&c,DrapingLimits::default(),CoverageProjectionLimits::default()).unwrap();assert_eq!(bits(&got),bits(&expected));assert!(cache.bytes<=CoverageGeometryCache::LIMIT);
        }
    }
    #[test]
    fn coverage_cache_source_limits_removal_and_errors_are_original() {
        let (mut e,h)=rings();let c=camera(50000.,0.,[640.,480.]);let mut cache=CoverageGeometryCache::default();cache.set_enabled(true);
        cache.project_rings((1,2,0),&e,&h,&c,DrapingLimits::default(),CoverageProjectionLimits::default()).unwrap();
        e[0][0]=-0.02;
        let a=CoverageGeometryCache::footprint(&e,&h);let expected=project_coverage_area(&a,&c,DrapingLimits::default(),CoverageProjectionLimits::default()).unwrap().0;
        assert_eq!(bits(&cache.project_rings((1,2,0),&e,&h,&c,DrapingLimits::default(),CoverageProjectionLimits::default()).unwrap()),bits(&expected));
        let mut limits=DrapingLimits::default();limits.screen_error_px=f64::NAN;
        assert_eq!(cache.project_rings((1,2,0),&e,&h,&c,limits,CoverageProjectionLimits::default()).unwrap_err(),project_coverage_area(&a,&c,limits,CoverageProjectionLimits::default()).unwrap_err());
        cache.set_enabled(false);assert_eq!(cache.bytes,0);assert!(cache.entries.is_empty());
        limits=DrapingLimits::default();limits.max_vertices=1;
        assert_eq!(cache.project_rings((1,2,0),&e,&h,&c,limits,CoverageProjectionLimits::default()).unwrap_err(),project_coverage_area(&a,&c,limits,CoverageProjectionLimits::default()).unwrap_err());
    }
    #[test]
    fn coverage_source_identity_is_exact_not_counts_or_float_equality() {
        let (e,h)=rings();let mut a=CoverageGeometryCache::footprint(&e,&h);assert!(CoverageGeometryCache::same_source(&a,&e,&h));
        a.exterior[0].x=f64::from_bits(a.exterior[0].x.to_bits()+1);assert!(!CoverageGeometryCache::same_source(&a,&e,&h));
        let mut p=CoverageGeometryCache::footprint(&[[0.,0.]],&[]);assert!(CoverageGeometryCache::same_source(&p,&[[0.,0.]],&[]));p.exterior[0].x=-0.;assert!(!CoverageGeometryCache::same_source(&p,&[[0.,0.]],&[]));
    }
}

#[cfg(test)]
mod coverage_mesh_hit_tests {
    use super::*;
    #[test]
    fn admission_rejects_oversize_and_overflow_without_display_omission() {
        assert!(CoverageGeometryCache::admissible(CoverageGeometryCache::LIMIT/2));
        assert!(!CoverageGeometryCache::admissible(CoverageGeometryCache::LIMIT/2+1));
        assert!(!CoverageGeometryCache::admissible(usize::MAX));
        assert_eq!(CoverageGeometryCache::source_footprint_bound(usize::MAX,0),None);
        assert_eq!(CoverageGeometryCache::source_footprint_bound(0,usize::MAX),None);
        assert!(CoverageGeometryCache::source_footprint_bound(5,1).unwrap()<CoverageGeometryCache::LIMIT);
        let e=vec![[0.,0.];5];let h=vec![vec![[0.,0.];4]];let a=CoverageGeometryCache::footprint(&e,&h);
        assert!(CoverageGeometryCache::footprint_bytes(&a)<=CoverageGeometryCache::source_footprint_bound(9,1).unwrap());
    }
    #[test]
    fn small_same_view_hit_preserves_every_mesh_bit() {
        let c=GlobeCamera::orbit(ferrite_kernel::geodesy::GeographicPosition::new(48.,0.).unwrap(),50000.,0.,35.,[640.,480.],45.,3.,1e9).unwrap();
        let e=vec![[-0.00001,47.99999],[0.00001,47.99999],[0.00001,48.00001],[-0.00001,48.00001],[-0.00001,47.99999]];
        let mut cache=CoverageGeometryCache::default();cache.set_enabled(true);let limits=DrapingLimits::default();
        cache.project_rings((0,1,0),&e,&[],&c,limits,CoverageProjectionLimits::default()).unwrap();
        cache.project_rings((0,1,0),&e,&[],&c,limits,CoverageProjectionLimits::default()).unwrap();
        assert_eq!(cache.hits,1);
        let a=CoverageGeometryCache::footprint(&e,&[]);let expected=drape_area(&a,&c,limits).unwrap().0;
        let got=cache.entries[0].mesh.reusable_mesh(&c,Color::WHITE.to_array()).unwrap().unwrap();
        assert_eq!(got.indices,expected.indices);assert_eq!(got.vertices.len(),expected.vertices.len());
        for (a,b) in got.vertices.iter().zip(expected.vertices.iter()){assert_eq!(a.ecef_m.map(f64::to_bits),b.ecef_m.map(f64::to_bits));assert_eq!(a.color.map(f32::to_bits),b.color.map(f32::to_bits));}
    }
}
