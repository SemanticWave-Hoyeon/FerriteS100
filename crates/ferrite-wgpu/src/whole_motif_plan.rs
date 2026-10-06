//! Experimental complete-authored-domain adapter for independent whole motifs.
//! Domain selection precedes viewport/coverage clipping. Boundary subdivision
//! uses the existing Mercator-surface interpolation, but its sampled deviation
//! is NOT a certified combined SVG/AA/geodetic error bound. Keep this route
//! opt-in until the full portrayal conformance gate is established.
use crate::{globe_portrayal::{ecef, ring}, globe_scene::{GlobeMesh, GlobeVertex}, whole_motif::NaturalMotifResource};
use ferrite_kernel::{globe_camera::GlobeCamera, whole_symbol::{WholeSymbolArea, ShapeLimits, SymbolSite, SymbolDecision, WholeSymbolLimits, select_whole_symbols}};
use ferrite_render::{WorldPoint, PatternLattice};
use std::sync::Arc;
const MAX_COORDS: usize = 16384;
const MAX_SITES: usize = 4096;
const MAX_VERTICES: usize = 262144;
const MAX_TRIANGLE_TESTS: usize = 1048576;
const SAMPLED_DEVIATION_PX: f64 = 1. / 64.;

pub(crate) struct AuthoredDomain { area: WholeSymbolArea, bounds: [f64; 4] }
fn project(uv: [f64; 2], camera: &GlobeCamera) -> Result<[f64; 2], String> {
    camera.project_visible(ecef(uv)?).map_err(|e|e.to_string())?
        .map(|p|p.screen_px).ok_or_else(||"Whole motif authored boundary crosses unavailable horizon/clip domain".into())
}
fn edge(a:[f64;2], b:[f64;2], pa:[f64;2], pb:[f64;2], camera:&GlobeCamera, depth:u32, out:&mut Vec<[f64;2]>, total:&mut usize) -> Result<(),String> {
    let mut samples = [[0.;2];3];
    let mut deviation:f64=0.;
    for (i,t) in [0.25,0.5,0.75].into_iter().enumerate() {
        samples[i]=project(std::array::from_fn(|k|a[k]+t*(b[k]-a[k])),camera)?;
        let expected:[f64;2]=std::array::from_fn(|k|pa[k]+t*(pb[k]-pa[k]));
        deviation=deviation.max((samples[i][0]-expected[0]).hypot(samples[i][1]-expected[1]));
    }
    if deviation>SAMPLED_DEVIATION_PX {
        if depth>=16 {return Err("Whole motif boundary subdivision budget exceeded".into());}
        let mid=std::array::from_fn(|k|a[k]+0.5*(b[k]-a[k]));
        edge(a,mid,pa,samples[1],camera,depth+1,out,total)?;
        edge(mid,b,samples[1],pb,camera,depth+1,out,total)
    } else {
        if *total>=MAX_COORDS {return Err("Whole motif boundary coordinate budget exceeded".into());}
        out.push(pa); *total+=1; Ok(())
    }
}
pub(crate) fn authored_domain(exterior:&[WorldPoint], holes:&[Vec<WorldPoint>], camera:&GlobeCamera)->Result<AuthoredDomain,String> {
    let count=std::iter::once(exterior).chain(holes.iter().map(Vec::as_slice)).try_fold(0usize,|n,r|n.checked_add(r.len())).ok_or("Whole motif input coordinate overflow")?;
    if count>MAX_COORDS || holes.len()>=256 {return Err("Whole motif authored input budget exceeded".into());}
    let first=exterior.first().ok_or("Whole motif area has no authored boundary")?;
    let meridian=first.x;
    let mut projected=Vec::with_capacity(holes.len()+1);
    let mut total=0;
    let mut bounds=[f64::INFINITY,f64::INFINITY,f64::NEG_INFINITY,f64::NEG_INFINITY];
    for points in std::iter::once(exterior).chain(holes.iter().map(Vec::as_slice)) {
        let uv=ring(points,meridian)?;
        let mut out=Vec::new();
        for i in 0..uv.len() {
            let a=uv[i]; let b=uv[(i+1)%uv.len()];
            edge(a,b,project(a,camera)?,project(b,camera)?,camera,0,&mut out,&mut total)?;
        }
        for p in &out {bounds[0]=bounds[0].min(p[0]);bounds[1]=bounds[1].min(p[1]);bounds[2]=bounds[2].max(p[0]);bounds[3]=bounds[3].max(p[1]);}
        projected.push(out);
    }
    // Geo adds one closing coordinate per ring; admit those before validation.
    let shape=WholeSymbolArea::from_rings(&projected[0],&projected[1..],ShapeLimits{max_coordinates:MAX_COORDS,max_components:1,max_rings:256}).map_err(|e|e.to_string())?;
    Ok(AuthoredDomain{area:shape,bounds})
}
fn sites(bounds:[f64;4], support:[f64;4], bitmap:[f64;4], origin:[f64;2], extent:[f64;2], lattice:PatternLattice, ordinal:usize)->Result<Vec<SymbolSite>,String> {
    if !bounds.iter().chain(support.iter()).chain(bitmap.iter()).chain(origin.iter()).chain(extent.iter()).all(|v|v.is_finite()) || extent.iter().any(|v|*v<=0.) || bounds[0]>bounds[2] || bounds[1]>bounds[3] || support[0]>support[2] || support[1]>support[3] || bitmap[0]>bitmap[2] || bitmap[1]>bitmap[3] {return Err("Invalid whole motif site bounds".into());}
    // Necessary AABB conditions only; complete raw containment decides each site.
    // Full bitmap intersects viewport, retaining contributing offscreen anchors.
    let range=[(bounds[0]-support[0]).max(-bitmap[2]),(bounds[1]-support[1]).max(-bitmap[3]),(bounds[2]-support[2]).min(extent[0]-bitmap[0]),(bounds[3]-support[3]).min(extent[1]-bitmap[1])];
    if range[0]>range[2] || range[1]>range[3] {return Ok(Vec::new());}
    let mut lo=[f64::INFINITY;2]; let mut hi=[f64::NEG_INFINITY;2];
    for p in [[range[0],range[1]],[range[2],range[1]],[range[2],range[3]],[range[0],range[3]]] {
        let q=lattice.coordinates([p[0]-origin[0],p[1]-origin[1]]);
        for k in 0..2 {lo[k]=lo[k].min(q[k]);hi[k]=hi[k].max(q[k]);}
    }
    for k in 0..2 {lo[k]=lo[k].floor()-1.;hi[k]=hi[k].ceil()+1.;}
    if !lo.iter().chain(hi.iter()).all(|v|v.is_finite() && v.abs()<=2147483647.) {return Err("Whole motif lattice index precision budget exceeded".into());}
    let lo=lo.map(|v|v as i64); let hi=hi.map(|v|v as i64);
    let count=(hi[0]-lo[0]+1).checked_mul(hi[1]-lo[1]+1).filter(|n|*n>=0 && *n<=MAX_SITES as i64).ok_or("Whole motif site budget exceeded")?;
    let mut result=Vec::with_capacity(count as usize);
    for y in lo[1]..=hi[1] {for x in lo[0]..=hi[0] {
        let p=lattice.site([x as f64,y as f64]);
        let mut at=[0.;2];
        for k in 0..2 {
            at[k]=origin[k]+p[k]; let vb=at[k]-origin[k];
            let error=((origin[k]-(at[k]-vb))+(p[k]-vb)).abs();
            if !at[k].is_finite() || error>1e-8 {return Err("Whole motif site translation precision budget exceeded".into());}
        }
        result.push(SymbolSite{source_ordinal:ordinal,lattice_index:[x,y],origin:at});
    }}
    Ok(result)
}
pub(crate) fn select(domain:&AuthoredDomain, resource:&NaturalMotifResource, origin:[f64;2], extent:[f64;2], lattice:PatternLattice, ordinal:usize)->Result<Vec<SymbolDecision>,String> {
    let b=resource.bitmap_origin_px;
    let bitmap=[b[0],b[1],b[0]+f64::from(resource.width),b[1]+f64::from(resource.height)];
    let sites=sites(domain.bounds,resource.support.bounds_px,bitmap,origin,extent,lattice,ordinal)?;
    select_whole_symbols(&domain.area,&resource.support.support,&sites,WholeSymbolLimits{max_sites:MAX_SITES,max_cross_coordinate_pairs:10_000_000,max_support_coordinate_pairs:10_000_000,max_decision_bytes:256*1024,max_translation_error:1e-8}).map_err(|e|e.to_string())
}
#[derive(Clone,Copy)]
struct Point {ecef:[f64;3],clip:[f64;4]}
fn trim(input:&[Point], distance:impl Fn(Point)->f64)->Result<Vec<Point>,String> {
    let mut out=Vec::with_capacity(input.len()+1);
    let Some(mut a)=input.last().copied() else{return Ok(out)};
    let mut da=distance(a);
    for &b in input {
        let db=distance(b);
        if !da.is_finite() || !db.is_finite() {return Err("Whole motif surface crop overflow".into());}
        if (da>=0.)!=(db>=0.) {
            let t=da/(da-db);
            if !t.is_finite() || !(0. ..=1.).contains(&t) {return Err("Invalid whole motif surface intersection".into());}
            out.push(Point{ecef:std::array::from_fn(|k|a.ecef[k]+t*(b.ecef[k]-a.ecef[k])),clip:std::array::from_fn(|k|a.clip[k]+t*(b.clip[k]-a.clip[k]))});
        }
        if db>=0. {out.push(b)} a=b;da=db;
    }
    Ok(out)
}
/// Crops accepted sites to original draped triangles, preserving each surface
/// depth rather than replacing the geographic surface by an anchor billboard.
/// Complexity O(accepted_sites * source_triangles + emitted_vertices), explicitly
/// bounded before attempts. Entire batch errors, never returns a painted prefix.
pub(crate) fn surface_meshes(source:&GlobeMesh,camera:&GlobeCamera,resource:&NaturalMotifResource,decisions:&[SymbolDecision])->Result<Vec<(SymbolSite,Arc<GlobeMesh>)>,String> {
    let n=decisions.iter().filter(|d|d.completely_contained).count();
    if source.indices.len()%3!=0 || source.indices.len()/3>MAX_TRIANGLE_TESTS || n>MAX_SITES || n.checked_mul(source.indices.len()/3).is_none_or(|v|v>MAX_TRIANGLE_TESTS) {return Err("Whole motif surface attempt budget exceeded".into());}
    if source.vertices.len()>MAX_VERTICES {return Err("Whole motif source vertex budget exceeded".into());}
    let mut projected=Vec::with_capacity(source.vertices.len());
    for v in &source.vertices {let clip=camera.clip_ecef(v.ecef_m).map_err(|e|e.to_string())?;if clip[3]<=0. {return Err("Whole motif source crosses camera plane".into());}projected.push(Point{ecef:v.ecef_m,clip});}
    let [w,h]=camera.viewport(); let mut total=0;
    let mut result=Vec::with_capacity(n);
    for d in decisions.iter().filter(|d|d.completely_contained) {
        let p=d.site.origin; let b=resource.bitmap_origin_px;
        let left=2.*(p[0]+b[0])/w-1.; let right=2.*(p[0]+b[0]+f64::from(resource.width))/w-1.;
        let top=1.-2.*(p[1]+b[1])/h; let bottom=1.-2.*(p[1]+b[1]+f64::from(resource.height))/h;
        let mut mesh=GlobeMesh{vertices:Vec::new(),indices:Vec::new()};
        for triangle in source.indices.chunks_exact(3) {
            let mut polygon=Vec::with_capacity(7);
            for &i in triangle {polygon.push(*projected.get(i as usize).ok_or("Whole motif source index out of range")?);}
            polygon=trim(&polygon,|q|q.clip[0]-left*q.clip[3])?;
            polygon=trim(&polygon,|q|right*q.clip[3]-q.clip[0])?;
            polygon=trim(&polygon,|q|top*q.clip[3]-q.clip[1])?;
            polygon=trim(&polygon,|q|q.clip[1]-bottom*q.clip[3])?;
            for i in 1..polygon.len().saturating_sub(1) {
                let t=[polygon[0],polygon[i],polygon[i+1]];
                let screen=t.map(|q|[q.clip[0]/q.clip[3],q.clip[1]/q.clip[3]]);
                let cross=(screen[1][0]-screen[0][0])*(screen[2][1]-screen[0][1])-(screen[1][1]-screen[0][1])*(screen[2][0]-screen[0][0]);
                if !cross.is_finite() {return Err("Whole motif crop projected area overflow".into());} if cross==0. {continue;}
                total+=3; if total>MAX_VERTICES {return Err("Whole motif crop vertex budget exceeded".into());}
                let base=mesh.vertices.len() as u32;
                mesh.vertices.extend(t.map(|q|GlobeVertex{ecef_m:q.ecef,color:[1.;4]}));mesh.indices.extend([base,base+1,base+2]);
            }
        }
        if !mesh.indices.is_empty() {result.push((d.site,Arc::new(mesh)));}
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn signed_shear_and_offscreen_anchor_not_modulo_reduced() {
        let lattice=PatternLattice::from_mm((4.,-4.),(-4.,-8.),1.).unwrap();
        let all=sites([-100.,-100.,100.,100.],[-2.,-2.,2.,2.],[-10.,-10.,10.,10.],[0.,0.],[30.,30.],lattice,37).unwrap();
        assert!(all.iter().any(|s|s.origin[0]<0. && s.origin[0]+10.>0.));
        assert!(all.iter().any(|s|s.lattice_index[0]<0 || s.lattice_index[1]<0));
        assert!(all.windows(2).all(|w|(w[0].lattice_index[1],w[0].lattice_index[0])<(w[1].lattice_index[1],w[1].lattice_index[0])));
        for s in all {assert_eq!(s.source_ordinal,37);assert_eq!(s.origin,lattice.site(s.lattice_index.map(|v|v as f64)));}
    }
    #[test] fn site_budget_rejects_batch_and_empty_range_is_empty() {
        let l=PatternLattice::from_mm((1.,0.),(0.,1.),1.).unwrap();
        assert!(sites([0.,0.,1e6,1e6],[0.,0.,1.,1.],[0.,0.,1.,1.],[0.,0.],[1e6,1e6],l,0).is_err());
        assert!(sites([0.,0.,1.,1.],[0.,0.,2.,2.],[0.,0.,2.,2.],[0.,0.],[100.,100.],l,0).unwrap().is_empty());
    }
    #[test] fn crop_intersections_preserve_clip_and_ecef_affine_relation() {
        let a=Point{ecef:[0.,0.,0.],clip:[-2.,0.,0.5,1.]}; let b=Point{ecef:[4.,0.,0.],clip:[2.,0.,0.5,1.]};
        let output=trim(&[a,b],|q|q.clip[0]).unwrap();
        assert_eq!(output.len(),3);
        assert!(output.iter().filter(|q|q.clip[0]==0.).all(|q|q.ecef==[2.,0.,0.]));
    }
}
