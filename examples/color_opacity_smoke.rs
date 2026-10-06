//! Actual S101 adapter colour commands -> palette remap -> native chart GPU oracle.
use ferrite_render::{
    AreaInstruction, Color, DisplayPriority, DrawingInstruction, GeoBounds, RenderContext,
    TextInstruction, Viewport, WorldPoint,
};
use ferrite_wgpu::WgpuRenderer;
use std::{path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
struct App {
    args: Vec<String>,
}
fn pixel(im: &image::RgbImage, p: [f32; 2], expected: [u8; 3]) {
    let actual = im.get_pixel(p[0].round() as u32, p[1].round() as u32).0;
    assert!(
        (0..3).all(|i| (actual[i] as i16 - expected[i] as i16).abs() <= 2),
        "{actual:?} != {expected:?}"
    );
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let out = PathBuf::from(&self.args[1]);
        std::fs::create_dir_all(&out).unwrap();
        let cell = ferrite_s100_core::S101Cell::load(&self.args[2]).unwrap();
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.args[3]).unwrap();
        let area_id = *cell
            .features
            .iter()
            .find(|(_, f)| {
                f.spatial_associations
                    .iter()
                    .any(|a| cell.surfaces.contains_key(&a.spatial_id.key()))
            })
            .unwrap()
            .0;
        let result=ferrite_lua::PortrayalResult::parse(&area_id.to_string(),"ColorFill:LITRD,0.5;AugmentedPoint:GeographicCRS,0,0;FontColor:CHBLK,1;FontBackgroundColor:LITRD,0.5;FontSize:24;TextAlignHorizontal:Center;TextAlignVertical:Center;DrawingPriority:5;TextInstruction:TEST;AugmentedPoint:GeographicCRS,5,0;FontColor:LITRD,0.5;FontBackgroundColor:CHBLK,1;TextInstruction:H","").unwrap();
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_inner_size(PhysicalSize::new(1400, 900))
                    .with_title("Authored opacity and text background"),
            )
            .unwrap(),
        );
        let density = w.scale_factor();
        let size = w.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::BLUE;
        let mut c = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        c.set_bounds(GeoBounds::new(-10., -10., 10., 10.));
        ferrite_s101::convert_lua_results_for_cell(&[result], &cell, &pc, &mut c, 0, "Day")
            .unwrap();
        let mut area: AreaInstruction = c
            .raw_instructions()
            .iter()
            .find_map(|i| {
                if let DrawingInstruction::Area(a) = i {
                    Some(a.clone())
                } else {
                    None
                }
            })
            .unwrap();
        let texts: Vec<_> = c
            .raw_instructions()
            .iter()
            .filter_map(|i| {
                if let DrawingInstruction::Text(t) = i {
                    Some(t.clone())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(texts.len(), 2);
        let mut background = texts[0].clone();
        let glyph = texts[1].clone();
        assert_eq!(area.fill_opacity, 0.5);
        assert_eq!(background.color.a, 0.);
        assert_eq!(background.background.unwrap().a, 0.5);
        assert_eq!(glyph.color.a, 0.5);
        // Retain adapter style data, use a known rectangle to isolate alpha compositing.
        area.exterior = vec![
            WorldPoint::new(-6., -1.),
            WorldPoint::new(-4., -1.),
            WorldPoint::new(-4., 1.),
            WorldPoint::new(-6., 1.),
        ];
        area.interiors.clear();
        let mut ghost: TextInstruction = glyph.clone();
        ghost.color_opacity = 0.;
        ghost.priority = DisplayPriority(99);
        let mut cases = Vec::new();
        for (name, palette, expected) in [
            ("opaque", Color::RED, [128, 0, 128]),
            ("palette-alpha", Color::GREEN.with_alpha(0.8), [0, 102, 153]),
        ] {
            for rotation in [0., 45., 90.] {
                background.rotation = rotation;
                for zoom in [0.5, 1., 2.] {
                    c.clear_instructions();
                    for mut inst in [
                        DrawingInstruction::Area(area.clone()),
                        DrawingInstruction::Text(background.clone()),
                        DrawingInstruction::Text(ghost.clone()),
                        DrawingInstruction::Text(glyph.clone()),
                    ] {
                        inst.remap_colors(&|_| palette);
                        c.add_instruction(inst);
                    }
                    r.reset_pan_offset();
                    r.set_gpu_zoom(1., 0., 0.);
                    r.begin_frame();
                    r.add_instructions(&mut c);
                    let pivot = c.scaler.world_to_screen(WorldPoint::new(0., 0.));
                    r.set_gpu_zoom(zoom, pivot.x, pivot.y);
                    let path = out.join(format!("{name}-{rotation}-{zoom}.png"));
                    r.save_screenshot(&path).unwrap();
                    let im = image::open(path).unwrap().to_rgb8();
                    let project = |x: f64| {
                        let p = c.scaler.world_to_screen(WorldPoint::new(x, 0.));
                        [pivot.x + (p.x - pivot.x) * zoom, pivot.y]
                    };
                    pixel(&im, project(-5.), expected);
                    pixel(&im, project(0.), expected);
                    let pos = project(5.);
                    let mut matches = 0;
                    for y in
                        (pos[1] as i32 - 50).max(0)..(pos[1] as i32 + 50).min(im.height() as i32)
                    {
                        for x in
                            (pos[0] as i32 - 50).max(0)..(pos[0] as i32 + 50).min(im.width() as i32)
                        {
                            let actual = im.get_pixel(x as u32, y as u32).0;
                            if (0..3).all(|i| (actual[i] as i16 - expected[i] as i16).abs() <= 2) {
                                matches += 1;
                            }
                        }
                    }
                    assert!(
                        matches > 20,
                        "visible glyph was suppressed by invisible label or lost alpha: {matches}"
                    );
                    r.render().unwrap();
                    let after = out.join(format!("{name}-{rotation}-{zoom}-after.png"));
                    r.save_screenshot(&after).unwrap();
                    assert_eq!(im, image::open(after).unwrap().to_rgb8());
                    cases.push(serde_json::json!({"palette":name,"rotation":rotation,"zoom":zoom,"area_center":true,"background_center":true,"glyph_pixels":matches,"surface_export_equal":true}));
                }
            }
        }
        std::fs::write(out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"density":density,"adapter_transparency_verified":true,"cases":cases,"native_os_input_verified":false,"execution_os":std::env::consts::OS,"windows_verified":cfg!(target_os="windows")})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    EventLoop::new()
        .unwrap()
        .run_app(&mut App {
            args: std::env::args().collect(),
        })
        .unwrap();
}
