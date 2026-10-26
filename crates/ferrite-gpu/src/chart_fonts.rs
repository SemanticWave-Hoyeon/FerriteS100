//! Shared chart font definitions with independently owned layout contexts.
pub fn chart_font_definitions() -> epaint::text::FontDefinitions {
    let mut fonts = epaint::text::FontDefinitions::default();
    fonts.font_data.insert(
        "ChartBold".into(),
        epaint::text::FontData::from_static(include_bytes!(
            "../../../Catalogues/PC/S-421/Fonts/OpenSans-Bold.ttf"
        ))
        .into(),
    );
    fonts.font_data.insert(
        "ChartMedium".into(),
        epaint::text::FontData::from_static(include_bytes!(
            "../../../Catalogues/PC/S-421/Fonts/OpenSans-Regular.ttf"
        ))
        .into(),
    );
    // Text such as LLM replies or Korean ENC names may need Hangul or other
    // CJK glyphs that the bundled faces lack. A system face is appended as
    // the last fallback (only glyphs that would otherwise be missing), before
    // the chart chains copy the proportional fallback order.
    for (index, (name, data)) in system_cjk_fallbacks().iter().enumerate() {
        let key = format!("SystemCjk{index}-{name}");
        fonts
            .font_data
            .insert(key.clone(), std::sync::Arc::new(data.clone()));
        for family in [
            epaint::FontFamily::Proportional,
            epaint::FontFamily::Monospace,
        ] {
            fonts.families.entry(family).or_default().push(key.clone());
        }
    }
    let mut medium_fallback = fonts.families[&epaint::FontFamily::Proportional].clone();
    medium_fallback.insert(0, "ChartMedium".into());
    fonts.families.insert(
        epaint::FontFamily::Name("ChartMedium".into()),
        medium_fallback,
    );
    let mut fallback = fonts.families[&epaint::FontFamily::Proportional].clone();
    fallback.insert(0, "ChartBold".into());
    fonts
        .families
        .insert(epaint::FontFamily::Name("ChartBold".into()), fallback);
    fonts
}

/// First readable face per script group, read once per process.
fn system_cjk_fallbacks() -> &'static [(String, epaint::text::FontData)] {
    static FONTS: std::sync::OnceLock<Vec<(String, epaint::text::FontData)>> =
        std::sync::OnceLock::new();
    FONTS.get_or_init(|| {
        // One Korean face: KS X 1001 faces also carry kana and the common
        // Hanja, so a separate 50+ MB Chinese face is not kept resident.
        let groups: [&[&str]; 1] = [&[
            "/System/Library/Fonts/Supplemental/AppleGothic.ttf",
            "C:\\Windows\\Fonts\\malgun.ttf",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
        ]];
        const MAX_BYTES: u64 = 64 * 1024 * 1024;
        groups
            .iter()
            .filter_map(|paths| {
                paths.iter().find_map(|path| {
                    let path = std::path::Path::new(path);
                    let len = std::fs::metadata(path).ok()?.len();
                    if len > MAX_BYTES {
                        return None;
                    }
                    let bytes = std::fs::read(path).ok()?;
                    let name = path.file_stem()?.to_string_lossy().replace(' ', "");
                    // Read once per process and borrowed by every font
                    // context, so a new context never copies the face.
                    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
                    Some((name, epaint::text::FontData::from_static(bytes)))
                })
            })
            .collect()
    })
}

/// Receiver best-match policy, not an ordering prescribed by S-100.
/// Only audited immutable bundled text faces participate; emoji fallbacks are
/// glyph fallback resources, not candidates for Latin font characteristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChartCharacteristicFace {
    ProportionalLight,
    ProportionalMedium,
    ProportionalBold,
    MonospaceMedium,
}

impl ChartCharacteristicFace {
    fn characteristics(
        self,
    ) -> (
        bool,
        ferrite_render::TextFontWeight,
        bool,
        ferrite_render::TextFontProportion,
    ) {
        use ferrite_render::{TextFontProportion as P, TextFontWeight as W};
        match self {
            Self::ProportionalLight => (false, W::Light, false, P::Proportional),
            Self::ProportionalMedium => (false, W::Medium, false, P::Proportional),
            Self::ProportionalBold => (false, W::Bold, false, P::Proportional),
            Self::MonospaceMedium => (false, W::Medium, false, P::MonoSpaced),
        }
    }

    pub(crate) fn family(self) -> epaint::FontFamily {
        // Retain one immutable family identifier instead of allocating it per label.
        static MEDIUM: std::sync::OnceLock<epaint::FontFamily> = std::sync::OnceLock::new();
        static BOLD: std::sync::OnceLock<epaint::FontFamily> = std::sync::OnceLock::new();
        match self {
            Self::ProportionalLight => epaint::FontFamily::Proportional,
            Self::ProportionalMedium => MEDIUM
                .get_or_init(|| epaint::FontFamily::Name("ChartMedium".into()))
                .clone(),
            Self::ProportionalBold => BOLD
                .get_or_init(|| epaint::FontFamily::Name("ChartBold".into()))
                .clone(),
            Self::MonospaceMedium => epaint::FontFamily::Monospace,
        }
    }
}

/// Actual-face mismatch remains distinct from optional receiver synthetic slant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChartFontMismatch {
    pub(crate) serifs: bool,
    pub(crate) weight: bool,
    pub(crate) slant: bool,
    pub(crate) proportion: bool,
}
impl ChartFontMismatch {
    fn count(self) -> u8 {
        u8::from(self.serifs)
            + u8::from(self.weight)
            + u8::from(self.slant)
            + u8::from(self.proportion)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChartFontMatch {
    pub(crate) face: ChartCharacteristicFace,
    pub(crate) mismatch: ChartFontMismatch,
    pub(crate) distance: u8,
    pub(crate) synthetic_italic: bool,
}
impl ChartFontMatch {
    pub(crate) fn family(self) -> epaint::FontFamily {
        debug_assert_eq!(self.distance, self.mismatch.count());
        self.face.family()
    }
}

/// Compare all four mandatory characteristics. Minimise the number of unequal
/// characteristics; ties favour proportion, serifs, slant, nearest weight, then
/// stable face order. Missing serif/italic/monospace-bold faces remain mismatches:
/// this function does not manufacture a face or assert every request is exact.
pub(crate) fn match_chart_characteristics(
    style: &ferrite_render::TextFontStyle,
    legacy_bold: bool,
    italic: bool,
) -> ChartFontMatch {
    use ferrite_render::TextFontWeight as W;
    fn rank(weight: W) -> u8 {
        match weight {
            W::Light => 0,
            W::Medium => 1,
            W::Bold => 2,
        }
    }
    let weight = if legacy_bold { W::Bold } else { style.weight };
    let faces = [
        ChartCharacteristicFace::ProportionalLight,
        ChartCharacteristicFace::ProportionalMedium,
        ChartCharacteristicFace::ProportionalBold,
        ChartCharacteristicFace::MonospaceMedium,
    ];
    let mut best = faces[0];
    let mut best_score = (u8::MAX, true, true, true, u8::MAX, usize::MAX);
    for (ordinal, face) in faces.into_iter().enumerate() {
        let (serifs, face_weight, slanted, proportion) = face.characteristics();
        let p = proportion != style.proportion;
        let s = serifs != style.serifs;
        let i = slanted != italic;
        let w = face_weight != weight;
        let score = (
            u8::from(p) + u8::from(s) + u8::from(i) + u8::from(w),
            p,
            s,
            i,
            rank(face_weight).abs_diff(rank(weight)),
            ordinal,
        );
        if score < best_score {
            best = face;
            best_score = score;
        }
    }
    let (serifs, face_weight, slanted, proportion) = best.characteristics();
    let mismatch = ChartFontMismatch {
        serifs: serifs != style.serifs,
        weight: face_weight != weight,
        slant: slanted != italic,
        proportion: proportion != style.proportion,
    };
    ChartFontMatch {
        face: best,
        distance: mismatch.count(),
        mismatch,
        synthetic_italic: italic && !slanted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_characteristic_combinations_choose_independent_available_face_oracle() {
        use ferrite_render::{TextFontProportion as P, TextFontWeight as W};
        for serifs in [false, true] {
            for italic in [false, true] {
                for weight in [W::Light, W::Medium, W::Bold] {
                    for proportion in [P::MonoSpaced, P::Proportional] {
                        let style = ferrite_render::TextFontStyle {
                            serifs,
                            weight,
                            proportion,
                            ..Default::default()
                        };
                        // No serif or italic faces exist in this immutable inventory.
                        // Closed-form independent oracle keeps mono widths even for bold.
                        let expected = if proportion == P::MonoSpaced {
                            ChartCharacteristicFace::MonospaceMedium
                        } else {
                            match weight {
                                W::Light => ChartCharacteristicFace::ProportionalLight,
                                W::Medium => ChartCharacteristicFace::ProportionalMedium,
                                W::Bold => ChartCharacteristicFace::ProportionalBold,
                            }
                        };
                        assert_eq!(
                            match_chart_characteristics(&style, false, italic).face,
                            expected
                        );
                        let matching = match_chart_characteristics(&style, false, italic);
                        assert_eq!(matching.mismatch.serifs, serifs);
                        assert_eq!(matching.mismatch.slant, italic);
                        assert!(!matching.mismatch.proportion);
                        assert_eq!(
                            matching.mismatch.weight,
                            proportion == P::MonoSpaced && weight != W::Medium
                        );
                        assert_eq!(
                            matching.distance,
                            u8::from(serifs)
                                + u8::from(italic)
                                + u8::from(matching.mismatch.weight)
                        );
                        assert_eq!(matching.synthetic_italic, italic);
                        let actual = expected.characteristics();
                        assert!(!actual.0);
                        assert!(!actual.2);
                        assert_eq!(actual.3, proportion);
                    }
                }
            }
        }
    }

    #[test]
    fn legacy_bold_and_mixed_weight_proportion_preserve_declared_width_class() {
        use ferrite_render::{TextFontProportion as P, TextFontWeight as W};
        let mut style = ferrite_render::TextFontStyle {
            weight: W::Light,
            ..Default::default()
        };
        assert_eq!(
            match_chart_characteristics(&style, true, true).face,
            ChartCharacteristicFace::ProportionalBold
        );
        style.proportion = P::MonoSpaced;
        assert_eq!(
            match_chart_characteristics(&style, true, true).face,
            ChartCharacteristicFace::MonospaceMedium
        );
    }

    #[test]
    fn medium_face_is_original_bundled_regular_and_family_storage_is_reused() {
        let fonts = chart_font_definitions();
        let family = ChartCharacteristicFace::ProportionalMedium.family();
        let chain = &fonts.families[&family];
        assert_eq!(chain[0], "ChartMedium");
        assert_eq!(
            fonts.font_data["ChartMedium"].font.as_ref(),
            include_bytes!("../../../Catalogues/PC/S-421/Fonts/OpenSans-Regular.ttf").as_slice()
        );
        assert_eq!(
            &chain[1..],
            fonts.families[&epaint::FontFamily::Proportional].as_slice()
        );
        if let (epaint::FontFamily::Name(a), epaint::FontFamily::Name(b)) =
            (family, ChartCharacteristicFace::ProportionalMedium.family())
        {
            assert!(std::sync::Arc::ptr_eq(&a, &b));
        } else {
            panic!("named medium family expected");
        }
    }

    #[test]
    fn audited_sfnt_inventory_matches_actual_available_font_tables() {
        fn table<'a>(font: &'a [u8], tag: &[u8; 4]) -> &'a [u8] {
            fn u16_at(bytes: &[u8], index: usize) -> usize {
                usize::from(u16::from_be_bytes(
                    bytes
                        .get(index..index.checked_add(2).unwrap())
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ))
            }
            let count = u16_at(font, 4);
            let directory_end = 12usize.checked_add(count.checked_mul(16).unwrap()).unwrap();
            assert!(directory_end <= font.len());
            let mut found = None;
            for entry in font[12..directory_end].chunks_exact(16) {
                if &entry[..4] != tag {
                    continue;
                }
                assert!(found.is_none(), "duplicate table");
                let offset =
                    usize::try_from(u32::from_be_bytes(entry[8..12].try_into().unwrap())).unwrap();
                let length =
                    usize::try_from(u32::from_be_bytes(entry[12..16].try_into().unwrap())).unwrap();
                found = Some(
                    font.get(offset..offset.checked_add(length).unwrap())
                        .unwrap(),
                );
            }
            found.expect("required SFNT table")
        }
        let fonts = chart_font_definitions();
        for (name, weight, fixed) in [
            ("Ubuntu-Light", 300u16, 0u32),
            ("ChartMedium", 400, 0),
            ("ChartBold", 700, 0),
            ("Hack", 400, 1),
        ] {
            let font = fonts.font_data[name].font.as_ref();
            let os2 = table(font, b"OS/2");
            let post = table(font, b"post");
            let head = table(font, b"head");
            assert_eq!(
                u16::from_be_bytes(os2[4..6].try_into().unwrap()),
                weight,
                "{name}: weight"
            );
            assert_eq!(os2[32], 2, "{name}: PANOSE Latin text");
            assert_eq!(os2[33], 11, "{name}: PANOSE normal sans");
            assert_eq!(
                u16::from_be_bytes(os2[62..64].try_into().unwrap()) & 1,
                0,
                "{name}: not italic"
            );
            assert_eq!(
                i32::from_be_bytes(post[4..8].try_into().unwrap()),
                0,
                "{name}: upright angle"
            );
            assert_eq!(
                u32::from_be_bytes(post[12..16].try_into().unwrap()),
                fixed,
                "{name}: fixed pitch"
            );
            assert_eq!(
                u16::from_be_bytes(head[44..46].try_into().unwrap()) & 2,
                0,
                "{name}: not italic"
            );
        }
    }

    #[test]
    fn new_chart_fonts_preserve_bold_family_and_original_fallback_order() {
        let fonts = chart_font_definitions();
        let bold = &fonts.families[&epaint::FontFamily::Name("ChartBold".into())];
        assert_eq!(bold.first().map(String::as_str), Some("ChartBold"));
        assert_eq!(
            &bold[1..],
            fonts.families[&epaint::FontFamily::Proportional].as_slice()
        );
        assert!(fonts.font_data.contains_key("ChartBold"));
    }
}
