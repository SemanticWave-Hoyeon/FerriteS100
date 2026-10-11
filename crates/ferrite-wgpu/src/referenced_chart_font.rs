//! Fallible staging, before installing font definitions into a PRIVATE egui Context.
use ab_glyph::Font;
use ferrite_portrayal_catalog::BoundFontReference;
/// The explicit reference has a single-font family; no system/ChartBold fallback.
/// Glyph availability must be checked for each final text before any live publication.
pub(crate) struct PreparedReferencedChartFont {
    pub family: egui::FontFamily,
    name: String,
    font_data: egui::FontData,
    parsed: ab_glyph::FontArc,
}
impl PreparedReferencedChartFont {
    pub fn prepare(font: &BoundFontReference) -> Result<Self, String> {
        let parsed = ab_glyph::FontArc::try_from_vec(font.bytes().to_vec())
            .map_err(|_| "PC FontReference cannot be parsed by chart font backend".to_owned())?;
        let units = parsed
            .units_per_em()
            .filter(|units| units.is_finite() && *units >= 16. && *units <= 16384.)
            .ok_or_else(|| "PC referenced font units exceed chart backend range".to_owned())?;
        let height = parsed.height_unscaled();
        if !height.is_finite()
            || height <= 0.
            || !parsed.ascent_unscaled().is_finite()
            || !parsed.descent_unscaled().is_finite()
            || !parsed.line_gap_unscaled().is_finite()
            || !(height / units).is_finite()
        {
            return Err("PC referenced font metrics cannot be safely laid out".into());
        }
        // FontDefinitions owns String keys; this occurs only for a newly prepared owner.
        // Repeated labels use the shared Arc family name without formatting/allocation.
        let name = font.render_family_name().to_string();
        let family = egui::FontFamily::Name(font.render_family_name().clone());
        Ok(Self {
            family,
            name,
            font_data: egui::FontData::from_owned(font.bytes().to_vec()),
            parsed,
        })
    }
    pub fn parsed(&self) -> ab_glyph::FontArc {
        self.parsed.clone()
    }
    pub fn validate_text(&self, text: &str) -> Result<(), String> {
        for ch in text.chars().filter(|c| !matches!(c, '\n' | '\r' | '\t')) {
            if self.parsed.glyph_id(ch).0 == 0 {
                return Err(format!(
                    "PC FontReference has no glyph for U+{:04X}",
                    u32::from(ch)
                ));
            }
        }
        Ok(())
    }
    pub fn install(self, definitions: &mut egui::FontDefinitions) -> egui::FontFamily {
        definitions
            .font_data
            .insert(self.name.clone(), self.font_data.into());
        definitions
            .families
            .insert(self.family.clone(), vec![self.name]);
        self.family
    }
}
// The owner charges the distinct captured set before preparing fonts (16MiB),
// then separately checks atlas/delta payload. Backend copies and glyph allocation
// mean these logical limits are not a whole-process peak-memory guarantee.
