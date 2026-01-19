//! Area fill definitions
//!
//! Parsed from XML like:
//! ```xml
//! <af:symbolFill>
//!   <areaCRS>GlobalGeometry</areaCRS>
//!   <symbol reference="DIAMOND1P"/>
//!   <v1><x>22.5</x><y>0.0</y></v1>
//!   <v2><x>0</x><y>43.13</y></v2>
//! </af:symbolFill>
//! ```

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

/// 2D Vector point
#[derive(Debug, Clone, Default)]
pub struct VectorPoint {
    pub x: f64,
    pub y: f64,
}

/// Color fill (solid color)
#[derive(Debug, Clone, Default)]
pub struct ColorFill {
    pub color_token: String,
    pub transparency: f64,
}

/// Symbol fill (pattern using symbols) - from XML symbolFill
#[derive(Debug, Clone, Default)]
pub struct SymbolFill {
    pub area_crs: String,
    pub symbol_ref: String,
    pub v1: VectorPoint,  // First vector for tiling
    pub v2: VectorPoint,  // Second vector for tiling
    pub clip_symbols: bool,
}

/// Pattern fill (using symbols) - legacy structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternFill {
    pub symbol_ref: String,
    pub spacing_x: f64,
    pub spacing_y: f64,
    #[serde(default)]
    pub offset_x: f64,
    #[serde(default)]
    pub offset_y: f64,
}

/// Hatch fill pattern
#[derive(Debug, Clone, Default)]
pub struct HatchFill {
    pub line_width: f64,
    pub line_color: String,
    pub spacing: f64,
    pub angle: f64,
}

/// Pixmap fill (image pattern)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PixmapFill {
    pub image_ref: PathBuf,
}

/// Area fill definition
#[derive(Debug, Clone)]
pub enum AreaFillType {
    Color(ColorFill),
    Symbol(SymbolFill),
    Pattern(PatternFill),
    Hatch(HatchFill),
    Pixmap(PixmapFill),
}

/// Complete area fill
#[derive(Debug, Clone)]
pub struct AreaFill {
    pub id: String,
    pub fill_type: AreaFillType,
}

impl AreaFill {
    /// Create a solid color fill
    pub fn solid(id: String, color_token: String) -> Self {
        AreaFill {
            id,
            fill_type: AreaFillType::Color(ColorFill {
                color_token,
                transparency: 0.0,
            }),
        }
    }

    /// Create a symbol fill from XML data
    pub fn symbol(id: String, symbol_ref: String, area_crs: String, v1: VectorPoint, v2: VectorPoint) -> Self {
        AreaFill {
            id,
            fill_type: AreaFillType::Symbol(SymbolFill {
                area_crs,
                symbol_ref,
                v1,
                v2,
                clip_symbols: false,
            }),
        }
    }

    /// Create a pattern fill
    pub fn pattern(id: String, symbol_ref: String, spacing_x: f64, spacing_y: f64) -> Self {
        AreaFill {
            id,
            fill_type: AreaFillType::Pattern(PatternFill {
                symbol_ref,
                spacing_x,
                spacing_y,
                offset_x: 0.0,
                offset_y: 0.0,
            }),
        }
    }
}
