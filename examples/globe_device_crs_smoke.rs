//! Native absolute-device portrayal, including views without Earth at the pixel.
use ferrite_kernel::{geodesy::GeographicPosition, globe_navigation::GlobePose};
use ferrite_render::*;
use ferrite_wgpu::{SymbolCache,WgpuRenderer};
use std::{path::PathBuf,sync::Arc};
use winit::{application::ApplicationHandler,dpi::PhysicalSize,event::WindowEvent,event_loop::{ActiveEventLoop,EventLoop},window::{Window,WindowId}};
struct App{out:PathBuf,pc:PathBuf}
impl ApplicationHandler for App {
    fn resumed(&mut self,e:&ActiveEventLoop){
        std::fs::create_dir_all(&self.out).unwrap();
        let window=Arc::new(e.create_window(Window::default_attributes().with_title("S-100 globe device coordinates").with_inner_size(PhysicalSize::new(1100,800))).unwrap());
        let density=window.scale_factor();
        let mut renderer=pollster::block_on(WgpuRenderer::new(window)).unwrap();
        let pc=ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let profile=pc.color_profiles.profiles.get("Day").unwrap();
        let mut cache=SymbolCache::new(self.pc.join("Symbols"));
        let mut context=RenderContext::new(Viewport::with_origin(40.,50.,900.,600.));
        context.set_bounds(GeoBounds::new(-0.1,-0.1,0.1,0.1));
        context.scaler.set_pixel_ratio(density);
        renderer.ui_state.globe_preview=true;
        let mut checks=Vec::new();
        for kind in ["symbol","text"] {
            let mut baseline:Option<[u32;4]>=None;
            for range in [30000.,150.,2e7] {
                for tilt in [0.,70.] {
                    for heading in [0.,90.] {
                        renderer.ui_state.globe_pose=Some(GlobePose{focus:if heading==0. {GeographicPosition::new(0.,0.).unwrap()} else {GeographicPosition::new(70.,179.9).unwrap()},range_m:range,heading_deg:heading,tilt_deg:tilt});
                        renderer.ui_state.globe_tilt_deg=tilt;
                        context.clear_instructions();
                        let mut command=if kind=="symbol" {
                            DrawingInstruction::Point(PointInstruction::new("ACHBRT07".into(),WorldPoint::new(40.,40.)).with_offset(1.,-2.))
                        } else {
                            DrawingInstruction::Text(TextInstruction::new("Device mm".into(),WorldPoint::new(40.,40.)).with_font_size(24.).with_color(Color::rgb(1.,0.,1.)).with_offset(1.,-2.).with_alignment(HAlign::Center,VAlign::Middle))
                        };
                        command.set_portrayal_origin(PortrayalOrigin::augmented_point(PointOriginCrs::Portrayal,[40.,40.]).unwrap());
                        context.add_instruction(command);
                        renderer.begin_frame();
                        renderer.prepare_globe_with_symbols(&mut context,None,&mut cache,Some(profile)).unwrap();
                        let diagnostics=renderer.globe_preview_diagnostics().unwrap();
                        assert_eq!(diagnostics.rejected_geometries,0,"Device geometry rejected: {:?}",diagnostics.reasons);
                        assert_eq!(diagnostics.missing_symbol_resources,0);
                        assert_eq!(if kind=="symbol" {diagnostics.symbols} else {diagnostics.texts},1,"Device command hidden by Earth horizon");
                        let footprint=if kind=="symbol" {diagnostics.symbol_footprints_px[0]} else {diagnostics.text_footprints_px[0]};
                        let path=self.out.join(format!("{kind}-{range}-{tilt}-{heading}.png"));
                        renderer.save_screenshot(&path).unwrap();
                        let image=image::open(path).unwrap().to_rgb8();
                        let mut bounds=[u32::MAX,u32::MAX,0,0];let mut colored=0;
                        for (x,y,p) in image.enumerate_pixels(){
                            if p[0] as i16-p[1] as i16>60 && p[2] as i16-p[1] as i16>60 {
                                colored+=1;bounds[0]=bounds[0].min(x);bounds[1]=bounds[1].min(y);bounds[2]=bounds[2].max(x);bounds[3]=bounds[3].max(y);
                            }
                        }
                        assert!(colored>16,"Empty device native raster");
                        if let Some(previous)=baseline {for a in 0..4 {assert!(bounds[a].abs_diff(previous[a])<=1,"Device native footprint moved with camera: {bounds:?}/{previous:?}");}} else {baseline=Some(bounds);}
                        checks.push(serde_json::json!({"kind":kind,"range_m":range,"zoom_relative_to_30000":30000./range,"tilt":tilt,"heading":heading,"colored_pixels":colored,"native_bounds":bounds,"cpu_footprint":footprint,"native_camera_motion_tolerance_px":1}));
                    }
                }
            }
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_string_pretty(&serde_json::json!({"checks":checks,"density":density,"shifted_viewport":[40,50,900,600],"fixture":"synthetic Portrayal CRS commands, official ACHBRT07 and actual text","real_product_native_verified":false,"coverage_3d_verified":false})).unwrap()).unwrap();
        e.exit();
    }
    fn window_event(&mut self,_:&ActiveEventLoop,_:WindowId,_:WindowEvent){}
}
fn main(){let mut app=App{out:std::env::args().nth(1).map(PathBuf::from).expect("output directory"),pc:std::env::args().nth(2).map(PathBuf::from).expect("PC directory")};EventLoop::new().unwrap().run_app(&mut app).unwrap();}
