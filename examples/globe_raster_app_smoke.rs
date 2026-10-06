//! Hidden production Pane raster source-cell oracle; no S102 conformance claim.
use ferrite_kernel::{geodesy::GeographicPosition,globe_navigation::GlobePose,CompositionStage};
use ferrite_render::*;
use ferrite_wgpu::{WgpuRenderer,SymbolCache};
use std::{path::PathBuf,sync::Arc};
use winit::{application::ApplicationHandler,event::WindowEvent,event_loop::{ActiveEventLoop,EventLoop},window::{Window,WindowId},dpi::PhysicalSize};
struct App{out:PathBuf}
fn bounds()->GeoBounds {GeoBounds::new(-2.13,48.62,-2.08,48.67)}
fn pixel(x:u32,y:u32)->[u8;4] {match (x+y)%4 {0=>[255,0,0,255],1=>[0,255,0,255],2=>[0,0,255,255],_=>[0,0,0,0]}}
fn layer(column:u32,row:u32,size:u32,below:bool)->RasterLayer {
    let b=bounds();let dx=b.width()/8.;let dy=b.height()/8.;
    RasterLayer{id:format!("grid-{column}-{row}"),width:size,height:size,rgba:(row..row+size).flat_map(|y|(column..column+size).flat_map(move|x|pixel(x,y))).collect(),bounds:GeoBounds::new(b.min_x+column as f64*dx,b.max_y-(row+size) as f64*dy,b.min_x+(column+size) as f64*dx,b.max_y-row as f64*dy),grid:Some(RasterGrid{bounds:b,width:8,height:8,column,row}),viewing_groups:vec![90020],draw_order:RasterDrawOrder{stage:if below{CompositionStage::Chart}else{CompositionStage::Overlay},display_plane:DisplayPlane::UnderRadar,priority:-100}}
}
impl ApplicationHandler for App {
 fn resumed(&mut self,event:&ActiveEventLoop) {
  assert_eq!(std::env::var("FERRITE_BACKGROUND_TEST").as_deref(),Ok("1"));assert_eq!(std::env::var("FERRITE_EXPERIMENTAL_GLOBE_RASTERS").as_deref(),Ok("1"));
  std::fs::create_dir_all(&self.out).unwrap();
  let window=Arc::new(event.create_window(Window::default_attributes().with_title("Background raster regression").with_visible(false).with_active(false).with_inner_size(PhysicalSize::new(900,700))).unwrap());
  assert!(!window.is_visible().unwrap_or(true));assert!(!window.has_focus());
  let mut renderer=pollster::block_on(WgpuRenderer::new(window.clone())).unwrap();renderer.ui_state.globe_preview=true;
  let pc=self.out.join("empty-pc");std::fs::create_dir_all(&pc).unwrap();let mut cache=SymbolCache::new(pc);let profile=ferrite_portrayal_catalog::ColorProfile::default();
  let mut context=RenderContext::new(Viewport::with_origin(60.,50.,640.,480.));context.set_bounds(GeoBounds::new(-2.18,48.57,-2.03,48.72));
  let b=bounds();context.add_instruction(DrawingInstruction::Area(AreaInstruction::new(vec![WorldPoint::new(b.min_x-0.01,b.min_y-0.01),WorldPoint::new(b.max_x+0.01,b.min_y-0.01),WorldPoint::new(b.max_x+0.01,b.max_y+0.01),WorldPoint::new(b.min_x-0.01,b.max_y+0.01)]).with_solid_fill(Color::rgb(1.,1.,0.)).with_priority(100).with_cell_index(3).with_feature_id(42)));
  let mut rows=Vec::new();
  for samples in [1,4] {renderer.set_globe_sample_count(samples).unwrap();for tilt in [0.,35.] {for heading in [0.,25.] {
   let pose=GlobePose{focus:GeographicPosition::new(48.645,-2.105).unwrap(),range_m:12000.,heading_deg:heading,tilt_deg:tilt};let camera=pose.camera([640.,480.]).unwrap();renderer.ui_state.globe_pose=Some(pose);renderer.ui_state.globe_tilt_deg=tilt;
   renderer.clear_raster_layers();renderer.begin_frame();renderer.prepare_globe_with_symbols(&mut context,None,&mut cache,Some(&profile)).unwrap();let base_path=self.out.join(format!("base-{samples}-{tilt}-{heading}.png"));renderer.save_screenshot(&base_path).unwrap();let base=image::open(base_path).unwrap().to_rgb8();
   let mut full:Option<Vec<Vec<u8>>>=None;
   for tiled in [false,true] {for below in [false,true] {for enabled in [true,false] {
    renderer.clear_raster_layers();let edge=if tiled{4}else{8};for y in (0..8).step_by(edge as usize) {for x in (0..8).step_by(edge as usize) {renderer.add_raster_layer(layer(x,y,edge,below),&context.scaler).unwrap();}}
    let groups=std::collections::HashSet::from([21010,if enabled{90020}else{90021}]);renderer.begin_frame();renderer.prepare_globe_with_symbols(&mut context,Some(&groups),&mut cache,Some(&profile)).unwrap();
    let name=format!("{samples}-{tilt}-{heading}-{tiled}-{below}-{enabled}");let path=self.out.join(format!("{name}.png"));renderer.save_screenshot(&path).unwrap();let image=image::open(path).unwrap().to_rgb8();
    let diag=renderer.globe_preview_diagnostics().unwrap();let count=diag.preparation["raster_surface_count"].as_u64().unwrap();assert_eq!(count,if enabled {if tiled{4}else{1}}else{0});
    let mut checked=0;let mut opaque=0;let mut nodata=0;let mut probe_opaque=None;let mut probe_nodata=None;
    for y in (0..480u32).step_by(2) {for x in (0..640u32).step_by(2) {
     let p=[x as f64+0.5,y as f64+0.5];let Some(hit)=camera.pick(p).unwrap() else{continue};let geo=hit.geodetic.surface;
     let q=[(geo.longitude()-b.min_x)/b.width()*8.,(b.max_y-geo.latitude())/b.height()*8.];
     if !q.iter().all(|v|*v>0. && *v<8.) || q.iter().any(|v|v.fract()<0.12 || v.fract()>0.88) {continue;}
     let expected=pixel(q[0].floor() as u32,q[1].floor() as u32);let px=image.get_pixel(x+60,y+50).0;let old=base.get_pixel(x+60,y+50).0;
     let color=if enabled && !below && expected[3]!=0 {expected[..3].try_into().unwrap()}else{old};
     assert!((0..3).all(|k|(i16::from(px[k])-i16::from(color[k])).abs()<=3),"{name} cell{q:?} point{p:?} actual{px:?} expected{color:?}");
     checked+=1;if expected[3]!=0 {opaque+=1;probe_opaque.get_or_insert(p);}else{nodata+=1;probe_nodata.get_or_insert(p);}
    }}
    assert!(checked>1000 && opaque>100 && nodata>100);
    let pick=|r:&mut WgpuRenderer,p:[f64;2]|r.globe_feature_candidates(&context,ScreenPoint::new(p[0] as f32+60.,p[1] as f32+50.),0.).unwrap();
    assert!(pick(&mut renderer,probe_nodata.unwrap()).iter().any(|(source,_)|*source==0));
    let opaque_pick=pick(&mut renderer,probe_opaque.unwrap());if enabled && !below {assert!(opaque_pick.is_empty(),"Raster ID must not be mislabeled as ENC source");} else {assert!(opaque_pick.iter().any(|(source,_)|*source==0));}
    let mut tile_pixel_mismatches=None;
    if enabled && !below {if tiled {let earlier=full.as_ref().unwrap();let mismatches=image.as_raw().chunks_exact(3).zip(earlier).filter(|(a,b)|a!=*b).count();assert_eq!(mismatches,0,"Full/tiled original-grid sampling must match every pixel: {name}");tile_pixel_mismatches=Some(mismatches);}else{full=Some(image.as_raw().chunks_exact(3).map(|p|p.to_vec()).collect::<Vec<_>>());}}
    rows.push(serde_json::json!({"name":name,"samples":samples,"tilt":tilt,"heading":heading,"tiled":tiled,"below":below,"group_enabled":enabled,"interior_source_cell_pixels":checked,"opaque":opaque,"nodata":nodata,"tile_full_all_pixel_mismatches":tile_pixel_mismatches,"raster_surface_count":count,"vector_source0_cell3_feature42_preserved":true}));
   }}}
  }}}
  assert!(!window.is_visible().unwrap_or(true));assert!(!window.has_focus());std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"cases":rows,"native_renderer_pane":true,"ray_source_cell_oracle":true,"boundary_cells_excluded_fraction":0.12,"visible":false,"focused":false,"official_iho_fixture":false})).unwrap()).unwrap();event.exit();
 }
 fn window_event(&mut self,_:&ActiveEventLoop,_:WindowId,_:WindowEvent){}
}
fn main(){let out=std::env::args().nth(1).expect("output");EventLoop::new().unwrap().run_app(&mut App{out:out.into()}).unwrap();}
