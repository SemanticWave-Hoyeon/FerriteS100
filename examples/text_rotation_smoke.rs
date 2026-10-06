//! Real S-101 adapter and GPU text comparison with an independent geodesic basis.
use ferrite_kernel::geodesy::{direct, GeographicPosition};
use ferrite_render::*;
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
    out: PathBuf,
    cell: PathBuf,
    pc: PathBuf,
}
fn direction(s: &Scaler, p: WorldPoint, bearing: f64) -> [f64; 2] {
    let origin = GeographicPosition::new(p.y, p.x).unwrap();
    let a = direct(origin, bearing + 180., 0.5).unwrap();
    let b = direct(origin, bearing, 0.5).unwrap();
    [
        s.scale_x() * (b.longitude_near(p.x).unwrap() - a.longitude_near(p.x).unwrap()),
        -s.scale_y()
            * (s.projection().project_y(b.latitude()) - s.projection().project_y(a.latitude())),
    ]
}
impl ApplicationHandler for App {
    fn resumed(&mut self, e: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            e.create_window(
                Window::default_attributes().with_inner_size(PhysicalSize::new(1000, 700)),
            )
            .unwrap(),
        );
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        let cell = ferrite_s100_core::S101Cell::load(&self.cell).unwrap();
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let vg = pc
            .viewing_groups
            .groups
            .values()
            .filter_map(|v| v.catalogue_id.parse::<u32>().ok())
            .min()
            .unwrap();
        let mut rows = Vec::new();
        for projection in [
            FlatProjection::LocalGeographic,
            FlatProjection::EllipsoidalMercator,
        ] {
            for lat in [0., 70.] {
                for (name, crs, tangent) in [
                    ("PortrayalCRS", RotationCrs::Portrayal, None),
                    ("GeographicCRS", RotationCrs::Geographic, None),
                    ("LocalCRS", RotationCrs::Local, None),
                    ("LineCRS", RotationCrs::Line, Some(90.)),
                    ("LineCRS-reversed", RotationCrs::Line, Some(270.)),
                ] {
                    let p = WorldPoint::new(179.9, lat);
                    let mut c = RenderContext::new(Viewport::new(1000., 700.));
                    c.scaler.set_projection(projection);
                    c.set_bounds(GeoBounds::new(178.9, lat - 1., 180.9, lat + 1.));
                    let lua_name = if tangent.is_some() { "LineCRS" } else { name };
                    let result=ferrite_lua::PortrayalResult::parse("TEXT-CRS",&format!("ViewingGroup:{vg};Rotation:{lua_name},45;AugmentedPoint:GeographicCRS,179.9,{lat};FontColor:CHBLK;FontSize:24;TextAlignHorizontal:Center;TextAlignVertical:Center;LocalOffset:3,2;TextInstruction:Heading"),"").unwrap();
                    ferrite_s101::convert_lua_results_for_cell(
                        &[result],
                        &cell,
                        &pc,
                        &mut c,
                        7,
                        "Day",
                    )
                    .unwrap();
                    let mut t = match &c.raw_instructions()[0] {
                        DrawingInstruction::Text(t) => t.clone(),
                        _ => panic!("text lost"),
                    };
                    assert_eq!(t.rotation_crs, crs);
                    t.curve_tangent_bearing = tangent;
                    let bytes = bincode::serialize(&t).unwrap();
                    let restored: TextInstruction = bincode::deserialize(&bytes).unwrap();
                    assert_eq!(restored.rotation_crs, crs);
                    assert_eq!(restored.curve_tangent_bearing, tangent);
                    let expected = if crs == RotationCrs::Geographic {
                        let d = direction(&c.scaler, p, 45.);
                        d[0].atan2(-d[1]).to_degrees()
                    } else if let Some(b) = tangent {
                        let d = direction(&c.scaler, p, b);
                        d[1].atan2(d[0]).to_degrees() + 45.
                    } else {
                        45.
                    };
                    let expected = expected.rem_euclid(360.) as f32;
                    let actual = flat_text_rotation(&t, &c.scaler).unwrap();
                    let error = (actual - expected).abs();
                    assert!(error < 0.001, "bearing oracle {name} {actual} {expected}");
                    c.clear_instructions();
                    c.add_instruction(DrawingInstruction::Text(t.clone()));
                    r.begin_frame();
                    r.add_instructions_with_symbols(&mut c, None, None, None);
                    let view_sensitive = crs == RotationCrs::Geographic || tangent.is_some();
                    assert_eq!(
                        r.requires_visibility_rebuild_for_navigation(),
                        view_sensitive
                    );
                    let index = rows.len();
                    let a = self.out.join(format!("case{index}-actual.png"));
                    r.save_screenshot(&a).unwrap();
                    let mut reference = t;
                    reference.rotation_crs = RotationCrs::Portrayal;
                    reference.curve_tangent_bearing = None;
                    reference.rotation = expected;
                    c.clear_instructions();
                    c.add_instruction(DrawingInstruction::Text(reference));
                    r.begin_frame();
                    r.add_instructions_with_symbols(&mut c, None, None, None);
                    let b = self.out.join(format!("case{index}-reference.png"));
                    r.save_screenshot(&b).unwrap();
                    let a = image::open(a).unwrap().to_rgba8();
                    let b = image::open(b).unwrap().to_rgba8();
                    let changed = a.pixels().zip(b.pixels()).filter(|(a, b)| a != b).count();
                    let ink = a
                        .pixels()
                        .filter(|p| p[0] < 240 && p[1] < 240 && p[2] < 240)
                        .count();
                    assert!(ink > 100);
                    assert!(
                        changed <= 20,
                        "GPU independent text basis {name}: {changed}"
                    );
                    rows.push(serde_json::json!({"case":index,"projection":format!("{projection:?}"),"latitude":lat,"crs":name,"adapter_preserved":true,"serialization_preserved":true,"tangent_supplied_by_fixture":tangent,"rotation_degrees":actual,"geodesic_oracle_error_deg":error,"changed_pixels":changed,"ink_pixels":ink,"navigation_rebuild_required":view_sensitive}));
                }
            }
        }
        let mut c = RenderContext::new(Viewport::new(1000., 700.));
        let invalid = ferrite_lua::PortrayalResult::parse(
            "INVALID-TEXT-CRS",
            "Rotation:TypoCRS,45;AugmentedPoint:GeographicCRS,0,0;TextInstruction:bad",
            "",
        )
        .unwrap();
        assert!(ferrite_s101::convert_lua_results_for_cell(
            &[invalid],
            &cell,
            &pc,
            &mut c,
            7,
            "Day"
        )
        .is_err());
        assert!(c.raw_instructions().is_empty());
        assert!(flat_text_rotation(
            &TextInstruction::new("x".into(), WorldPoint::new(0., 0.))
                .with_rotation_crs(RotationCrs::Line),
            &c.scaler
        )
        .is_err());
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"rows":rows,"invalid_crs_rejected_before_mutation":true,"missing_line_tangent_rejected":true,"physical_input_verified":false,"complete_line_text_placement":false})).unwrap()).unwrap();
        e.exit();
    }
    fn window_event(&mut self, e: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            e.exit();
        }
    }
}
fn main() {
    let a = std::env::args().skip(1).collect::<Vec<_>>();
    let mut app = App {
        out: a[0].clone().into(),
        cell: a[1].clone().into(),
        pc: a[2].clone().into(),
    };
    EventLoop::new().unwrap().run_app(&mut app).unwrap();
}
