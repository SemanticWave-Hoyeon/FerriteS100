//! HDF5-to-GPU oracle for a source grid wider than the device texture limit.
#![allow(non_local_definitions)]
use anyhow::Result;
use ferrite_kernel::{CoverageSample, CoverageSource, GridGeometry, GridWindow};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{Color, RenderContext, ScreenPoint, Viewport};
use ferrite_s102::{hdf5, BathymetryCoverage, BathymetryPortrayal, DepthSettings};
use ferrite_wgpu::WgpuRenderer;
use hdf5::types::VarLenAscii;
use std::{path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
#[derive(hdf5::H5Type, Clone, Copy)]
#[repr(C)]
struct DepthValue {
    depth: f32,
    uncertainty: f32,
}
#[derive(hdf5::H5Type, Clone)]
#[repr(C)]
#[allow(non_snake_case)]
struct Definition {
    code: VarLenAscii,
    name: VarLenAscii,
    #[hdf5(rename = "uom.name")]
    unit: VarLenAscii,
    fillValue: VarLenAscii,
    datatype: VarLenAscii,
    lower: VarLenAscii,
    upper: VarLenAscii,
    closure: VarLenAscii,
}
impl Definition {
    fn for_code(code: &str) -> Self {
        let text = |s: &str| VarLenAscii::from_ascii(s).unwrap();
        let (name, unit, fill, datatype, lower, upper, closure) = match code {
            "uncertainty" => (
                code,
                "metres",
                "1000000",
                "H5T_FLOAT",
                "0",
                "",
                "geSemiInterval",
            ),
            "iD" => ("ID", "", "0", "H5T_INTEGER", "1", "", "geSemiInterval"),
            _ => (
                code,
                "metres",
                "1000000",
                "H5T_FLOAT",
                "-14",
                "11050",
                "closedInterval",
            ),
        };
        Self {
            code: text(code),
            name: text(name),
            unit: text(unit),
            fillValue: text(fill),
            datatype: text(datatype),
            lower: text(lower),
            upper: text(upper),
            closure: text(closure),
        }
    }
}
fn declarations(g: &hdf5::Group) {
    g.new_dataset::<VarLenAscii>()
        .shape(2)
        .create("featureCode")
        .unwrap()
        .write_raw(
            &["BathymetryCoverage", "QualityOfBathymetryCoverage"]
                .map(|s| VarLenAscii::from_ascii(s).unwrap()),
        )
        .unwrap();
    g.new_dataset::<Definition>()
        .shape(1)
        .create("QualityOfBathymetryCoverage")
        .unwrap()
        .write_raw(&[Definition::for_code("iD")])
        .unwrap();
}
fn attr<T: hdf5::H5Type>(g: &hdf5::Group, name: &str, value: T) {
    g.new_attr::<T>()
        .create(name)
        .unwrap()
        .write_scalar(&value)
        .unwrap();
}
fn text(g: &hdf5::Group, name: &str, value: &str) {
    attr(
        g,
        name,
        hdf5::types::VarLenAscii::from_ascii(value).unwrap(),
    );
}
// Signed orientations are generic kernel tests, not S102 product encodings.
struct OrientedFixture {
    coverage: BathymetryCoverage,
    grid: GridGeometry,
}
impl CoverageSource for OrientedFixture {
    fn geometry(&self) -> &GridGeometry {
        &self.grid
    }
    fn read_window(&self, window: GridWindow) -> Result<ferrite_kernel::CoverageTile> {
        window.validate(&self.grid)?;
        self.coverage.read_window(window)
    }
}
fn fixture(path: &std::path::Path, grid: GridGeometry) -> OrientedFixture {
    {
        let f = hdf5::File::create(path).unwrap();
        text(&f, "productSpecification", "INT.IHO.S-102.3.0.0");
        text(&f, "issueDate", "20261006");
        for (n, v) in [
            (
                "westBoundLongitude",
                grid.origin_x - grid.spacing_x.abs() * 0.5,
            ),
            (
                "eastBoundLongitude",
                grid.origin_x + (grid.width as f64 - 0.5) * grid.spacing_x.abs(),
            ),
            (
                "southBoundLatitude",
                grid.origin_y - grid.spacing_y.abs() * 0.5,
            ),
            (
                "northBoundLatitude",
                grid.origin_y + (grid.height as f64 - 0.5) * grid.spacing_y.abs(),
            ),
        ] {
            attr(&f, n, v as f32);
        }
        attr(&f, "horizontalCRS", 4326u32);
        attr(&f, "verticalCS", 6498u32);
        attr(&f, "verticalCoordinateBase", 2u8);
        attr(&f, "verticalDatum", 10u32);
        attr(&f, "verticalDatumReference", 1u8);
        let b = f.create_group("BathymetryCoverage").unwrap();
        b.new_dataset::<VarLenAscii>()
            .shape(2)
            .create("axisNames")
            .unwrap()
            .write_raw(&[
                VarLenAscii::from_ascii("Latitude").unwrap(),
                VarLenAscii::from_ascii("Longitude").unwrap(),
            ])
            .unwrap();
        attr(&b, "horizontalPositionUncertainty", -1f32);
        attr(&b, "verticalUncertainty", -1f32);
        attr(&b, "dataCodingFormat", 2u8);
        attr(&b, "dimension", 2u8);
        attr(&b, "commonPointRule", 2u8);
        attr(&b, "interpolationType", 1u8);
        attr(&b, "numInstances", 1u8);
        attr(&b, "dataOffsetCode", 5u8);
        attr(&b, "sequencingRule.type", 1u8);
        text(&b, "sequencingRule.scanDirection", "Longitude, Latitude");
        let g = b.create_group("BathymetryCoverage.01").unwrap();
        attr(&g, "numGRP", 1u32);
        text(&g, "startSequence", "0,0");
        attr(&g, "numPointsLongitudinal", grid.width as u32);
        attr(&g, "numPointsLatitudinal", grid.height as u32);
        attr(&g, "gridOriginLongitude", grid.origin_x);
        attr(&g, "gridOriginLatitude", grid.origin_y);
        attr(&g, "gridSpacingLongitudinal", grid.spacing_x.abs());
        attr(&g, "gridSpacingLatitudinal", grid.spacing_y.abs());
        for (n, v) in [
            (
                "westBoundLongitude",
                grid.origin_x - grid.spacing_x.abs() * 0.5,
            ),
            (
                "eastBoundLongitude",
                grid.origin_x + (grid.width as f64 - 0.5) * grid.spacing_x.abs(),
            ),
            (
                "southBoundLatitude",
                grid.origin_y - grid.spacing_y.abs() * 0.5,
            ),
            (
                "northBoundLatitude",
                grid.origin_y + (grid.height as f64 - 0.5) * grid.spacing_y.abs(),
            ),
        ] {
            attr(&g, n, v as f32);
        }
        let data = g.create_group("Group_001").unwrap();
        attr(&data, "minimumDepth", 7f32);
        attr(&data, "maximumDepth", 60f32);
        attr(&data, "minimumUncertainty", 0.5f32);
        attr(&data, "maximumUncertainty", 0.5f32);
        text(&data, "timePoint", "00010101T000000Z");
        let values: Vec<_> = (0..grid.height)
            .flat_map(|y| {
                (0..grid.width).map(move |x| DepthValue {
                    depth: if (x + y) % 3 == 0 {
                        1e6
                    } else if (x + y) % 2 == 0 {
                        7.
                    } else {
                        60.
                    },
                    uncertainty: if (x + y) % 3 == 0 { 1e6 } else { 0.5 },
                })
            })
            .collect();
        data.new_dataset::<DepthValue>()
            .shape([grid.height, grid.width])
            .chunk([3, 511.min(grid.width)])
            .deflate(1)
            .create("values")
            .unwrap()
            .write_raw(&values)
            .unwrap();
        let info = f.create_group("Group_F").unwrap();
        declarations(&info);
        let definitions: Vec<_> = ["depth", "uncertainty"]
            .iter()
            .map(|name| Definition::for_code(name))
            .collect();
        info.new_dataset::<Definition>()
            .shape(2)
            .create("BathymetryCoverage")
            .unwrap()
            .write_raw(&definitions)
            .unwrap();
    }
    OrientedFixture {
        coverage: BathymetryCoverage::open(path).unwrap().remove(0),
        grid,
    }
}
struct Smoke {
    mercator: bool,
    out: PathBuf,
}
impl ApplicationHandler for Smoke {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Larger-than-device coverage oracle")
                    .with_visible(false)
                    .with_active(false)
                    .with_inner_size(PhysicalSize::new(1200, 800)),
            )
            .unwrap(),
        );
        assert!(!w.is_visible().unwrap_or(true));
        assert!(!w.has_focus());
        let size = w.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(w.clone())).unwrap();
        r.background_color = Color::WHITE;
        let limit = r.raster_texture_limit() as usize;
        let width = limit + 1;
        let height = if self.mercator { 129 } else { 9 };
        let pc = PortrayalCatalogue::load(
            "Standards/S102-PC-3.0.0/S-102-Portrayal-Catalogue-3.0.0.20241216/PortrayalCatalog",
        )
        .unwrap();
        let p = BathymetryPortrayal::from_catalogue(&pc, "Day", DepthSettings::default()).unwrap();
        std::fs::create_dir_all(&self.out).unwrap();
        let mut cases = Vec::new();
        for sx in [-1., 1.] {
            for sy in [-1., 1.] {
                let source = fixture(
                    &self.out.join(format!("source-{sx}-{sy}.H5")),
                    GridGeometry {
                        width,
                        height,
                        origin_x: -2.,
                        origin_y: if self.mercator { 75. } else { 48. },
                        spacing_x: sx * (if self.mercator { 1. } else { 0.001 }) / width as f64,
                        spacing_y: sy * (if self.mercator { 0.003 } else { 0.00005 }),
                        horizontal_crs: 4326,
                    },
                );
                let whole = GridWindow {
                    column: 0,
                    row: 0,
                    width,
                    height,
                };
                let mut ctx =
                    RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
                let layer = p.raster_window(&source, whole, "oversized".into()).unwrap();
                if self.mercator {
                    ctx.scaler
                        .set_projection(ferrite_render::FlatProjection::EllipsoidalMercator);
                }
                ctx.set_bounds(layer.bounds);
                r.clear_raster_layers();
                let error = r
                    .add_raster_layer(layer, &ctx.scaler)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("texture limit"), "Unexpected error: {error}");
                for zoom in [0.5f32, 1., 2.] {
                    r.reset_pan_offset();
                    r.begin_frame();
                    r.add_instructions(&mut ctx);
                    r.clear_raster_layers();
                    r.set_pan_offset(17.25, -11.5);
                    r.set_gpu_zoom(zoom, size.width as f32 / 2., size.height as f32 / 2.);
                    let mut counts = Vec::new();
                    let mut images = Vec::new();
                    let mut probes = 0;
                    let mut missing = 0;
                    let mut valid = 0;
                    for edge in [2048usize.min(limit), 511usize.min(limit)] {
                        let mut tiles = 0;
                        r.raster_batch(&ctx.scaler, true, |upload| -> Result<()> {
                            for window in source.geometry().windows(edge, edge)? {
                                upload(p.raster_window(&source, window, "large-tile".into())?)?;
                                tiles += 1;
                            }
                            Ok(())
                        })
                        .unwrap();
                        counts.push(tiles);
                        let path = self.out.join(format!("{sx}-{sy}-{zoom}-{edge}.png"));
                        r.save_screenshot(&path).unwrap();
                        let image = image::open(path).unwrap().to_rgb8();
                        // Independent inverse geographic query at physical pixel centres; avoid
                        // cell boundaries where f32 rounding makes exact ties indeterminate.
                        for py in (50..size.height - 50).step_by(29) {
                            for px in (50..size.width - 50).step_by(31) {
                                let world = ctx.scaler.screen_to_world(ScreenPoint::new(
                                    (px as f32 + 0.5 - size.width as f32 / 2.) / zoom
                                        + size.width as f32 / 2.
                                        - 17.25,
                                    (py as f32 + 0.5 - size.height as f32 / 2.) / zoom
                                        + size.height as f32 / 2.
                                        + 11.5,
                                ));
                                let cx = (world.x - source.geometry().origin_x)
                                    / source.geometry().spacing_x
                                    + 0.5;
                                let cy = (world.y - source.geometry().origin_y)
                                    / source.geometry().spacing_y
                                    + 0.5;
                                if cx < 0.
                                    || cy < 0.
                                    || cx >= width as f64
                                    || cy >= height as f64
                                    || cx.fract() < 0.05
                                    || cx.fract() > 0.95
                                    || cy.fract() < 0.05
                                    || cy.fract() > 0.95
                                {
                                    continue;
                                }
                                let column = cx.floor() as usize;
                                let row = cy.floor() as usize;
                                let value = if (column + row) % 3 == 0 {
                                    None
                                } else {
                                    Some(if (column + row) % 2 == 0 { 7. } else { 60. })
                                };
                                let rgba = p
                                    .rgba(CoverageSample {
                                        value,
                                        uncertainty: None,
                                    })
                                    .unwrap();
                                let expected = if rgba[3] == 0 {
                                    missing += 1;
                                    [255, 255, 255]
                                } else {
                                    valid += 1;
                                    [rgba[0], rgba[1], rgba[2]]
                                };
                                assert_eq!(image.get_pixel(px,py).0,expected,"Large grid source {column},{row} at {px},{py}, signs {sx},{sy}, zoom {zoom}, edge {edge}");
                                probes += 1;
                            }
                        }
                        images.push(image);
                    }
                    let changed = images[0]
                        .pixels()
                        .zip(images[1].pixels())
                        .filter(|(a, b)| a != b)
                        .count();
                    assert_eq!(
                        changed, 0,
                        "Different tile sizes must select same source cells"
                    );
                    assert!(probes > 0 && valid > 0 && missing > 0);
                    r.render().unwrap();
                    let after = self.out.join(format!("{sx}-{sy}-{zoom}-surface.png"));
                    r.save_screenshot(&after).unwrap();
                    assert!(images[1] == image::open(after).unwrap().to_rgb8());
                    cases.push(serde_json::json!({"axis_signs":[sx,sy],"zoom":zoom,"source_width":width,"source_height":height,"projection":format!("{:?}",ctx.scaler.projection()),"tile_counts":counts,"changed_pixels":changed,"independent_pixel_probes":probes,"missing_probes":missing,"valid_probes":valid}));
                }
            }
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"source":"synthetic S-102 HDF5 fixture; not a certified exchange set","gpu_texture_limit":limit,"cases":cases,"native_os_input_verified":false,"window_visible":false,"window_focused":false})).unwrap()).unwrap();
        assert!(!w.is_visible().unwrap_or(true));
        assert!(!w.has_focus());
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    EventLoop::new()
        .unwrap()
        .run_app(&mut Smoke {
            out: std::env::args().nth(1).unwrap().into(),
            mercator: std::env::args().nth(2).as_deref() == Some("--mercator"),
        })
        .unwrap();
}
