use anyhow::{ensure, Result};
use ferrite_kernel::{CoverageSample, CoverageSource, CoverageTile, GridGeometry, GridWindow};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_s102::{BathymetryCoverage, BathymetryPortrayal, DepthSettings};
struct Mock {
    g: GridGeometry,
}
impl CoverageSource for Mock {
    fn geometry(&self) -> &GridGeometry {
        &self.g
    }
    fn read_window(&self, w: GridWindow) -> Result<CoverageTile> {
        w.validate(&self.g)?;
        let mut samples = Vec::new();
        for y in w.row..w.row + w.height {
            for x in w.column..w.column + w.width {
                samples.push(CoverageSample {
                    value: if (x + y) % 3 == 0 {
                        None
                    } else {
                        Some(if (x + y) % 2 == 0 { 7. } else { 60. })
                    },
                    uncertainty: None,
                });
            }
        }
        Ok(CoverageTile { window: w, samples })
    }
}
fn check(c: &impl CoverageSource, p: &BathymetryPortrayal) -> Result<usize> {
    let g = c.geometry();
    let mut reconstructed = vec![0u8; g.width * g.height * 4];
    let mut count = 0;
    let edge = if g.width < 10 { 2 } else { 256 };
    for w in g.windows(edge, edge)? {
        let tile = p.raster_window(c, w, "fixture".into())?;
        ensure!(
            tile.width as usize == w.width && tile.height as usize == w.height,
            "tile size"
        );
        let left = if g.spacing_x > 0. {
            w.column
        } else {
            g.width - w.column - w.width
        };
        let top = if g.spacing_y < 0. {
            w.row
        } else {
            g.height - w.row - w.height
        };
        for y in 0..w.height {
            for x in 0..w.width {
                let dest = ((top + y) * g.width + left + x) * 4;
                let source = (y * w.width + x) * 4;
                reconstructed[dest..dest + 4].copy_from_slice(&tile.rgba[source..source + 4]);
            }
        }
        count += 1;
    }
    let full = p.raster_window(
        c,
        GridWindow {
            column: 0,
            row: 0,
            width: g.width,
            height: g.height,
        },
        "reference".into(),
    )?;
    ensure!(
        full.rgba == reconstructed,
        "Tile reconstruction differs from full coverage"
    );
    // Independent source-index colour oracle, not another tiled-image projection.
    for w in g.windows(g.width, 128)? {
        let tile = c.read_window(w)?;
        for (i, sample) in tile.samples.iter().enumerate() {
            let row = w.row + i / w.width;
            let col = i % w.width;
            let y = if g.spacing_y > 0. {
                g.height - 1 - row
            } else {
                row
            };
            let x = if g.spacing_x < 0. {
                g.width - 1 - col
            } else {
                col
            };
            ensure!(
                reconstructed[(y * g.width + x) * 4..(y * g.width + x) * 4 + 4]
                    == p.rgba(*sample)?,
                "Wrong source-node colour orientation"
            );
        }
    }
    Ok(count)
}
fn main() -> Result<()> {
    let root = std::env::args().nth(1).unwrap();
    let pc = PortrayalCatalogue::load(
        "Standards/S102-PC-3.0.0/S-102-Portrayal-Catalogue-3.0.0.20241216/PortrayalCatalog",
    )?;
    let p = BathymetryPortrayal::from_catalogue(&pc, "Day", DepthSettings::default())?;
    let mut real = Vec::new();
    for e in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("h5"))
        })
    {
        for c in BathymetryCoverage::open(e.path())? {
            let tiles = check(&c, &p)?;
            real.push(serde_json::json!({"file":e.path(),"tiles":tiles,"source_nodes":c.geometry().width*c.geometry().height}));
        }
    }
    for x in [-1., 1.] {
        for y in [-1., 1.] {
            check(
                &Mock {
                    g: GridGeometry {
                        width: 5,
                        height: 3,
                        origin_x: 1.,
                        origin_y: 20.,
                        spacing_x: x,
                        spacing_y: y,
                        horizontal_crs: 4326,
                    },
                },
                &p,
            )?;
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"real":real,"axis_orientation_fixtures":4})
        )?
    );
    Ok(())
}
