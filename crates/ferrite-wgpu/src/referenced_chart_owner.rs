//! Owner-private text atlas. No UI Context clone/font mutation or global reference aliases.
use crate::{Result, WgpuError};
use ferrite_portrayal_catalog::BoundFontReference;
use std::collections::BTreeMap;
type FontKey = ([u8; 32], [u8; 32]);
type FontUses<'a> = (&'a BoundFontReference, Vec<&'a str>);
type FontInventory<'a> = BTreeMap<FontKey, FontUses<'a>>;
const INPUT_BUDGET: usize = 16 * 1024 * 1024;
const MAX_FONTS: usize = 32;
const MAX_LABELS: usize = 4096;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_ATLAS_SIDE: usize = 2048;
const MAX_DELTA_BYTES: usize = 32 * 1024 * 1024;
fn bad(s: impl Into<String>) -> WgpuError {
    WgpuError::Render(s.into())
}
pub(crate) fn font_family(font: &BoundFontReference) -> egui::FontFamily {
    egui::FontFamily::Name(font.render_family_name().clone())
}
/// Charge complete distinct set BEFORE owned copies/backend parse. Snapshot already retained.
fn font_inventory<'a>(
    requests: impl IntoIterator<Item = (&'a BoundFontReference, &'a str)>,
) -> Result<FontInventory<'a>> {
    let mut fonts = FontInventory::new();
    let (mut bytes, mut labels, mut text_bytes) = (0usize, 0usize, 0usize);
    for (font, text) in requests {
        labels += 1;
        text_bytes = text_bytes
            .checked_add(text.len())
            .ok_or_else(|| bad("Referenced chart text size overflow"))?;
        if labels > MAX_LABELS || text_bytes > MAX_TEXT_BYTES {
            return Err(bad("Referenced chart text receiver budget exceeded"));
        }
        let key = (*font.pc_digest(), *font.font_digest());
        let count = fonts.len();
        match fonts.entry(key) {
            std::collections::btree_map::Entry::Occupied(mut row) => row.get_mut().1.push(text),
            std::collections::btree_map::Entry::Vacant(row) => {
                bytes = bytes
                    .checked_add(font.bytes().len())
                    .ok_or_else(|| bad("Referenced font size overflow"))?;
                if count == MAX_FONTS || bytes > INPUT_BUDGET {
                    return Err(bad("Referenced font receiver budget exceeded"));
                }
                row.insert((font, vec![text]));
            }
        }
    }
    Ok(fonts)
}
pub(crate) fn validate_all_text<'a>(
    labels: impl IntoIterator<Item = (&'a str, f32)>,
) -> Result<()> {
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (text, size) in labels {
        count += 1;
        bytes = bytes
            .checked_add(text.len())
            .ok_or_else(|| bad("Chart text overflow"))?;
        if count > MAX_LABELS
            || bytes > MAX_TEXT_BYTES
            || !size.is_finite()
            || size <= 0.
            || size > 512.
        {
            return Err(bad("Private chart text receiver budget exceeded"));
        }
    }
    Ok(())
}
pub(crate) struct ReferencedChartOwner {
    preparation_identity: std::sync::Arc<()>,
    atlas_generation: u64,
    pub(crate) context: egui::Context,
    atlas: egui_wgpu::Renderer,
    fonts: BTreeMap<FontKey, (BoundFontReference, ab_glyph::FontArc)>,
    pending: egui::TexturesDelta,
    raw: egui::RawInput,
    ppp: f32,
    side: usize,
    extent: [u32; 2],
    density: f32,
}
impl ReferencedChartOwner {
    /// Fresh owner for referenced or bundled chart text; never shares a live atlas.
    /// Empty references still admit the bundled chart families.
    pub(crate) fn prepare<'a>(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        source: &egui::Context,
        extent: [u32; 2],
        density: f32,
        requests: impl IntoIterator<Item = (&'a BoundFontReference, &'a str)>,
    ) -> Result<Self> {
        let fonts = font_inventory(requests)?;
        let ppp = source.pixels_per_point();
        let zoom = source.zoom_factor();
        if extent.contains(&0)
            || !ppp.is_finite()
            || ppp <= 0.
            || !density.is_finite()
            || density <= 0.
            || !zoom.is_finite()
            || zoom <= 0.
        {
            return Err(bad("Invalid referenced chart font environment"));
        }
        let mut definitions = crate::chart_fonts::chart_font_definitions();
        let mut retained_fonts = BTreeMap::new();
        for (font, texts) in fonts.values() {
            let prepared = crate::referenced_chart_font::PreparedReferencedChartFont::prepare(font)
                .map_err(bad)?;
            for text in texts {
                prepared.validate_text(text).map_err(bad)?;
            }
            retained_fonts.insert(
                (*font.pc_digest(), *font.font_digest()),
                ((*font).clone(), prepared.parsed()),
            );
            prepared.install(&mut definitions);
        }
        let context = egui::Context::default();
        context.options_mut(|o| *o = source.options(Clone::clone));
        context.set_zoom_factor(zoom);
        context.set_fonts(definitions);
        let side = MAX_ATLAS_SIDE.min(device.limits().max_texture_dimension_2d as usize);
        if side == 0 {
            return Err(bad("Device has no referenced font atlas capacity"));
        }
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(extent[0] as f32 / density, extent[1] as f32 / density),
            )),
            max_texture_side: Some(side),
            ..Default::default()
        };
        raw.viewports
            .entry(context.viewport_id())
            .or_default()
            .native_pixels_per_point = Some(density);
        context.begin_pass(raw.clone());
        if context.pixels_per_point().to_bits() != ppp.to_bits() {
            return Err(bad("Referenced chart font DPI differs from active layout"));
        }
        Ok(Self {
            preparation_identity: std::sync::Arc::new(()),
            atlas_generation: 0,
            fonts: retained_fonts,
            context,
            atlas: egui_wgpu::Renderer::new(device, format, None, 1, false),
            pending: Default::default(),
            raw,
            ppp,
            side,
            extent,
            density,
        })
    }
    /// Only an exclusively owned active renderer font owner may be reused.
    /// A new private future scene starts with None; never import a live atlas here.
    pub(crate) fn matches<'a>(
        &self,
        extent: [u32; 2],
        density: f32,
        ppp: f32,
        requests: impl IntoIterator<Item = (&'a BoundFontReference, &'a str)>,
    ) -> Result<bool> {
        if self.extent != extent
            || self.density.to_bits() != density.to_bits()
            || self.ppp.to_bits() != ppp.to_bits()
        {
            return Ok(false);
        }
        let next = font_inventory(requests)?;
        if next.len() != self.fonts.len() {
            return Ok(false);
        }
        for (key, (font, texts)) in next {
            let Some((previous, parsed)) = self.fonts.get(&key) else {
                return Ok(false);
            };
            if !std::sync::Arc::ptr_eq(previous.bytes(), font.bytes()) {
                return Ok(false);
            }
            use ab_glyph::Font;
            for text in texts {
                for ch in text.chars().filter(|c| !matches!(c, '\n' | '\r' | '\t')) {
                    if parsed.glyph_id(ch).0 == 0 {
                        return Err(bad(format!(
                            "PC FontReference has no glyph for U+{:04X}",
                            u32::from(ch)
                        )));
                    }
                }
            }
        }
        Ok(true)
    }
    pub(crate) fn begin_metrics(&self) {
        self.context.begin_pass(self.raw.clone());
    }
    /// End metrics pass before exposing owner; validate without any live/GPU writes.
    pub(crate) fn end_metrics(&mut self) -> Result<()> {
        let output = self.context.end_pass();
        self.accept_output(output)
    }
    pub(crate) fn begin_display(&self, extent: [u32; 2], density: f32, ppp: f32) -> Result<()> {
        if self.extent != extent
            || self.density.to_bits() != density.to_bits()
            || self.ppp.to_bits() != ppp.to_bits()
        {
            return Err(bad(
                "Referenced chart fonts require fresh view/DPI preparation",
            ));
        }
        self.context.begin_pass(self.raw.clone());
        Ok(())
    }
    pub(crate) fn end_display(&mut self) -> Result<()> {
        let output = self.context.end_pass();
        self.accept_output(output)
    }
    fn accept_output(&mut self, output: egui::FullOutput) -> Result<()> {
        if self.context.fonts(|fonts| fonts.font_atlas_fill_ratio()) >= 0.8 {
            return Err(bad(
                "Referenced chart atlas requires larger admitted capacity",
            ));
        }
        if output.pixels_per_point.to_bits() != self.ppp.to_bits()
            || !output.textures_delta.free.is_empty()
        {
            return Err(bad("Referenced atlas density/free lifecycle rejected"));
        }
        validate_delta(
            self.pending.set.iter().chain(&output.textures_delta.set),
            self.side,
        )?;
        self.pending.append(output.textures_delta);
        Ok(())
    }
    pub(crate) fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<()> {
        validate_delta(self.pending.set.iter(), self.side)?;
        // Retire before any atlas update; no old cache key may match a replaced texture.
        if !self.pending.set.is_empty() {
            if self.atlas_generation == u64::MAX {
                self.preparation_identity = std::sync::Arc::new(());
                self.atlas_generation = 0;
            } else {
                self.atlas_generation += 1;
            }
        }
        for (id, delta) in &self.pending.set {
            self.atlas.update_texture(device, queue, *id, delta);
        }
        self.pending = Default::default();
        Ok(())
    }
    pub(crate) fn preparation_identity(&self) -> (&std::sync::Arc<()>, u64) {
        (&self.preparation_identity, self.atlas_generation)
    }
    pub(crate) fn bind_group(&self, id: egui::TextureId) -> Result<wgpu::BindGroup> {
        self.atlas
            .texture(&id)
            .map(|texture| texture.bind_group.clone())
            .ok_or_else(|| bad("Referenced chart atlas binding missing"))
    }
}
fn validate_delta<'a>(
    deltas: impl IntoIterator<Item = &'a (egui::TextureId, egui::epaint::ImageDelta)>,
    side: usize,
) -> Result<()> {
    let mut bytes = 0usize;
    for (id, delta) in deltas {
        if *id != egui::TextureId::Managed(0) {
            return Err(bad("Unexpected referenced chart texture namespace"));
        }
        let size = delta.image.size();
        if size.contains(&0)
            || size.iter().any(|&n| n > side)
            || delta.pos.is_some_and(|pos| {
                pos[0].checked_add(size[0]).is_none_or(|n| n > side)
                    || pos[1].checked_add(size[1]).is_none_or(|n| n > side)
            })
        {
            return Err(bad("Referenced chart atlas bounds exceeded"));
        }
        bytes = size[0]
            .checked_mul(size[1])
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| bytes.checked_add(n))
            .ok_or_else(|| bad("Referenced chart atlas size overflow"))?;
        if bytes > MAX_DELTA_BYTES {
            return Err(bad(
                "Referenced chart atlas upload receiver budget exceeded",
            ));
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn complete_label_admission_precedes_private_font_work() {
        assert!(validate_all_text([("valid", 16.)]).is_ok());
        for size in [0., f32::NAN, 513.] {
            assert!(validate_all_text([("valid", size)]).is_err());
        }
        assert!(validate_all_text(std::iter::repeat_n(("", 16.), MAX_LABELS + 1)).is_err());
        let text = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(validate_all_text([(text.as_str(), 16.)]).is_err());
    }
    #[test]
    fn unknown_texture_and_oversize_rejected_before_upload() {
        let delta = egui::epaint::ImageDelta::full(
            egui::ColorImage::new([2, 2], egui::Color32::WHITE),
            egui::TextureOptions::LINEAR,
        );
        assert!(validate_delta([&(egui::TextureId::User(0), delta.clone())], 2).is_err());
        assert!(validate_delta([&(egui::TextureId::Managed(0), delta.clone())], 1).is_err());
        assert!(validate_delta([&(egui::TextureId::Managed(0), delta)], 2).is_ok());
    }
}

#[cfg(test)]
#[path = "referenced_chart_owner_gpu_tests.rs"]
pub(crate) mod gpu_tests;
