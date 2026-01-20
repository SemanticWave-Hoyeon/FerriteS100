//! Symbol definitions

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Symbol placement on line
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineSymbolPlacement {
    pub offset: f64,
    pub repeat_interval: Option<f64>,
}

/// Symbol placement in area
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AreaSymbolPlacement {
    pub spacing_x: f64,
    pub spacing_y: f64,
    pub offset_x: f64,
    pub offset_y: f64,
}

/// Pivot point for symbol
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct PivotPoint {
    pub x: f64,
    pub y: f64,
}

/// Symbol definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub id: String,
    pub svg_reference: PathBuf,
    #[serde(default)]
    pub rotation: f64,
    #[serde(default)]
    pub rotation_crs: Option<String>,
    #[serde(default = "default_scale")]
    pub scale_factor: f64,
    #[serde(default)]
    pub pivot: PivotPoint,
    #[serde(default)]
    pub offset_x: f64,
    #[serde(default)]
    pub offset_y: f64,
}

fn default_scale() -> f64 {
    1.0
}

impl Symbol {
    pub fn new(id: String, svg_reference: PathBuf) -> Self {
        Symbol {
            id,
            svg_reference,
            rotation: 0.0,
            rotation_crs: None,
            scale_factor: 1.0,
            pivot: PivotPoint::default(),
            offset_x: 0.0,
            offset_y: 0.0,
        }
    }
}

/// Collection of symbols
#[derive(Debug, Clone, Default)]
pub struct Symbols {
    pub symbols: HashMap<String, Symbol>,
    pub base_path: PathBuf,
}

impl Symbols {
    pub fn new(base_path: PathBuf) -> Self {
        Symbols {
            symbols: HashMap::new(),
            base_path,
        }
    }

    /// Get symbol by ID
    pub fn get(&self, id: &str) -> Option<&Symbol> {
        self.symbols.get(id)
    }

    /// Get full path to symbol SVG
    pub fn get_svg_path(&self, id: &str) -> Option<PathBuf> {
        self.symbols
            .get(id)
            .map(|s| self.base_path.join(&s.svg_reference))
    }
}
