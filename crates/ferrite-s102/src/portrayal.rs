//! Evaluate the official PC coverage rule and colour its exact numeric intervals.
use crate::BathymetryCoverage;
use anyhow::{ensure, Context, Result};
use ferrite_kernel::{CoverageSample, CoverageSource, GridWindow, NumericCoverageSource};
use ferrite_lua::{DrawingCommand, LookupEntry, LuaSession};
use ferrite_portrayal_catalog::{ColorProfile, PortrayalCatalogue};
use ferrite_render::{GeoBounds, RasterDrawOrder, RasterGrid, RasterLayer};
#[derive(Debug, Clone, Copy)]
pub struct DepthSettings {
    pub safety_contour: f64,
    pub shallow_contour: f64,
    pub deep_contour: f64,
    pub four_shades: bool,
}
impl Default for DepthSettings {
    fn default() -> Self {
        Self {
            safety_contour: 30.0,
            shallow_contour: 2.0,
            deep_contour: 30.0,
            four_shades: false,
        }
    }
}
pub struct BathymetryPortrayal {
    entries: Vec<LookupEntry>,
    profile: ColorProfile,
    pub instructions: String,
    pub draw_order: RasterDrawOrder,
    pub viewing_groups: Vec<u32>,
}
impl BathymetryPortrayal {
    pub fn from_catalogue(
        pc: &PortrayalCatalogue,
        profile: &str,
        settings: DepthSettings,
    ) -> Result<Self> {
        ensure!(
            [
                settings.safety_contour,
                settings.shallow_contour,
                settings.deep_contour
            ]
            .iter()
            .all(|v| v.is_finite()),
            "Non-finite contour setting"
        );
        ensure!(
            settings.shallow_contour >= 0.0
                && settings.shallow_contour <= settings.safety_contour
                && settings.safety_contour <= settings.deep_contour,
            "Contour settings must be 0 <= shallow <= safety <= deep"
        );
        let session = LuaSession::new()?;
        let lua = session.lua();
        let script = std::fs::read(pc.root_path.join("Rules/BathymetryCoverage.lua"))?;
        lua.load(script.as_slice())
            .set_name("@S102-BathymetryCoverage")
            .exec()?;
        let portrayal = lua.create_table()?;
        portrayal.set("instructions", lua.create_table()?)?;
        portrayal.set(
            "AddInstructions",
            lua.load("return function(self,s) table.insert(self.instructions,s) end")
                .eval::<ferrite_lua::mlua::Function>()?,
        )?;
        let params = lua.create_table()?;
        params.set("FourShades", settings.four_shades)?;
        params.set("ShallowContour", settings.shallow_contour)?;
        params.set("SafetyContour", settings.safety_contour)?;
        params.set("DeepContour", settings.deep_contour)?;
        let rule: ferrite_lua::mlua::Function = lua.globals().get("BathymetryCoverage")?;
        rule.call::<()>((lua.create_table()?, portrayal.clone(), params))?;
        let list: ferrite_lua::mlua::Table = portrayal.get("instructions")?;
        let instructions = list
            .sequence_values::<String>()
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join(";");
        let parsed = ferrite_lua::parse_instruction_string("BathymetryCoverage", &instructions)?;
        let (entries, visibility) = parsed
            .commands
            .into_iter()
            .find_map(|c| match c {
                DrawingCommand::CoverageFill {
                    attribute_code,
                    lookup_entries,
                    visibility,
                    ..
                } if attribute_code == "depth" => Some((lookup_entries, visibility)),
                _ => None,
            })
            .context("PC did not emit CoverageFill:depth")?;
        ensure!(!entries.is_empty(), "PC emitted empty depth lookup");
        ensure!(
            !visibility.viewing_groups.is_empty() || !visibility.named_viewing_groups.is_empty(),
            "PC coverage has no viewing group"
        );
        let draw_order = RasterDrawOrder {
            stage: ferrite_kernel::CompositionStage::Overlay,
            display_plane: match visibility.display_plane.reference() {
                Some(name) => ferrite_render::DisplayPlane::from_catalogue_order(
                    pc.display_planes.resolve(name)?,
                ),
                None => ferrite_render::DisplayPlane::default(),
            },
            priority: visibility.drawing_priority,
        };
        let viewing_groups = pc
            .viewing_groups
            .resolve_drawing_groups(&visibility.viewing_groups, &visibility.named_viewing_groups)?
            .into_owned();
        let profile = pc
            .color_profiles
            .profiles
            .get(profile)
            .with_context(|| format!("Missing colour profile {profile}"))?
            .clone();
        for e in &entries {
            ensure!(
                e.end_color.is_none(),
                "CIE xyL coverage colour ramps not yet supported"
            );
            ensure!(
                e.color_token
                    .as_ref()
                    .and_then(|c| profile.get_srgb(c))
                    .is_some(),
                "Unresolved coverage colour"
            );
        }
        Ok(Self {
            entries,
            profile,
            draw_order,
            viewing_groups,
            instructions,
        })
    }
    /// Compatibility export of the entire coverage. The app streams raster_window tiles.
    pub fn raster_layer(&self, coverage: &BathymetryCoverage, id: String) -> Result<RasterLayer> {
        let g = coverage.geometry();
        self.raster_window(
            coverage,
            GridWindow {
                column: 0,
                row: 0,
                width: g.width,
                height: g.height,
            },
            id,
        )
    }
    /// Staged original-candidate export. The renderer requires an independently
    /// qualified frame before it can commit this packed material to a scene.
    pub fn continuous_raster_window(
        &self,
        coverages: &[BathymetryCoverage],
        corrections: crate::continuous::ConstantDatumAdjustments,
        window: GridWindow,
        id: String,
        max_texture_dimension: u32,
    ) -> Result<ferrite_render::ContinuousRasterLayer> {
        let packet =
            crate::continuous::ContinuousDepthTile::capture(coverages, corrections, window, self)?;
        let atlas = packet.atlas(max_texture_dimension)?;
        ferrite_render::ContinuousRasterLayer::new(RasterLayer {
            draw_order: self.draw_order,
            viewing_groups: self.viewing_groups.clone(),
            id,
            bounds: packet.bounds,
            width: atlas.width,
            height: atlas.height,
            rgba: atlas.bytes,
            grid: Some(packet.grid),
        })
    }
    /// Read only this source window, oriented west-to-east and north-to-south.
    pub fn raster_window(
        &self,
        coverage: &(impl NumericCoverageSource + ?Sized),
        window: GridWindow,
        id: String,
    ) -> Result<RasterLayer> {
        ensure!(!coverage.requires_spatial_mask(),"Geometric domain portrayal requires continuous clipping; centroid flattening is unsupported");
        let g = coverage.numeric_geometry();
        window.validate(g)?;
        ensure!(
            g.horizontal_crs == 4326,
            "Map portrayal currently requires EPSG:4326"
        );
        let width = u32::try_from(window.width)?;
        let height = u32::try_from(window.height)?;
        let size = window
            .width
            .checked_mul(window.height)
            .and_then(|n| n.checked_mul(4))
            .context("Raster size overflow")?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(size)
            .context("Cannot allocate coverage tile")?;
        rgba.resize(size, 0);
        let mut visited = 0usize;
        coverage.visit_window_values(window, &mut |index, value| {
            ensure!(
                index == visited && index < window.width * window.height,
                "Numeric coverage visitor violated row-major sample contract"
            );
            visited += 1;
            let source_row = index / window.width;
            let source_column = index % window.width;
            let dest_row = if g.spacing_y > 0. {
                window.height - 1 - source_row
            } else {
                source_row
            };
            let dest_column = if g.spacing_x > 0. {
                source_column
            } else {
                window.width - 1 - source_column
            };
            let dest = (dest_row * window.width + dest_column) * 4;
            rgba[dest..dest + 4].copy_from_slice(&self.rgba_value(value)?);
            Ok(())
        })?;
        ensure!(
            visited == window.width * window.height,
            "Numeric coverage visitor omitted samples"
        );
        let x0 = g.origin_x + (window.column as f64 - 0.5) * g.spacing_x;
        let x1 = g.origin_x + ((window.column + window.width) as f64 - 0.5) * g.spacing_x;
        let y0 = g.origin_y + (window.row as f64 - 0.5) * g.spacing_y;
        let y1 = g.origin_y + ((window.row + window.height) as f64 - 0.5) * g.spacing_y;
        let gx0 = g.origin_x - g.spacing_x * 0.5;
        let gx1 = g.origin_x + (g.width as f64 - 0.5) * g.spacing_x;
        let gy0 = g.origin_y - g.spacing_y * 0.5;
        let gy1 = g.origin_y + (g.height as f64 - 0.5) * g.spacing_y;
        let column = if g.spacing_x > 0. {
            window.column
        } else {
            g.width - window.column - window.width
        };
        let row = if g.spacing_y < 0. {
            window.row
        } else {
            g.height - window.row - window.height
        };
        Ok(RasterLayer {
            viewing_groups: self.viewing_groups.clone(),
            draw_order: self.draw_order,
            id,
            width,
            height,
            rgba,
            bounds: GeoBounds::new(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)),
            grid: Some(RasterGrid {
                bounds: GeoBounds::new(gx0.min(gx1), gy0.min(gy1), gx0.max(gx1), gy0.max(gy1)),
                width: u32::try_from(g.width)?,
                height: u32::try_from(g.height)?,
                column: u32::try_from(column)?,
                row: u32::try_from(row)?,
            }),
        })
    }
    pub fn rgba(&self, sample: CoverageSample) -> Result<[u8; 4]> {
        self.rgba_value(sample.value.map(f64::from))
    }
    /// Adjusted depths retain their precision through the official PC interval lookup.
    pub fn rgba_value(&self, value: Option<f64>) -> Result<[u8; 4]> {
        let Some(depth) = value else {
            return Ok([0, 0, 0, 0]);
        };
        ensure!(depth.is_finite(), "Non-finite portrayal depth");
        let entry = self
            .entries
            .iter()
            .find(|e| e.closure.contains(depth, e.range_min, e.range_max))
            .context("Depth outside PC lookup intervals")?;
        let rgb = self
            .profile
            .get_srgb(entry.color_token.as_deref().unwrap())
            .context("Missing coverage colour")?;
        Ok([
            rgb.r,
            rgb.g,
            rgb.b,
            ((1.0 - entry.transparency.clamp(0.0, 1.0)) * 255.0).round() as u8,
        ])
    }
}

#[cfg(test)]
mod numeric_tests {
    use super::*;
    use ferrite_kernel::{GridGeometry, IntervalClosure};
    use ferrite_portrayal_catalog::{ColorDefinition, SrgbColor};
    fn portrayal() -> BathymetryPortrayal {
        let mut profile = ColorProfile::default();
        for (token, rgb) in [("SHALLOW", [255, 0, 0]), ("DEEP", [0, 0, 255])] {
            profile.colors.insert(
                token.into(),
                ColorDefinition {
                    token: token.into(),
                    srgb: Some(SrgbColor::new(rgb[0], rgb[1], rgb[2])),
                    cie: None,
                },
            );
        }
        let entries = [
            ("SHALLOW", IntervalClosure::Less, 30.),
            ("DEEP", IntervalClosure::GreaterEqual, 30.),
        ]
        .into_iter()
        .map(|(token, closure, limit)| LookupEntry {
            label: token.into(),
            closure,
            range_min: limit,
            range_max: limit,
            color_token: Some(token.into()),
            transparency: 0.,
            end_color: None,
            pen_width: 0.,
            symbol: None,
            text: None,
        })
        .collect();
        BathymetryPortrayal {
            entries,
            profile,
            instructions: String::new(),
            draw_order: RasterDrawOrder::default(),
            viewing_groups: vec![1],
        }
    }
    struct Numeric {
        g: GridGeometry,
        malformed: bool,
    }
    impl NumericCoverageSource for Numeric {
        fn numeric_geometry(&self) -> &GridGeometry {
            &self.g
        }
        fn visit_window_values(
            &self,
            _: GridWindow,
            visitor: &mut dyn FnMut(usize, Option<f64>) -> Result<()>,
        ) -> Result<()> {
            visitor(if self.malformed { 1 } else { 0 }, Some(30. - 1e-8))?;
            visitor(1, Some(30.))?;
            Ok(())
        }
    }
    #[test]
    fn geometric_sources_cannot_silently_enter_centroid_raster_path() {
        struct Masked(GridGeometry);
        impl NumericCoverageSource for Masked {
            fn numeric_geometry(&self) -> &GridGeometry {
                &self.0
            }
            fn requires_spatial_mask(&self) -> bool {
                true
            }
            fn visit_window_values(
                &self,
                _: GridWindow,
                _: &mut dyn FnMut(usize, Option<f64>) -> Result<()>,
            ) -> Result<()> {
                panic!("Geometric source must be rejected before reading/flattening values");
            }
        }
        let source = Masked(GridGeometry {
            width: 1,
            height: 1,
            origin_x: 0.5,
            origin_y: 0.5,
            spacing_x: 1.,
            spacing_y: 1.,
            horizontal_crs: 4326,
        });
        let result = portrayal().raster_window(
            &source,
            GridWindow {
                column: 0,
                row: 0,
                width: 1,
                height: 1,
            },
            "masked".into(),
        );
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("continuous clipping"));
    }
    #[test]
    fn adjusted_f64_depth_is_classified_without_f32_rounding() {
        let p = portrayal();
        assert_eq!(p.rgba_value(Some(30. - 1e-8)).unwrap(), [255, 0, 0, 255]);
        assert_eq!(
            p.rgba(CoverageSample {
                value: Some((30. - 1e-8) as f32),
                uncertainty: None
            })
            .unwrap(),
            [0, 0, 255, 255]
        );
        let mut numeric = Numeric {
            g: GridGeometry {
                width: 2,
                height: 1,
                origin_x: 0.5,
                origin_y: 0.5,
                spacing_x: 1.,
                spacing_y: 1.,
                horizontal_crs: 4326,
            },
            malformed: false,
        };
        let w = GridWindow {
            column: 0,
            row: 0,
            width: 2,
            height: 1,
        };
        let layer = p.raster_window(&numeric, w, "test".into()).unwrap();
        assert_eq!(layer.rgba, [255, 0, 0, 255, 0, 0, 255, 255]);
        numeric.malformed = true;
        assert!(p.raster_window(&numeric, w, "test".into()).is_err());
        assert!(p.rgba_value(Some(f64::NAN)).is_err());
    }
}
