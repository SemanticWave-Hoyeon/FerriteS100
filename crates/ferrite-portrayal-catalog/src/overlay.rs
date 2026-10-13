//! Namespaced symbol ids for overlay products (S-124, S-129, ...).
//!
//! Overlay products are portrayed with their own portrayal catalogue but
//! share the renderer's symbol caches with the S-101 catalogue.  Their symbol
//! ids are namespaced as `<product id>/<symbol id>` (e.g.
//! `S-124/NavigationalWarningFeaturePart`) so they never collide with S-101
//! symbol ids, which never contain `/`.

use crate::{AreaFillType, LineStyle, PortrayalCatalogue};

/// Separator between the product prefix and the catalogue symbol id.
pub const OVERLAY_SYMBOL_SEPARATOR: char = '/';

/// Build the namespaced id `<prefix>/<symbol>`. Already-namespaced ids are
/// returned unchanged.
pub fn overlay_symbol_id(prefix: &str, symbol: &str) -> String {
    if split_overlay_symbol_id(symbol).is_some() {
        symbol.to_owned()
    } else {
        format!("{prefix}{OVERLAY_SYMBOL_SEPARATOR}{symbol}")
    }
}

/// Split `<prefix>/<symbol>` at the first `/`. Returns `None` for plain
/// (S-101) ids or when either side is empty.
pub fn split_overlay_symbol_id(id: &str) -> Option<(&str, &str)> {
    let (prefix, symbol) = id.split_once(OVERLAY_SYMBOL_SEPARATOR)?;
    (!prefix.is_empty() && !symbol.is_empty()).then_some((prefix, symbol))
}

impl PortrayalCatalogue {
    /// Rewrite every symbol reference held by line styles and area fills to
    /// the namespaced form `<prefix>/<id>`, so drawing instructions converted
    /// with this catalogue resolve their embedded symbols from the overlay
    /// symbol source registered for `prefix` instead of the S-101 catalogue.
    /// Idempotent. The `symbols` table keeps its plain ids.
    pub fn namespace_symbol_references(&mut self, prefix: &str) {
        for style in self.line_styles.values_mut() {
            let parts = match style {
                LineStyle::Simple(s) => std::slice::from_mut(s),
                LineStyle::Complex(c) => c.strokes.as_mut_slice(),
                LineStyle::Composite(c) => c.components.as_mut_slice(),
            };
            for part in parts {
                for symbol in &mut part.symbols {
                    symbol.reference = overlay_symbol_id(prefix, &symbol.reference);
                }
            }
        }
        for fill in self.area_fills.values_mut() {
            match &mut fill.fill_type {
                AreaFillType::Symbol(s) => s.symbol_ref = overlay_symbol_id(prefix, &s.symbol_ref),
                AreaFillType::Pattern(p) => p.symbol_ref = overlay_symbol_id(prefix, &p.symbol_ref),
                AreaFillType::Color(_) | AreaFillType::Hatch(_) | AreaFillType::Pixmap(_) => {}
            }
        }
    }

    /// Symbols directory of this catalogue (`<PC>/Symbols`).
    pub fn symbols_dir(&self) -> &std::path::Path {
        &self.symbols.base_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_round_trip() {
        assert_eq!(overlay_symbol_id("S-124", "QUESMRK1"), "S-124/QUESMRK1");
        assert_eq!(overlay_symbol_id("S-124", "S-129/X"), "S-129/X");
        assert_eq!(
            split_overlay_symbol_id("S-124/QUESMRK1"),
            Some(("S-124", "QUESMRK1"))
        );
        assert_eq!(split_overlay_symbol_id("QUESMRK1"), None);
        assert_eq!(split_overlay_symbol_id("/x"), None);
        assert_eq!(split_overlay_symbol_id("x/"), None);
    }
}
