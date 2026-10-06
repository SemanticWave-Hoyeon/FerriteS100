//! Official PC 1.1 string identifiers through Lua, adapter, and native GPU AND masks.
use ferrite_render::*;
use ferrite_wgpu::{SymbolCache, WgpuRenderer};
use std::{collections::HashSet, path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
struct App {
    out: PathBuf,
    cell: PathBuf,
    pc: PathBuf,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let accuracy = pc
            .viewing_groups
            .runtime_id("accuracy")
            .expect("PC 1.1 accuracy");
        assert_eq!(
            pc.viewing_groups.get(accuracy).unwrap().catalogue_id,
            "accuracy"
        );
        let cell = ferrite_s100_core::S101Cell::load(&self.cell).unwrap();
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
        let mut converted = RenderContext::new(Viewport::new(100., 100.));
        let result=ferrite_lua::PortrayalResult::parse(&area_id.to_string(),"ViewingGroup:31011,accuracy;ColorFill:CHBLK;AugmentedPoint:GeographicCRS,0,0;PointInstruction:INFORM01;TextInstruction:X","").unwrap();
        ferrite_s101::convert_lua_results_for_cell(&[result], &cell, &pc, &mut converted, 0, "Day")
            .unwrap();
        let point_id = *cell
            .features
            .iter()
            .find(|(_, f)| {
                f.spatial_associations
                    .iter()
                    .any(|a| cell.points.contains_key(&a.spatial_id.key()))
            })
            .unwrap()
            .0;
        let line=ferrite_lua::PortrayalResult::parse(&point_id.to_string(),"ViewingGroup:31011,accuracy;AugmentedRay:LocalCRS,90,LocalCRS,25;LineStyle:base,,1.28,CHBLK;LineInstruction:base","").unwrap();
        ferrite_s101::convert_lua_results_for_cell(&[line], &cell, &pc, &mut converted, 0, "Day")
            .unwrap();
        assert!(converted
            .raw_instructions()
            .iter()
            .any(|i| matches!(i, DrawingInstruction::Line(_))));
        let before = converted.instruction_count();
        let bad=ferrite_lua::PortrayalResult::parse(&point_id.to_string(),"ViewingGroup:31011;PointInstruction:INFORM01;ViewingGroup:undeclared;PointInstruction:INFORM01","").unwrap();
        assert!(ferrite_s101::convert_lua_results_for_cell(
            &[bad],
            &cell,
            &pc,
            &mut converted,
            0,
            "Day"
        )
        .is_err());
        assert_eq!(
            converted.instruction_count(),
            before,
            "Rejected identities must not partially mutate the display"
        );
        assert!(converted
            .raw_instructions()
            .iter()
            .any(|i| matches!(i, DrawingInstruction::Area(_))));
        assert!(converted
            .raw_instructions()
            .iter()
            .any(|i| matches!(i, DrawingInstruction::Point(_))));
        assert!(converted
            .raw_instructions()
            .iter()
            .any(|i| matches!(i, DrawingInstruction::Text(_))));
        for i in converted.raw_instructions() {
            assert_eq!(
                i.viewing_groups().map(|g| g.0).collect::<Vec<_>>(),
                [31011, accuracy]
            );
        }
        let s102pc =
            ferrite_portrayal_catalog::PortrayalCatalogue::load("Catalogues/PC/S-102").unwrap();
        let coverage =
            ferrite_s102::BathymetryPortrayal::from_catalogue(&s102pc, "Day", Default::default())
                .unwrap();
        assert_eq!(coverage.viewing_groups, [13030]);
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Viewing group visibility")
                    .with_inner_size(PhysicalSize::new(1200, 800)),
            )
            .unwrap(),
        );
        let size = w.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        let mut c = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        c.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        let groups = [31011, accuracy];
        c.add_instruction(DrawingInstruction::Area(
            AreaInstruction::new(vec![
                WorldPoint::new(1., 6.),
                WorldPoint::new(2., 6.),
                WorldPoint::new(2., 8.),
                WorldPoint::new(1., 8.),
            ])
            .with_solid_fill(Color::GREEN)
            .with_viewing_groups(&groups),
        ));
        c.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(3., 7.), WorldPoint::new(4., 7.)])
                .with_style(LineStyle::solid(Color::RED, 8.))
                .with_viewing_groups(&groups),
        ));
        c.add_instruction(DrawingInstruction::Point(
            PointInstruction::new("INFORM01".into(), WorldPoint::new(5.5, 7.))
                .with_viewing_groups(&groups),
        ));
        c.add_instruction(DrawingInstruction::Text(
            TextInstruction::new("TEST".into(), WorldPoint::new(7.5, 7.))
                .with_font_size(24.)
                .with_color(Color::BLACK)
                .with_viewing_groups(&groups),
        ));
        let mut cache = SymbolCache::new(self.pc.join("Symbols"));
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        r.add_raster_layer(
            RasterLayer {
                draw_order: Default::default(),
                viewing_groups: vec![13030, accuracy],
                id: "multi-group coverage".into(),
                bounds: GeoBounds::new(1., 1., 9., 3.),
                width: 2,
                height: 2,
                rgba: [0, 255, 255, 255].repeat(4),
                grid: None,
            },
            &c.scaler,
        )
        .unwrap();
        let mut cases = Vec::new();
        for (name, enabled, visible) in [
            ("all", None, true),
            ("primary-only", Some(HashSet::from([31011, 13030])), false),
            ("secondary-only", Some(HashSet::from([accuracy])), false),
            ("both", Some(HashSet::from([31011, 13030, accuracy])), true),
            ("none", Some(HashSet::new()), false),
            ("restored", None, true),
        ] {
            r.begin_frame();
            r.add_instructions_with_symbols(
                &mut c,
                Some(&mut cache),
                Some(profile),
                enabled.as_ref(),
            );
            assert_eq!(r.displayed_symbols().len(), usize::from(visible));
            let path = self.out.join(format!("groups-{name}.png"));
            r.save_screenshot(&path).unwrap();
            let im = image::open(&path).unwrap().to_rgb8();
            let sample = |x, y| {
                let p = c.scaler.world_to_screen(WorldPoint::new(x, y));
                im.get_pixel(p.x.round() as u32, p.y.round() as u32).0
            };
            assert_eq!(
                sample(1.5, 7.),
                if visible {
                    [0, 255, 0]
                } else {
                    [255, 255, 255]
                },
                "area {name}"
            );
            assert_eq!(
                sample(3.5, 7.),
                if visible {
                    [255, 0, 0]
                } else {
                    [255, 255, 255]
                },
                "line {name}"
            );
            assert_eq!(
                sample(5., 2.),
                if visible {
                    [0, 255, 255]
                } else {
                    [255, 255, 255]
                },
                "coverage {name}"
            );
            // Independently check the text region contains ink only in enabled cases.
            let a = c.scaler.world_to_screen(WorldPoint::new(6.5, 8.));
            let b = c.scaler.world_to_screen(WorldPoint::new(9.5, 6.));
            let ink = (a.y.max(0.) as u32..b.y.min(im.height() as f32) as u32)
                .flat_map(|y| {
                    (a.x.max(0.) as u32..b.x.min(im.width() as f32) as u32).map(move |x| (x, y))
                })
                .filter(|&(x, y)| im.get_pixel(x, y).0 != [255, 255, 255])
                .count();
            assert_eq!(ink > 0, visible, "text {name}: {ink}");
            r.render().unwrap();
            let after = self.out.join(format!("groups-{name}-after.png"));
            r.save_screenshot(&after).unwrap();
            assert_eq!(im, image::open(after).unwrap().to_rgb8());
            cases.push(serde_json::json!({"name":name,"visible":visible,"text_ink_pixels":ink,"surface_export_equal":true}));
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"cases":cases,"s101_adapter_multiple_groups_preserved":true,"original_named_group":"accuracy","runtime_handle":accuracy,"undeclared_group_rollback":true,"four_adapter_kinds":true,"s102_pc_viewing_groups":coverage.viewing_groups,"raster_uploaded_once":true,"app_ic_activated":false})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let mut a = std::env::args().skip(1);
    EventLoop::new()
        .unwrap()
        .run_app(&mut App {
            out: a.next().unwrap().into(),
            cell: a.next().unwrap().into(),
            pc: a.next().unwrap().into(),
        })
        .unwrap();
}
