//! Native device-position regression using official symbols and actual text.
use ferrite_render::*;
use ferrite_wgpu::{SymbolCache, WgpuRenderer};
use std::{path::PathBuf, sync::Arc};
use winit::{application::ApplicationHandler, dpi::PhysicalSize, event::WindowEvent, event_loop::{ActiveEventLoop, EventLoop}, window::{Window, WindowId}};

struct App { out: PathBuf, pc: PathBuf }
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let window = Arc::new(el.create_window(Window::default_attributes().with_title("S-100 Portrayal device coordinates").with_inner_size(PhysicalSize::new(640,400))).unwrap());
        let size = window.inner_size();
        let density = window.scale_factor();
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.background_color = Color::WHITE;
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        let mut cache = SymbolCache::new(self.pc.join("Symbols"));
        let mut checks = Vec::new();
        for kind in ["symbol", "text"] {
            for zoom in [1., 4., 200.] {
                for rotation_crs in [RotationCrs::Portrayal, RotationCrs::Geographic] {
                    let mut reference = None;
                    for fixed in [false, true] {
                        let viewport = Viewport { x: 37., y: 25., width: size.width as f32 - 74., height: size.height as f32 - 60. };
                        let mut context = RenderContext::new(viewport);
                        let center = if zoom == 1. { -100. } else { 100. };
                        context.set_bounds(GeoBounds::new(center - 0.5/zoom, 48. - 0.5/zoom, center + 0.5/zoom, 48. + 0.5/zoom));
                        context.scaler.set_projection(FlatProjection::EllipsoidalMercator);
                        context.scaler.set_pixel_ratio(density);
                        let ppm = context.scaler.pixels_per_mm();
                        let screen = ScreenPoint::new((viewport.x as f64 + 40.*ppm) as f32, (viewport.y as f64 + viewport.height as f64 - 40.*ppm) as f32);
                        let anchor = if fixed { WorldPoint::new(40., 40.) } else { context.scaler.screen_to_world(screen) };
                        let mut command = match kind {
                            "symbol" => DrawingInstruction::Point(PointInstruction::new("ACHBRT07".into(), anchor).with_scale(4.).with_offset(1.,-2.).with_rotation(30.).with_rotation_crs(rotation_crs)),
                            "text" => DrawingInstruction::Text(TextInstruction::new("Device mm".into(), anchor).with_font_size(24.).with_offset(1.,-2.).with_rotation(30.).with_rotation_crs(rotation_crs).with_alignment(HAlign::Center,VAlign::Middle)),
                            _ => unreachable!(),
                        };
                        command.set_portrayal_origin(if fixed { PortrayalOrigin::augmented_point(PointOriginCrs::Portrayal,[40.,40.]).unwrap() } else { PortrayalOrigin::NonPoint });
                        context.add_instruction(command);
                        context.get_sorted_instructions();
                        renderer.begin_frame();
                        renderer.reset_pan_offset();
                        // Deliberately small wrap offset: accidental copies become visible.
                        renderer.set_lon_wrap_pixels(if fixed {80.} else {0.});
                        renderer.add_instructions_with_symbols(&mut context,Some(&mut cache),Some(profile),None);
                        assert_eq!(renderer.requires_visibility_rebuild_for_navigation(), fixed || rotation_crs == RotationCrs::Geographic);
                        if fixed {
                            assert!(!renderer.set_gpu_view_scaler(&context.scaler));
                            let before=(renderer.get_pan_offset(),renderer.fast_view_scales());
                            assert!(!renderer.set_pan_offset(20.,-10.));
                            assert!(!renderer.set_gpu_zoom(4.,screen.x,screen.y));
                            assert_eq!(renderer.add_pan_offset(20.,-10.),None);
                            assert_eq!((renderer.get_pan_offset(),renderer.fast_view_scales()),before);

                            assert!(!renderer.coverage_fragment_visible(0,1,[screen.x,screen.y]));
                            assert!(!renderer.coverage_fragment_visible(0,2,[screen.x,screen.y]));
                        }
                        let path = self.out.join(format!("{kind}-{zoom}-{rotation_crs:?}-{}.png",if fixed {"device"} else {"reference"}));
                        renderer.save_screenshot(&path).unwrap();
                        let actual = image::open(path).unwrap().to_rgba8();
                        let colored = actual.pixels().filter(|p|p.0 != [255;4]).count();
                        assert!(colored>16,"Empty native device image");
                        if fixed {
                            let expected: &image::RgbaImage = reference.as_ref().unwrap();
                            let different = actual.pixels().zip(expected.pixels()).filter(|(a,b)|a.0!=b.0).count();
                            assert_eq!(different,0,"Device point moved or repeated: {kind}/{zoom}/{rotation_crs:?}");
                            checks.push(serde_json::json!({"kind":kind,"zoom":zoom,"rotation_crs":format!("{rotation_crs:?}"),"different_pixels":different,"colored_pixels":colored,"wrapped_copies_rejected":true,"gpu_affine_rejected":true,"low_level_affine_rejected_without_mutation":true}));
                        } else { reference=Some(actual); }
                    }
                }
            }
        }
        // Same-priority mixed ranges: geographical symbols retain wrap copies,
        // while the device symbol is emitted once. The reference explicitly
        // constructs the three geographic copies with wrapping disabled.
        let mut reference = None;
        for fixed in [false, true] {
            let viewport = Viewport::new(size.width as f32, size.height as f32);
            let mut context = RenderContext::new(viewport);
            context.set_bounds(GeoBounds::new(-1.,47.,1.,49.));
            context.scaler.set_pixel_ratio(density);
            let world = WorldPoint::new(0.,48.);
            let ordinary = context.scaler.world_to_screen(world);
            for shift in if fixed { vec![0.] } else { vec![0., -80., 80.] } {
                let position = context.scaler.screen_to_world(ScreenPoint::new(ordinary.x+shift,ordinary.y));
                let mut command = DrawingInstruction::Point(PointInstruction::new("ACHBRT07".into(),position).with_scale(0.8));
                command.set_portrayal_origin(PortrayalOrigin::NonPoint);
                context.add_instruction(command);
            }
            let ppm=context.scaler.pixels_per_mm();
            let screen=ScreenPoint::new((40.*ppm) as f32,(size.height as f64-40.*ppm) as f32);
            let position=if fixed {WorldPoint::new(40.,40.)} else {context.scaler.screen_to_world(screen)};
            let mut device=DrawingInstruction::Point(PointInstruction::new("ACHBRT07".into(),position).with_scale(0.8));
            device.set_portrayal_origin(if fixed {PortrayalOrigin::augmented_point(PointOriginCrs::Portrayal,[40.,40.]).unwrap()} else {PortrayalOrigin::NonPoint});
            context.add_instruction(device);
            context.get_sorted_instructions();
            renderer.begin_frame();
            renderer.reset_pan_offset();
            renderer.set_lon_wrap_pixels(if fixed {80.} else {0.});
            renderer.add_instructions_with_symbols(&mut context,Some(&mut cache),Some(profile),None);
            let path=self.out.join(format!("mixed-{}.png",if fixed {"device"} else {"reference"}));
            renderer.save_screenshot(&path).unwrap();
            let actual=image::open(path).unwrap().to_rgba8();
            if fixed {
                let expected: &image::RgbaImage=reference.as_ref().unwrap();
                let different=actual.pixels().zip(expected.pixels()).filter(|(a,b)|a.0!=b.0).count();
                assert_eq!(different,0,"Mixed geographic/device ranges repeat device or drop ordinary copies");
                checks.push(serde_json::json!({"kind":"mixed_symbol_ranges","different_pixels":different,"geographic_wrap_copies":3,"device_copies":1}));
            } else {reference=Some(actual);}
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_string_pretty(&serde_json::json!({"checks":checks,"density":density,"extent":[size.width,size.height],"scene":"synthetic device commands with official S-101 ACHBRT07 and text","shifted_viewport":true,"real_product_native_verified":false})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self,_:&ActiveEventLoop,_:WindowId,_:WindowEvent) {}
}
fn main(){ let mut app=App{out:std::env::args().nth(1).map(PathBuf::from).expect("output directory"),pc:std::env::args().nth(2).map(PathBuf::from).expect("PC directory")}; EventLoop::new().unwrap().run_app(&mut app).unwrap(); }
