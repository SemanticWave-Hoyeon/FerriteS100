//! Evaluate the official PC coverage rule and colour its exact numeric intervals.
use crate::BathymetryCoverage;
use anyhow::{ensure, Context, Result};
use ferrite_kernel::{
    CoverageSample, CoverageSource, GridWindow, IntervalClosure, NumericCoverageSource,
};
use ferrite_lua::{DrawingCommand, LookupEntry, LuaSession};
use ferrite_portrayal_catalog::{BoundPortrayalCatalogue, ColorProfile, PortrayalCatalogue};
use ferrite_render::{GeoBounds, RasterDrawOrder, RasterGrid, RasterLayer};
use std::{path::Path, sync::Arc};
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
/// Exact retained input owner of a successfully evaluated bound S102 rule.
/// Construction is private: a digest or UI profile label cannot mint this owner.
pub struct BoundBathymetryEvaluation {
    catalogue: Arc<BoundPortrayalCatalogue>,
    rule: Arc<[u8]>,
    profile_id: String,
    settings: DepthSettings,
    draw_order: RasterDrawOrder,
    viewing_groups: Vec<u32>,
}
impl BoundBathymetryEvaluation {
    pub fn catalogue(&self) -> &Arc<BoundPortrayalCatalogue> {
        &self.catalogue
    }
    pub fn executed_rule(&self) -> &[u8] {
        &self.rule
    }
    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }
    pub fn settings(&self) -> DepthSettings {
        self.settings
    }
}
/// Producer-sealed regular tile. Pixel/lattice/order fields cannot be edited or
/// rewrapped by a caller after the producer has attached its evaluation owner.
/// This proves PC evaluation provenance, NOT authenticated HDF/datum authority.
pub struct BoundBathymetryMaterial {
    layer: RasterLayer,
    evaluation: Arc<BoundBathymetryEvaluation>,
}
impl BoundBathymetryMaterial {
    pub fn bounds(&self) -> GeoBounds {
        self.layer.bounds
    }
    /// Consuming extraction is for the GPU upload boundary. There is deliberately
    /// no public constructor from an arbitrary layer plus evaluation owner.
    pub fn into_gpu_parts(self) -> (RasterLayer, Arc<BoundBathymetryEvaluation>) {
        (self.layer, self.evaluation)
    }
}
struct ResolvedDepthEntry {
    closure: IntervalClosure,
    range_min: f64,
    range_max: f64,
    rgba: [u8; 4],
}

pub struct BathymetryPortrayal {
    bound_evaluation: Option<Arc<BoundBathymetryEvaluation>>,
    entries: Vec<ResolvedDepthEntry>,
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
        Self::validate_settings(settings)?;
        let session = LuaSession::new()?;
        // Compatibility path remains explicitly unbound, including its live read.
        let script = std::fs::read(pc.root_path.join("Rules/BathymetryCoverage.lua"))?;
        Self::evaluate_rule(pc, profile, settings, &script, session)
    }
    /// Parse/profile/rule execution all use the same immutable captured PC owner.
    pub fn from_bound_catalogue(
        pc: Arc<BoundPortrayalCatalogue>,
        profile: &str,
        settings: DepthSettings,
    ) -> Result<Self> {
        Self::validate_settings(settings)?;
        let session = LuaSession::new()?;
        let rule = pc
            .sources()
            .read_relative(Path::new("Rules/BathymetryCoverage.lua"))?;
        let mut result = Self::evaluate_rule(&pc, profile, settings, &rule, session)?;
        result.bound_evaluation = Some(Arc::new(BoundBathymetryEvaluation {
            draw_order: result.draw_order,
            viewing_groups: result.viewing_groups.clone(),
            profile_id: result.profile.id.clone(),
            catalogue: pc,
            rule,
            settings,
        }));
        Ok(result)
    }
    pub fn bound_evaluation(&self) -> Option<&Arc<BoundBathymetryEvaluation>> {
        self.bound_evaluation.as_ref()
    }
    fn validate_settings(settings: DepthSettings) -> Result<()> {
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
        Ok(())
    }
    fn evaluate_rule(
        pc: &PortrayalCatalogue,
        profile: &str,
        settings: DepthSettings,
        script: &[u8],
        session: LuaSession,
    ) -> Result<Self> {
        let lua = session.lua();
        lua.load(script)
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
        let entries = Self::resolve_depth_entries(&entries, &profile)?;
        Ok(Self {
            bound_evaluation: None,
            entries,
            profile,
            draw_order,
            viewing_groups,
            instructions,
        })
    }
    /// Resolve immutable palette and alpha once, retaining rule-emitted order.
    fn resolve_depth_entries(
        entries: &[LookupEntry],
        profile: &ColorProfile,
    ) -> Result<Vec<ResolvedDepthEntry>> {
        entries
            .iter()
            .map(|e| {
                ensure!(
                    e.end_color.is_none(),
                    "CIE xyL coverage colour ramps not yet supported"
                );
                let rgb = e
                    .color_token
                    .as_ref()
                    .and_then(|c| profile.get_srgb(c))
                    .context("Unresolved coverage colour")?;
                Ok(ResolvedDepthEntry {
                    closure: e.closure,
                    range_min: e.range_min,
                    range_max: e.range_max,
                    rgba: [
                        rgb.r,
                        rgb.g,
                        rgb.b,
                        ((1.0 - e.transparency.clamp(0.0, 1.0)) * 255.0).round() as u8,
                    ],
                })
            })
            .collect()
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
    /// Sealed original regular material; IC transformation is intentionally not
    /// available through this method. A validated IC-specific producer must be
    /// added before the App switches any IC-governed publication to this path.
    pub fn bound_raster_window(
        &self,
        coverage: &(impl NumericCoverageSource + ?Sized),
        window: GridWindow,
        id: String,
    ) -> Result<BoundBathymetryMaterial> {
        let evaluation = self
            .bound_evaluation
            .as_ref()
            .context("Unbound portrayal cannot produce a bound tile")?;
        ensure!(
            self.draw_order == evaluation.draw_order
                && self.viewing_groups == evaluation.viewing_groups,
            "Bound portrayal metadata was changed after evaluation"
        );
        let layer = self.raster_window(coverage, window, id)?;
        Ok(BoundBathymetryMaterial {
            layer,
            evaluation: Arc::clone(evaluation),
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
        Ok(entry.rgba)
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
        let entries: Vec<LookupEntry> = [
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
            bound_evaluation: None,
            entries: BathymetryPortrayal::resolve_depth_entries(&entries, &profile).unwrap(),
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

/// Display modes are resolved from the S-102 PC, independently of S-101 local handles.
#[derive(Debug, Clone, Copy)]
pub enum BathymetryDisplayPreset {
    Base,
    Standard,
    Other,
}

pub fn bathymetry_viewing_groups_for_preset(
    pc: &PortrayalCatalogue,
    preset: BathymetryDisplayPreset,
) -> Result<std::collections::HashSet<u32>> {
    ensure!(
        pc.product_id == "S-102",
        "Bathymetry display selection requires S-102 PC"
    );
    let normalized = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let aliases: &[&str] = match preset {
        BathymetryDisplayPreset::Base => &["base", "displaybase"],
        BathymetryDisplayPreset::Standard => &["standard", "standarddisplay"],
        BathymetryDisplayPreset::Other => &["other", "otherinformation"],
    };
    let mut modes = pc.display_modes.modes.values().filter(|m| {
        aliases.contains(&normalized(&m.name).as_str())
            || aliases.contains(&normalized(&m.id).as_str())
    });
    let mode = modes
        .next()
        .with_context(|| format!("S-102 catalogue has no {preset:?} display mode"))?;
    ensure!(
        modes.next().is_none(),
        "Ambiguous S-102 {preset:?} display mode"
    );
    let mut groups = std::collections::HashSet::new();
    for id in &mode.viewing_group_layers {
        let layer = pc.viewing_group_layers.get(id).with_context(|| {
            format!(
                "S-102 display mode {} references missing layer {id}",
                mode.id
            )
        })?;
        groups.extend(layer.viewing_group_ids.iter().copied());
    }
    groups.extend(pc.foundation_mode.iter().copied());
    for group in &groups {
        ensure!(
            pc.viewing_groups.get(*group).is_some(),
            "S-102 selection references unknown group {group}"
        );
    }
    Ok(groups)
}

#[cfg(test)]
mod display_selection_tests {
    use super::*;
    fn delivered_pc() -> PortrayalCatalogue {
        PortrayalCatalogue::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Catalogues/PC/S-102"),
        )
        .unwrap()
    }
    #[test]
    fn delivered_depth_group_is_enabled_in_each_mode() {
        let pc = delivered_pc();
        for preset in [
            BathymetryDisplayPreset::Base,
            BathymetryDisplayPreset::Standard,
            BathymetryDisplayPreset::Other,
        ] {
            let groups = bathymetry_viewing_groups_for_preset(&pc, preset).unwrap();
            assert_eq!(groups, std::collections::HashSet::from([13030]));
            assert!(ferrite_render::raster_groups_visible(
                &[13030],
                Some(&groups)
            ));
        }
    }
    #[test]
    fn raster_local_handles_do_not_depend_on_s101_handles() {
        let mut pc = delivered_pc();
        let mut group = pc.viewing_groups.groups.remove(&13030).unwrap();
        group.id = 42;
        pc.viewing_groups.groups.insert(42, group);
        pc.foundation_mode = vec![42];
        for layer in pc.viewing_group_layers.layers.values_mut() {
            layer.viewing_group_ids = vec![42];
        }
        let own = bathymetry_viewing_groups_for_preset(&pc, BathymetryDisplayPreset::Base).unwrap();
        let unrelated_s101 = std::collections::HashSet::from([13030, 21010]);
        assert!(ferrite_render::raster_groups_visible(&[42], Some(&own)));
        assert!(!ferrite_render::raster_groups_visible(
            &[42],
            Some(&unrelated_s101)
        ));
    }
    #[test]
    fn invalid_mapping_and_wrong_product_fail_closed() {
        let mut pc = delivered_pc();
        pc.foundation_mode.push(999999);
        assert!(bathymetry_viewing_groups_for_preset(&pc, BathymetryDisplayPreset::Base).is_err());
        pc.product_id = "S-101".into();
        assert!(bathymetry_viewing_groups_for_preset(&pc, BathymetryDisplayPreset::Base).is_err());
    }
}

#[cfg(test)]
mod bound_evaluation_tests {
    use super::*;
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "ferrite-s102-bound-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&p).unwrap();
            for sub in ["Rules", "ColorProfiles"] {
                std::fs::create_dir(p.join(sub)).unwrap();
            }
            std::fs::write(p.join("portrayal_catalogue.xml"), "<portrayalCatalog productId='S-102' version='3.0.0'><displayPlanes><displayPlane id='UnderRadar' order='1'/></displayPlanes><viewingGroups><viewingGroup id='1'/></viewingGroups><foundationMode><viewingGroup>1</viewingGroup></foundationMode><viewingGroupLayers><viewingGroupLayer id='base'><viewingGroup>1</viewingGroup></viewingGroupLayer></viewingGroupLayers><displayModes><displayMode id='DisplayBase'><viewingGroupLayer>base</viewingGroupLayer></displayMode><displayMode id='StandardDisplay'><viewingGroupLayer>base</viewingGroupLayer></displayMode><displayMode id='OtherInformation'><viewingGroupLayer>base</viewingGroupLayer></displayMode></displayModes></portrayalCatalog>").unwrap();
            Self(p)
        }
        fn inputs(&self, red: u8, token: &str) {
            std::fs::write(self.0.join("ColorProfiles/colorProfile.xml"), format!("<colorProfile><palette name='Day'><item token='A'><srgb><red>{red}</red><green>0</green><blue>0</blue></srgb></item><item token='B'><srgb><red>0</red><green>0</green><blue>255</blue></srgb></item></palette></colorProfile>")).unwrap();
            std::fs::write(self.0.join("Rules/BathymetryCoverage.lua"), format!("function BathymetryCoverage(feature, portrayal, params) portrayal:AddInstructions('ViewingGroup:1;CoverageColor:{token},0;LookupEntry:All,0,,geSemiInterval;CoverageFill:depth') end")).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn bound_tile_retains_actual_producer_and_rejects_mutated_metadata_or_legacy() {
        struct Source(ferrite_kernel::GridGeometry);
        impl NumericCoverageSource for Source {
            fn numeric_geometry(&self) -> &ferrite_kernel::GridGeometry {
                &self.0
            }
            fn visit_window_values(
                &self,
                window: GridWindow,
                visitor: &mut dyn FnMut(usize, Option<f64>) -> Result<()>,
            ) -> Result<()> {
                window.validate(&self.0)?;
                visitor(0, Some(5.))?;
                visitor(1, None)
            }
        }
        let f = Fixture::new();
        f.inputs(255, "A");
        let pc = Arc::new(PortrayalCatalogue::load_bound(&f.0).unwrap());
        let mut p =
            BathymetryPortrayal::from_bound_catalogue(pc.clone(), "Day", DepthSettings::default())
                .unwrap();
        let source = Source(ferrite_kernel::GridGeometry {
            width: 2,
            height: 1,
            origin_x: -3.,
            origin_y: 50.,
            spacing_x: 0.1,
            spacing_y: 0.1,
            horizontal_crs: 4326,
        });
        let window = GridWindow {
            column: 0,
            row: 0,
            width: 2,
            height: 1,
        };
        let reference = p.raster_window(&source, window, "same".into()).unwrap();
        let (actual, owner) = p
            .bound_raster_window(&source, window, "same".into())
            .unwrap()
            .into_gpu_parts();
        assert_eq!(actual.rgba, reference.rgba);
        assert_eq!(actual.rgba, vec![255, 0, 0, 255, 0, 0, 0, 0]);
        assert!(Arc::ptr_eq(owner.catalogue(), &pc));
        let order = p.draw_order;
        p.draw_order.priority += 1;
        assert!(p
            .bound_raster_window(&source, window, "changed".into())
            .is_err());
        p.draw_order = order;
        p.viewing_groups.push(99);
        assert!(p
            .bound_raster_window(&source, window, "changed".into())
            .is_err());
        let legacy =
            BathymetryPortrayal::from_catalogue(&pc, "Day", DepthSettings::default()).unwrap();
        assert!(legacy
            .bound_raster_window(&source, window, "legacy".into())
            .is_err());
    }

    #[test]
    fn retained_rule_and_palette_are_the_actual_executed_inputs_after_live_mutation() {
        let f = Fixture::new();
        f.inputs(255, "A");
        let pc = Arc::new(PortrayalCatalogue::load_bound(&f.0).unwrap());
        let first = BathymetryPortrayal::from_bound_catalogue(
            Arc::clone(&pc),
            "Day",
            DepthSettings::default(),
        )
        .unwrap();
        f.inputs(17, "B");
        let held = BathymetryPortrayal::from_bound_catalogue(
            Arc::clone(&pc),
            "Day",
            DepthSettings::default(),
        )
        .unwrap();
        assert_eq!(first.instructions, held.instructions);
        assert_eq!(
            first.rgba_value(Some(5.)).unwrap(),
            held.rgba_value(Some(5.)).unwrap()
        );
        assert_eq!(held.rgba_value(Some(5.)).unwrap(), [255, 0, 0, 255]);
        let fresh_pc = Arc::new(PortrayalCatalogue::load_bound(&f.0).unwrap());
        let fresh = BathymetryPortrayal::from_bound_catalogue(
            Arc::clone(&fresh_pc),
            "Day",
            DepthSettings::default(),
        )
        .unwrap();
        assert_ne!(fresh.instructions, held.instructions);
        assert_eq!(fresh.rgba_value(Some(5.)).unwrap(), [0, 0, 255, 255]);
        assert_ne!(pc.source_digest(), fresh_pc.source_digest());
        let owner = held.bound_evaluation().unwrap();
        assert!(Arc::ptr_eq(owner.catalogue(), &pc));
        assert_eq!(
            owner.executed_rule(),
            pc.sources()
                .read_relative(Path::new("Rules/BathymetryCoverage.lua"))
                .unwrap()
                .as_ref()
        );
        assert_eq!(owner.profile_id(), "Day");
        // Colour-only mutation has observable effect on a fresh original capture too.
        f.inputs(17, "A");
        let colour = BathymetryPortrayal::from_bound_catalogue(
            Arc::new(PortrayalCatalogue::load_bound(&f.0).unwrap()),
            "Day",
            DepthSettings::default(),
        )
        .unwrap();
        assert_eq!(colour.rgba_value(Some(5.)).unwrap(), [17, 0, 0, 255]);
    }
    #[test]
    fn bound_missing_rule_or_profile_refuses_and_legacy_never_claims_bound_owner() {
        let f = Fixture::new();
        f.inputs(255, "A");
        let pc = Arc::new(PortrayalCatalogue::load_bound(&f.0).unwrap());
        assert!(BathymetryPortrayal::from_bound_catalogue(
            Arc::clone(&pc),
            "Unknown",
            DepthSettings::default()
        )
        .is_err());
        let legacy =
            BathymetryPortrayal::from_catalogue(&pc, "Day", DepthSettings::default()).unwrap();
        assert!(legacy.bound_evaluation().is_none());
        std::fs::remove_file(f.0.join("Rules/BathymetryCoverage.lua")).unwrap();
        assert!(
            BathymetryPortrayal::from_bound_catalogue(pc, "Day", DepthSettings::default()).is_ok()
        );
        let missing = Arc::new(PortrayalCatalogue::load_bound(&f.0).unwrap());
        assert!(BathymetryPortrayal::from_bound_catalogue(
            missing,
            "Day",
            DepthSettings::default()
        )
        .is_err());
    }
}

#[cfg(test)]
mod resolved_lookup_tests {
    use super::*;
    use ferrite_portrayal_catalog::{ColorDefinition, SrgbColor};

    fn profile() -> ColorProfile {
        let mut profile = ColorProfile::default();
        for (token, rgb) in [("A", [17, 129, 251]), ("B", [241, 33, 7])] {
            profile.colors.insert(
                token.into(),
                ColorDefinition {
                    token: token.into(),
                    srgb: Some(SrgbColor::new(rgb[0], rgb[1], rgb[2])),
                    cie: None,
                },
            );
        }
        profile
    }
    fn entry(closure: IntervalClosure, min: f64, max: f64, token: &str, alpha: f64) -> LookupEntry {
        LookupEntry {
            label: token.into(),
            closure,
            range_min: min,
            range_max: max,
            color_token: Some(token.into()),
            transparency: alpha,
            end_color: None,
            pen_width: 0.,
            symbol: None,
            text: None,
        }
    }
    // Independent reference: original per-pixel palette/alpha algorithm.
    fn original(
        entries: &[LookupEntry],
        profile: &ColorProfile,
        value: Option<f64>,
    ) -> Result<[u8; 4]> {
        let Some(depth) = value else {
            return Ok([0, 0, 0, 0]);
        };
        ensure!(depth.is_finite(), "Non-finite portrayal depth");
        let entry = entries
            .iter()
            .find(|e| e.closure.contains(depth, e.range_min, e.range_max))
            .context("Depth outside PC lookup intervals")?;
        let rgb = profile
            .get_srgb(entry.color_token.as_deref().unwrap())
            .context("Missing coverage colour")?;
        Ok([
            rgb.r,
            rgb.g,
            rgb.b,
            ((1.0 - entry.transparency.clamp(0., 1.)) * 255.).round() as u8,
        ])
    }
    fn prepared(entries: &[LookupEntry], profile: ColorProfile) -> BathymetryPortrayal {
        BathymetryPortrayal {
            bound_evaluation: None,
            entries: BathymetryPortrayal::resolve_depth_entries(entries, &profile).unwrap(),
            profile,
            instructions: String::new(),
            draw_order: RasterDrawOrder::default(),
            viewing_groups: vec![1],
        }
    }
    fn compare(actual: Result<[u8; 4]>, expected: Result<[u8; 4]>) {
        match (actual, expected) {
            (Ok(a), Ok(e)) => assert_eq!(a, e),
            (Err(a), Err(e)) => assert_eq!(a.to_string(), e.to_string()),
            _ => panic!("optimized/reference acceptance differs"),
        }
    }
    #[test]
    fn all_closures_boundaries_alpha_and_first_match_equal_original() {
        let closures = [
            IntervalClosure::Open,
            IntervalClosure::Closed,
            IntervalClosure::LeftClosed,
            IntervalClosure::RightClosed,
            IntervalClosure::Greater,
            IntervalClosure::GreaterEqual,
            IntervalClosure::Less,
            IntervalClosure::LessEqual,
        ];
        // Alpha keeps exact old clamp/round/cast semantics, including unusual parsed values.
        for closure in closures {
            for alpha in [
                -1.,
                0.,
                0.5,
                1.,
                2.,
                f64::NAN,
                f64::NEG_INFINITY,
                f64::INFINITY,
            ] {
                let entries = vec![
                    entry(closure, 0., 30., "A", alpha),
                    entry(IntervalClosure::Closed, 0., 30., "B", 0.25),
                ];
                let profile = profile();
                let p = prepared(&entries, profile.clone());
                for value in [
                    None,
                    Some(-f64::MAX),
                    Some(-1.),
                    Some(-0.),
                    Some(0.),
                    Some(f64::from_bits(1)),
                    Some(30. - 1e-8),
                    Some(30.),
                    Some(f64::from_bits(30f64.to_bits() + 1)),
                    Some(f64::MAX),
                    Some(f64::NAN),
                    Some(f64::NEG_INFINITY),
                    Some(f64::INFINITY),
                ] {
                    compare(p.rgba_value(value), original(&entries, &profile, value));
                }
                for n in -400..=800 {
                    let value = Some(n as f64 / 16.);
                    compare(p.rgba_value(value), original(&entries, &profile, value));
                }
                let reversed: Vec<_> = entries.into_iter().rev().collect();
                let reversed_p = prepared(&reversed, profile.clone());
                compare(
                    reversed_p.rgba_value(Some(15.)),
                    original(&reversed, &profile, Some(15.)),
                );
            }
        }
        // Exact tie/order matters when coincident depth thresholds emit overlapping intervals.
        let entries = vec![
            entry(IntervalClosure::Closed, 2., 2., "B", 0.5),
            entry(IntervalClosure::GreaterEqual, 2., 2., "A", 0.),
        ];
        let p = prepared(&entries, profile());
        assert_eq!(p.rgba_value(Some(2.)).unwrap(), [241, 33, 7, 128]);
        assert_eq!(
            p.rgba_value(Some(f64::from_bits(2f64.to_bits() + 1)))
                .unwrap(),
            [17, 129, 251, 255]
        );
    }
    #[test]
    fn delivered_bound_rule_all_profiles_shades_and_equal_contours_match_original() {
        let pc = Arc::new(
            PortrayalCatalogue::load_bound(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Catalogues/PC/S-102"),
            )
            .unwrap(),
        );
        assert!(!pc.color_profiles.profiles.is_empty());
        for profile_id in pc.color_profiles.profiles.keys() {
            for four_shades in [false, true] {
                for (shallow, safety, deep) in
                    [(2., 30., 30.), (2., 10., 30.), (0., 0., 0.), (2., 2., 2.)]
                {
                    let p = BathymetryPortrayal::from_bound_catalogue(
                        Arc::clone(&pc),
                        profile_id,
                        DepthSettings {
                            shallow_contour: shallow,
                            safety_contour: safety,
                            deep_contour: deep,
                            four_shades,
                        },
                    )
                    .unwrap();
                    let parsed = ferrite_lua::parse_instruction_string(
                        "BathymetryCoverage",
                        &p.instructions,
                    )
                    .unwrap();
                    let entries = parsed
                        .commands
                        .into_iter()
                        .find_map(|command| match command {
                            DrawingCommand::CoverageFill {
                                attribute_code,
                                lookup_entries,
                                ..
                            } if attribute_code == "depth" => Some(lookup_entries),
                            _ => None,
                        })
                        .unwrap();
                    let profile = &pc.color_profiles.profiles[profile_id];
                    for value in [
                        None,
                        Some(-1.),
                        Some(-0.),
                        Some(0.),
                        Some(shallow - 1e-8),
                        Some(shallow),
                        Some(shallow + 1e-8),
                        Some(safety - 1e-8),
                        Some(safety),
                        Some(safety + 1e-8),
                        Some(deep - 1e-8),
                        Some(deep),
                        Some(deep + 1e-8),
                        Some(f64::MAX),
                        Some(f64::NAN),
                        Some(f64::INFINITY),
                    ] {
                        compare(p.rgba_value(value), original(&entries, profile, value));
                    }
                    assert!(Arc::ptr_eq(p.bound_evaluation().unwrap().catalogue(), &pc));
                }
            }
        }
    }
    #[test]
    fn missing_palette_token_null_token_and_ramp_still_refuse_before_pixels() {
        let profile = profile();
        let mut e = entry(IntervalClosure::Closed, 0., 30., "MISSING", 0.);
        let error = BathymetryPortrayal::resolve_depth_entries(&[e.clone()], &profile)
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "Unresolved coverage colour");
        e.color_token = None;
        assert_eq!(
            BathymetryPortrayal::resolve_depth_entries(&[e.clone()], &profile)
                .err()
                .unwrap()
                .to_string(),
            "Unresolved coverage colour"
        );
        e.color_token = Some("A".into());
        e.end_color = Some(("B".into(), 0.5));
        assert_eq!(
            BathymetryPortrayal::resolve_depth_entries(&[e], &profile)
                .err()
                .unwrap()
                .to_string(),
            "CIE xyL coverage colour ramps not yet supported"
        );
    }
    #[test]
    fn whole_window_rgba_matches_original_with_negative_axis_and_missing_depth() {
        struct Source {
            g: ferrite_kernel::GridGeometry,
            values: Vec<Option<f64>>,
        }
        impl NumericCoverageSource for Source {
            fn numeric_geometry(&self) -> &ferrite_kernel::GridGeometry {
                &self.g
            }
            fn visit_window_values(
                &self,
                w: GridWindow,
                v: &mut dyn FnMut(usize, Option<f64>) -> Result<()>,
            ) -> Result<()> {
                w.validate(&self.g)?;
                for (i, value) in self.values.iter().enumerate() {
                    v(i, *value)?;
                }
                Ok(())
            }
        }
        let entries = vec![
            entry(IntervalClosure::Less, 0., 30., "A", 0.5),
            entry(IntervalClosure::GreaterEqual, 30., 30., "B", 0.),
        ];
        let profile = profile();
        let p = prepared(&entries, profile.clone());
        let values = vec![
            Some(30. - 1e-8),
            Some(30.),
            None,
            Some(-1.),
            Some(31.),
            Some(0.),
        ];
        for (dx, dy) in [(0.1, 0.1), (-0.1, 0.1), (0.1, -0.1), (-0.1, -0.1)] {
            let source = Source {
                g: ferrite_kernel::GridGeometry {
                    width: 3,
                    height: 2,
                    origin_x: 0.,
                    origin_y: 50.,
                    spacing_x: dx,
                    spacing_y: dy,
                    horizontal_crs: 4326,
                },
                values: values.clone(),
            };
            let actual = p
                .raster_window(
                    &source,
                    GridWindow {
                        column: 0,
                        row: 0,
                        width: 3,
                        height: 2,
                    },
                    "oracle".into(),
                )
                .unwrap();
            let mut expected = vec![0u8; 24];
            for (i, value) in values.iter().enumerate() {
                let row = i / 3;
                let col = i % 3;
                let dest_row = if dy > 0. { 1 - row } else { row };
                let dest_col = if dx < 0. { 2 - col } else { col };
                let start = (dest_row * 3 + dest_col) * 4;
                expected[start..start + 4]
                    .copy_from_slice(&original(&entries, &profile, *value).unwrap());
            }
            assert_eq!(actual.rgba, expected);
        }
    }
}
