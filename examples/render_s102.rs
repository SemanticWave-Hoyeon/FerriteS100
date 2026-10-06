use anyhow::Result;
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_s102::{BathymetryCoverage, BathymetryPortrayal, DepthSettings};
use std::path::PathBuf;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let output = PathBuf::from(&args[2]);
    std::fs::create_dir_all(&output)?;
    let pc = PortrayalCatalogue::load(
        "Standards/S102-PC-3.0.0/S-102-Portrayal-Catalogue-3.0.0.20241216/PortrayalCatalog",
    )?;
    let portrayal = BathymetryPortrayal::from_catalogue(&pc, "Day", DepthSettings::default())?;
    std::fs::write(
        output.join("official-coverage-instructions.txt"),
        &portrayal.instructions,
    )?;
    for e in walkdir::WalkDir::new(&args[1])
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path()
                    .extension()
                    .is_some_and(|s| s.eq_ignore_ascii_case("h5"))
        })
    {
        for c in BathymetryCoverage::open(e.path())? {
            let raster = portrayal.raster_layer(&c, c.instance_name.clone())?;
            let name = format!(
                "{}-{}.png",
                e.path().file_stem().unwrap().to_string_lossy(),
                c.instance_name
            );
            image::RgbaImage::from_raw(raster.width, raster.height, raster.rgba)
                .unwrap()
                .save(output.join(name))?;
        }
    }
    Ok(())
}
