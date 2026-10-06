//! Product-neutral raster draping on the WGS84 portrayal surface.
//! Pixels are already resolved PC RGBA. Texture sampling remains nearest-node;
//! these meshes are display geometry at ellipsoid height zero, NEVER bathymetric
//! elevations or a chart-datum to ellipsoid conversion.
use crate::{globe_portrayal::{drape_area_geometry, DrapingLimits, DrapingStats},globe_scene::GlobeMesh};
use ferrite_kernel::{geocentric::from_ecef,globe_camera::GlobeCamera};
use ferrite_render::{GeoBounds,RasterGrid,WorldPoint};

pub(crate) struct GlobeRasterLayer<'a> {
    pub bounds:GeoBounds, pub grid:RasterGrid, pub tile_size:[u32;2],
    pub continuous:bool,
    pub draw_order:ferrite_render::RasterDrawOrder, pub viewing_groups:&'a [u32],
    pub bind_group:&'a wgpu::BindGroup,
}

pub(crate) fn tile_bounds(grid:RasterGrid,tile:[u32;2])->Result<GeoBounds,String> {
    let b=grid.bounds;
    if grid.width==0 || grid.height==0 || grid.width>1048576 || grid.height>1048576 || tile.contains(&0) || grid.column.checked_add(tile[0]).is_none_or(|v|v>grid.width) || grid.row.checked_add(tile[1]).is_none_or(|v|v>grid.height)
        || ![b.min_x,b.max_x,b.min_y,b.max_y].iter().all(|v|v.is_finite()) || b.min_x>=b.max_x || b.min_y>=b.max_y || b.width()>=180. || b.min_y<=-89.5 || b.max_y>=89.5 {
        return Err("Unsupported globe raster source lattice; regular nonpolar geographic grid required".into());
    }
    let lon=|x:u32|b.min_x+b.width()*(x as f64/grid.width as f64);
    let lat=|y:u32|b.max_y-b.height()*(y as f64/grid.height as f64);
    Ok(GeoBounds::new(lon(grid.column),lat(grid.row+tile[1]),lon(grid.column+tile[0]),lat(grid.row)))
}
/// Original source-cell coordinates plus integer tile origin. Every partition
/// uses the SAME whole-grid surface and perspective interpolant. The fragment
/// shader chooses one global cell before testing ownership and textureLoad.
fn uv(grid:RasterGrid,_tile:[u32;2],longitude:f64,latitude:f64)->Result<[f32;4],String> {
    let b=grid.bounds;
    let global=[(longitude-b.min_x)/b.width()*grid.width as f64,(b.max_y-latitude)/b.height()*grid.height as f64];
    if !global.iter().enumerate().all(|(k,v)|v.is_finite() && *v>=-1e-6 && *v<=[grid.width,grid.height][k] as f64+1e-6) {return Err("Globe raster vertex outside shared source grid".into());}
    let rounded=global.map(|v|v.max(0.) as f32);
    if (0..2).any(|k|(f64::from(rounded[k])-global[k]).abs()>0.125) {return Err("Globe raster source-coordinate cast exceeds source-cell budget".into());}
    Ok([rounded[0],rounded[1],grid.column as f32,grid.row as f32])
}
pub(crate) fn mesh(bounds:GeoBounds,grid:RasterGrid,tile:[u32;2],camera:&GlobeCamera)->Result<(GlobeMesh,DrapingStats),String> {
    let exact=tile_bounds(grid,tile)?;
    if [bounds.min_x-exact.min_x,bounds.max_x-exact.max_x,bounds.min_y-exact.min_y,bounds.max_y-exact.max_y].iter().any(|v|!v.is_finite() || v.abs()>1e-9) {return Err("Globe raster tile bounds disagree with shared lattice".into());}
    let shared=grid.bounds;
    let exterior=[WorldPoint::new(shared.min_x,shared.min_y),WorldPoint::new(shared.max_x,shared.min_y),WorldPoint::new(shared.max_x,shared.max_y),WorldPoint::new(shared.min_x,shared.max_y)];
    let (mut mesh,stats)=drape_area_geometry(&exterior,&[],camera,DrapingLimits{screen_error_px:1./64.,chord_error_m:0.5,max_vertices:65536,frustum_culling:true})?;
    let meridian=(grid.bounds.min_x+grid.bounds.max_x)/2.;
    for vertex in &mut mesh.vertices {
        let geo=from_ecef(vertex.ecef_m).map_err(|e|e.to_string())?.surface;
        let longitude=geo.longitude_near(meridian).map_err(|e|e.to_string())?;
        vertex.color=uv(grid,tile,longitude,geo.latitude())?;
    }
    Ok((mesh,stats))
}
#[cfg(test)] mod tests {
    use super::*;
    fn grid()->RasterGrid {RasterGrid{bounds:GeoBounds::new(179.,40.,181.,42.),width:100,height:80,column:0,row:0}}
    #[test] fn shared_lattice_tile_edges_uv_and_dateline_lift() {
        let mut a=grid();a.column=40;a.row=20;
        let b=tile_bounds(a,[30,40]).unwrap();assert!((b.min_x-179.8).abs()<1e-12);assert!((b.max_x-180.4).abs()<1e-12);assert_eq!(b.max_y,41.5);assert_eq!(b.min_y,40.5);
        let start=uv(a,[30,40],b.min_x,b.max_y).unwrap();assert!((start[0]-40.).abs()<1e-5);assert_eq!(&start[1..],&[20.,40.,20.]);
        let p=uv(a,[30,40],b.max_x,b.min_y).unwrap();assert!((p[0]-70.).abs()<1e-5);assert_eq!(p[1],60.);
        let mut c=a;c.column+=30;
        assert_eq!(tile_bounds(c,[30,40]).unwrap().min_x,b.max_x);
    }
    #[test] fn invalid_lattice_rejected_before_draping() {
        let mut g=grid();g.column=99;assert!(tile_bounds(g,[2,1]).is_err());g.column=0;g.bounds.max_y=90.;assert!(tile_bounds(g,[1,1]).is_err());g=grid();g.width=0;assert!(tile_bounds(g,[1,1]).is_err());
    }
    #[test] fn actual_wgs84_mesh_maps_shared_north_south_texture_cells() {
        use ferrite_kernel::{geodesy::GeographicPosition,globe_navigation::GlobePose};
        let g=RasterGrid{bounds:GeoBounds::new(-2.13,48.62,-2.08,48.67),width:100,height:100,column:0,row:0};
        let c=GlobePose{focus:GeographicPosition::new(48.645,-2.105).unwrap(),range_m:10000.,heading_deg:25.,tilt_deg:35.}.camera([640.,480.]).unwrap();
        let (m,_)=mesh(g.bounds,g,[100,100],&c).unwrap();assert!(!m.indices.is_empty());
        for v in m.vertices {let p=from_ecef(v.ecef_m).unwrap();assert!(p.ellipsoidal_height_m.abs()<1e-5);assert!(v.color[0]>=0. && v.color[0]<=100. && v.color[1]>=0. && v.color[1]<=100.);}
    }
    #[test] fn partition_preserves_entire_original_grid_surface_and_interpolants() {
        use ferrite_kernel::{geodesy::GeographicPosition,globe_navigation::GlobePose};
        let g=RasterGrid{bounds:GeoBounds::new(-2.13,48.62,-2.08,48.67),width:8,height:8,column:0,row:0};
        for tilt in [0.,35.] {for heading in [0.,25.] {
            let camera=GlobePose{focus:GeographicPosition::new(48.645,-2.105).unwrap(),range_m:12000.,heading_deg:heading,tilt_deg:tilt}.camera([640.,480.]).unwrap();
            let (full,_)=mesh(g.bounds,g,[8,8],&camera).unwrap();
            for row in [0,4] {for column in [0,4] {
                let mut tile=g;tile.column=column;tile.row=row;
                let (part,_)=mesh(tile_bounds(tile,[4,4]).unwrap(),tile,[4,4],&camera).unwrap();
                assert_eq!(full.indices,part.indices);assert_eq!(full.vertices.len(),part.vertices.len());
                for (a,b) in full.vertices.iter().zip(&part.vertices) {
                    assert_eq!(a.ecef_m,b.ecef_m);assert_eq!(&a.color[..2],&b.color[..2]);
                    assert_eq!(&b.color[2..],&[column as f32,row as f32]);
                }
            }}
        }}
    }

}
