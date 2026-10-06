//! Real SHOM feature and catalogue; synthetic portrayal output through the
//! actual parser, S-101 adapter, cache, flat renderer, and globe renderer.
use ferrite_kernel::{geodesy::GeographicPosition,globe_navigation::GlobePose};
use ferrite_render::*;
use ferrite_s100_core::{S101Cell,SpatialPrimitiveType};
use ferrite_wgpu::{SymbolCache,WgpuRenderer};
use std::{path::PathBuf,sync::Arc};
use winit::{application::ApplicationHandler,dpi::PhysicalSize,event::WindowEvent,event_loop::{ActiveEventLoop,EventLoop},window::{Window,WindowId}};
struct App{out:PathBuf,pc:PathBuf,cell:PathBuf}
impl ApplicationHandler for App{
    fn resumed(&mut self,e:&ActiveEventLoop){
        std::fs::create_dir_all(&self.out).unwrap();
        let cell=S101Cell::load(&self.cell).unwrap();
        let (id,anchor)=cell.features.iter().filter(|(_,f)|f.primitive_type==SpatialPrimitiveType::Point).filter_map(|(id,f)|f.spatial_associations.iter().find_map(|a|cell.points.get(&a.spatial_id.key())).map(|p|(*id,WorldPoint::new(p.position.x,p.position.y)))).min_by_key(|(id,_)|*id).unwrap();
        let pc=ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let profile=pc.color_profiles.profiles.get("Day").unwrap();
        let commands="AugmentedPoint:PortrayalCRS,40,40;LocalOffset:1,-2;PointInstruction:ACHBRT07;AugmentedPoint:PortrayalCRS,40,25;LocalOffset:1,-2;FontSize:24;FontColor:CHMGD;TextInstruction:Device adapter";
        let results=vec![ferrite_lua::PortrayalResult::parse(&format!("point|{id}"),commands,"").unwrap()];
        let window=Arc::new(e.create_window(Window::default_attributes().with_title("SHOM product-to-device portrayal").with_inner_size(PhysicalSize::new(1100,800))).unwrap());
        let density=window.scale_factor();
        let mut renderer=pollster::block_on(WgpuRenderer::new(window)).unwrap();
        let mut cache=SymbolCache::new(self.pc.join("Symbols"));
        let mut checks=Vec::new();
        for zoom in [1.,200.]{
            let mut context=RenderContext::new(Viewport::with_origin(40.,50.,900.,600.));
            context.set_bounds(GeoBounds::new(anchor.x-0.2/zoom,anchor.y-0.2/zoom,anchor.x+0.2/zoom,anchor.y+0.2/zoom));
            context.scaler.set_pixel_ratio(density);
            ferrite_s101::convert_lua_results_for_cell(&results,&cell,&pc,&mut context,0,"Day").unwrap();
            assert_eq!(context.instruction_count(),2);
            for instruction in context.raw_instructions(){
                assert!(instruction.portrayal_origin().is_device_fixed());
                assert_eq!(instruction.cell_index(),Some(0));
                let expected=if matches!(instruction,DrawingInstruction::Point(_)){[40.,40.]}else{[40.,25.]};
                let PortrayalOrigin::Point(source)=instruction.portrayal_origin() else{panic!("Missing source")};
                assert!(matches!(source.as_ref(),PointOriginGeometry::AugmentedPoint{crs:PointOriginCrs::Portrayal,coordinates} if *coordinates==expected));
            }
            let bytes=bincode::serialize(context.raw_instructions()).unwrap();
            let restored:Vec<DrawingInstruction>=bincode::deserialize(&bytes).unwrap();
            assert!(restored.iter().zip(context.raw_instructions()).all(|(a,b)|a.portrayal_origin()==b.portrayal_origin()));
            let mut reference_context=RenderContext::new(context.scaler.viewport);
            reference_context.scaler=context.scaler.clone();
            for mut instruction in restored{
                let screen=instruction.portrayal_origin().flat_source_position(&context.scaler,0.).unwrap().unwrap();
                let position=context.scaler.screen_to_world(screen);
                match &mut instruction{DrawingInstruction::Point(p)=>p.position=position,DrawingInstruction::Text(t)=>t.position=position,_=>panic!()}
                instruction.set_portrayal_origin(PortrayalOrigin::NonPoint);
                reference_context.add_instruction(instruction);
            }
            let mut reference=None;
            for fixed in [false,true]{
                renderer.ui_state.globe_preview=false;
                renderer.begin_frame();renderer.reset_pan_offset();renderer.set_lon_wrap_pixels(if fixed{80.}else{0.});
                let c=if fixed{&mut context}else{&mut reference_context};
                renderer.add_instructions_with_symbols(c,Some(&mut cache),Some(profile),None);
                let path=self.out.join(format!("flat-{zoom}-{}.png",if fixed{"adapter"}else{"reference"}));
                renderer.save_screenshot(&path).unwrap();
                let image=image::open(path).unwrap().to_rgba8();
                if fixed{let before:&image::RgbaImage=reference.as_ref().unwrap();let different=image.pixels().zip(before.pixels()).filter(|(a,b)|a.0!=b.0).count();assert_eq!(different,0,"Product adapter changed device placement");checks.push(serde_json::json!({"kind":"flat","zoom":zoom,"different_pixels":different,"cache_roundtrip":true}));}else{reference=Some(image);}
            }
            renderer.ui_state.globe_preview=true;
            renderer.ui_state.globe_pose=Some(GlobePose{focus:GeographicPosition::new(anchor.y,anchor.x).unwrap(),range_m:30000./zoom,heading_deg:90.,tilt_deg:70.});
            renderer.ui_state.globe_tilt_deg=70.;
            renderer.begin_frame();
            renderer.prepare_globe_with_symbols(&mut context,None,&mut cache,Some(profile)).unwrap();
            let d=renderer.globe_preview_diagnostics().unwrap();
            assert_eq!((d.symbols,d.texts,d.rejected_geometries),(1,1,0));
            let footprints=serde_json::json!({"symbol":d.symbol_footprints_px,"text":d.text_footprints_px});
            let path=self.out.join(format!("globe-{zoom}-adapter.png"));renderer.save_screenshot(&path).unwrap();
            let image=image::open(path).unwrap().to_rgb8();
            let colored=image.pixels().filter(|p|p[0] as i16-p[1] as i16>60 && p[2] as i16-p[1] as i16>60).count();
            assert!(colored>16,"Product adapter native image empty");
            checks.push(serde_json::json!({"kind":"globe","zoom":zoom,"symbols":1,"texts":1,"colored_pixels":colored,"footprints":footprints}));
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_string_pretty(&serde_json::json!({"checks":checks,"feature_id":id,"actual_cell":self.cell,"pc":self.pc,"actual_reference":[anchor.x,anchor.y],"density":density,"synthetic_lua_command":true,"official_pc_emits_device_commands_claimed":false,"raw_dataset_modified":false,"signature_verification_in_this_fixture":false,"coverage_3d_verified":false})).unwrap()).unwrap();e.exit();
    }
    fn window_event(&mut self,_:&ActiveEventLoop,_:WindowId,_:WindowEvent){}
}
fn main(){let mut app=App{out:std::env::args().nth(1).map(PathBuf::from).expect("output"),pc:std::env::args().nth(2).map(PathBuf::from).expect("PC"),cell:std::env::args().nth(3).map(PathBuf::from).expect("cell")};EventLoop::new().unwrap().run_app(&mut app).unwrap();}
