//! Selection-only original PC/palette symbol preview. No per-frame SVG work.
use crate::{CellPortrayalResources, Result, SelectedFeature, SymbolGeometry, WgpuError};
use std::sync::Arc;
const MAX_PREVIEW_BYTES: usize = 1024 * 1024;
const MAX_PREVIEW_SIDE: u32 = 512;
const MAX_SOURCE_SIDE: u32 = 8192;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_SOURCE_TEXT: usize = 8192;
#[derive(Clone)]
pub struct SelectedSymbolPreview {
    cell: u32,
    feature_id: i64,
    source: String,
    symbol: String,
    pc_digest: [u8; 32],
    palette: String,
    background: egui::Color32,
    background_token: &'static str,
    image: Arc<egui::ColorImage>,
    texture: Option<egui::TextureHandle>,
}
impl std::fmt::Debug for SelectedSymbolPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectedSymbolPreview")
            .field("cell", &self.cell)
            .field("symbol", &self.symbol)
            .field("palette", &self.palette)
            .field("size", &self.image.size)
            .finish_non_exhaustive()
    }
}
impl SelectedSymbolPreview {
    fn from_geometry(
        feature: &SelectedFeature,
        digest: [u8; 32],
        palette: &str,
        symbol: &SymbolGeometry,
        background: egui::Color32,
        background_token: &'static str,
    ) -> Result<Self> {
        let error =
            || WgpuError::Render("Selected symbol preview input/size budget rejected".into());
        let cell = feature.cell_index.ok_or_else(error)?;
        let source = feature.source.as_deref().ok_or_else(error)?;
        let name = feature.symbol_name.as_deref().ok_or_else(error)?;
        if source.len() > MAX_SOURCE_TEXT
            || name.len() > 1024
            || palette.len() > 128
            || name != symbol.name
            || symbol.width == 0
            || symbol.height == 0
            || symbol.width > MAX_SOURCE_SIDE
            || symbol.height > MAX_SOURCE_SIDE
        {
            return Err(error());
        }
        let bytes = (symbol.width as usize)
            .checked_mul(symbol.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(error)?;
        if bytes > MAX_SOURCE_BYTES || symbol.pixels.len() != bytes {
            return Err(error());
        }
        // Reuse the already rendered PC pixels. Legitimate large symbols get a
        // bounded thumbnail; no second SVG parse or source-sized image copy.
        let longest = symbol.width.max(symbol.height);
        let width = if longest > MAX_PREVIEW_SIDE {
            (u64::from(symbol.width) * u64::from(MAX_PREVIEW_SIDE) / u64::from(longest)).max(1)
                as usize
        } else {
            symbol.width as usize
        };
        let height = if longest > MAX_PREVIEW_SIDE {
            (u64::from(symbol.height) * u64::from(MAX_PREVIEW_SIDE) / u64::from(longest)).max(1)
                as usize
        } else {
            symbol.height as usize
        };
        let image = if longest <= MAX_PREVIEW_SIDE {
            egui::ColorImage::from_rgba_premultiplied([width, height], &symbol.pixels)
        } else {
            let mut pixels = Vec::with_capacity(width * height * 4);
            for y in 0..height {
                let sy = (((y as f64 + 0.5) * f64::from(symbol.height) / height as f64) - 0.5)
                    .clamp(0., f64::from(symbol.height - 1));
                let y0 = sy.floor() as usize;
                let y1 = (y0 + 1).min(symbol.height as usize - 1);
                let fy = sy - y0 as f64;
                for x in 0..width {
                    let sx = (((x as f64 + 0.5) * f64::from(symbol.width) / width as f64) - 0.5)
                        .clamp(0., f64::from(symbol.width - 1));
                    let x0 = sx.floor() as usize;
                    let x1 = (x0 + 1).min(symbol.width as usize - 1);
                    let fx = sx - x0 as f64;
                    for channel in 0..4 {
                        let sample = |x, y| {
                            f64::from(symbol.pixels[(y * symbol.width as usize + x) * 4 + channel])
                        };
                        let top = sample(x0, y0) * (1. - fx) + sample(x1, y0) * fx;
                        let bottom = sample(x0, y1) * (1. - fx) + sample(x1, y1) * fx;
                        pixels.push((top * (1. - fy) + bottom * fy).round() as u8);
                    }
                }
            }
            egui::ColorImage::from_rgba_premultiplied([width, height], &pixels)
        };
        debug_assert!(image.pixels.len() * 4 <= MAX_PREVIEW_BYTES);
        Ok(Self {
            cell,
            feature_id: feature.feature_id,
            source: source.into(),
            symbol: name.into(),
            pc_digest: digest,
            palette: palette.into(),
            background,
            background_token,
            image: Arc::new(image),
            texture: None,
        })
    }
    pub fn matches(&self, feature: &SelectedFeature, palette: &str) -> bool {
        feature.cell_index == Some(self.cell)
            && feature.feature_id == self.feature_id
            && feature.source.as_deref() == Some(self.source.as_str())
            && feature.symbol_name.as_deref() == Some(self.symbol.as_str())
            && palette == self.palette
    }
    /// Repeated click/unchanged publication fast path, with current exact bound
    /// PC identity checked by the App before retaining this selection image.
    pub fn matches_bound(
        &self,
        feature: &SelectedFeature,
        palette: &str,
        pc_digest: &[u8; 32],
    ) -> bool {
        &self.pc_digest == pc_digest && self.matches(feature, palette)
    }
    pub fn pc_digest(&self) -> &[u8; 32] {
        &self.pc_digest
    }
    pub fn draw(&mut self, ui: &mut egui::Ui, feature: &SelectedFeature, palette: &str) {
        if !self.matches(feature, palette) {
            return;
        }
        let texture = self.texture.get_or_insert_with(|| {
            ui.ctx().load_texture(
                "selected-PC-symbol-preview",
                self.image.clone(),
                egui::TextureOptions::LINEAR,
            )
        });
        let [width, height] = self.image.size;
        let factor = (64.0 / (width.max(height) as f32)).min(1.0);
        let size = egui::vec2(width as f32 * factor, height as f32 * factor);
        ui.horizontal(|ui| {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(72., 72.), egui::Sense::hover());
            ui.painter().rect_filled(rect, 2., self.background);
            ui.painter().image(
                texture.id(),
                egui::Rect::from_center_size(rect.center(), size),
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1., 1.)),
                egui::Color32::WHITE,
            );
            response.on_hover_text(format!(
                "{} · {} · original cell PC · {} backdrop",
                self.symbol, self.palette, self.background_token
            ));
            ui.vertical(|ui| {
                ui.label(&self.symbol);
                ui.weak(&self.palette);
            });
        });
    }
}
fn owner_background(
    profile: &ferrite_portrayal_catalog::ColorProfile,
) -> Result<(egui::Color32, &'static str)> {
    for token in ["DEPDW", "CHWHT"] {
        if let Some(rgb) = profile.colors.get(token).and_then(|color| color.get_srgb()) {
            return Ok((egui::Color32::from_rgb(rgb.r, rgb.g, rgb.b), token));
        }
    }
    Err(WgpuError::Render(
        "Selected owner palette lacks sRGB DEPDW/CHWHT preview backdrop".into(),
    ))
}
/// Resolve exactly the selected cell owner; missing owners/assets never fall
/// back to another edition's same-named symbol. No point symbol returns None.
/// Call only on selection or successful source/palette publication, not frames.
pub fn prepare_selected_symbol_preview(
    feature: &SelectedFeature,
    resources: &mut CellPortrayalResources,
) -> Result<Option<SelectedSymbolPreview>> {
    let Some(name) = feature.symbol_name.as_deref() else {
        return Ok(None);
    };
    if name.len() > 1024
        || feature
            .source
            .as_ref()
            .is_none_or(|source| source.len() > MAX_SOURCE_TEXT)
    {
        return Err(WgpuError::Render(
            "Selected symbol preview identity budget rejected".into(),
        ));
    }
    let owner = resources.resolve_mut(feature.cell_index.map(|v| v as usize))?;
    let digest = *owner.owner.pc_digest();
    let palette = owner.profile.id.clone();
    let (background, background_token) = owner_background(owner.profile)?;
    let geometry = owner
        .cache
        .get_symbol(name, owner.profile)
        .ok_or_else(|| WgpuError::Render(format!("Selected cell PC symbol unavailable: {name}")))?;
    SelectedSymbolPreview::from_geometry(
        feature,
        digest,
        &palette,
        geometry,
        background,
        background_token,
    )
    .map(Some)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn feature() -> SelectedFeature {
        SelectedFeature {
            feature_type: "Beacon".into(),
            feature_id: 7,
            foid: None,
            cell_index: Some(1),
            primitive_type: "Point".into(),
            source: Some("/original/test.000".into()),
            attributes: vec![],
            world_pos: (0., 0.),
            longitude_shift: 0.,
            definition: None,
            symbol_name: Some("SAME".into()),
        }
    }
    fn geometry() -> SymbolGeometry {
        SymbolGeometry {
            name: "SAME".into(),
            pixels: vec![32, 16, 0, 64],
            width: 1,
            height: 1,
            bounds: (0., 0., 1., 1.),
            pivot: (0., 0.),
            texture_pivot: (0., 0.),
            render_scale: 1.,
        }
    }
    #[test]
    fn original_premultiplied_bytes_and_selection_owner_are_preserved() {
        let f = feature();
        let p = SelectedSymbolPreview::from_geometry(
            &f,
            [1; 32],
            "Night",
            &geometry(),
            egui::Color32::BLACK,
            "DEPDW",
        )
        .unwrap();
        assert_eq!(p.image.pixels[0].to_array(), [32, 16, 0, 64]);
        assert!(p.matches(&f, "Night"));
        assert!(p.matches_bound(&f, "Night", &[1; 32]));
        assert!(!p.matches_bound(&f, "Night", &[2; 32]));
        let mut foreign = f.clone();
        foreign.cell_index = Some(2);
        assert!(!p.matches(&foreign, "Night"));
        foreign = f.clone();
        foreign.source = Some("/other/test.000".into());
        assert!(!p.matches(&foreign, "Night"));
        assert!(!p.matches(&f, "Day"));
    }
    #[test]
    fn malformed_and_oversized_preview_declines_before_copy() {
        let f = feature();
        let mut g = geometry();
        g.pixels.push(0);
        assert!(SelectedSymbolPreview::from_geometry(
            &f,
            [0; 32],
            "Day",
            &g,
            egui::Color32::BLACK,
            "DEPDW"
        )
        .is_err());
        g = geometry();
        g.width = 513;
        assert!(SelectedSymbolPreview::from_geometry(
            &f,
            [0; 32],
            "Day",
            &g,
            egui::Color32::BLACK,
            "DEPDW"
        )
        .is_err());
        g = geometry();
        g.name = "OTHER".into();
        assert!(SelectedSymbolPreview::from_geometry(
            &f,
            [0; 32],
            "Day",
            &g,
            egui::Color32::BLACK,
            "DEPDW"
        )
        .is_err());
    }
    #[test]
    fn headless_frames_upload_once_and_stale_palette_never_uploads() {
        let ctx = egui::Context::default();
        let f = feature();
        let mut p = SelectedSymbolPreview::from_geometry(
            &f,
            [0; 32],
            "Day",
            &geometry(),
            egui::Color32::BLACK,
            "DEPDW",
        )
        .unwrap();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| p.draw(ui, &f, "Night"));
        });
        assert!(p.texture.is_none());
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| p.draw(ui, &f, "Day"));
        });
        let id = p.texture.as_ref().unwrap().id();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| p.draw(ui, &f, "Day"));
        });
        assert_eq!(p.texture.as_ref().unwrap().id(), id);
    }
}

#[cfg(test)]
mod backdrop_tests {
    use super::*;
    use ferrite_portrayal_catalog::{ColorDefinition, ColorProfile, SrgbColor};
    #[test]
    fn only_selected_owner_palette_backdrop_and_same_owner_fallback() {
        let mut p = ColorProfile::new("Day".into(), "Day".into());
        assert!(owner_background(&p).is_err());
        p.colors.insert(
            "CHWHT".into(),
            ColorDefinition {
                token: "CHWHT".into(),
                srgb: Some(SrgbColor::new(1, 2, 3)),
                cie: None,
            },
        );
        assert_eq!(
            owner_background(&p).unwrap(),
            (egui::Color32::from_rgb(1, 2, 3), "CHWHT")
        );
        p.colors.insert(
            "DEPDW".into(),
            ColorDefinition {
                token: "DEPDW".into(),
                srgb: Some(SrgbColor::new(4, 5, 6)),
                cie: None,
            },
        );
        assert_eq!(
            owner_background(&p).unwrap(),
            (egui::Color32::from_rgb(4, 5, 6), "DEPDW")
        );
    }
}
