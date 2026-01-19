//! Line style definitions
//!
//! Parsed from XML like:
//! ```xml
//! <ls:lineStyle>
//!    <intervalLength>32.3</intervalLength>
//!    <pen width="0.32">
//!       <color>CHMGD</color>
//!    </pen>
//!    <dash>
//!       <start>2</start>
//!       <length>6</length>
//!    </dash>
//!    <symbol reference="EMAREMG1">
//!       <position>5</position>
//!    </symbol>
//! </ls:lineStyle>
//! ```

use serde::{Deserialize, Serialize};

/// Line cap style
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum CapStyle {
    #[default]
    Butt,
    Round,
    Square,
}

/// Line join style
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum JoinStyle {
    #[default]
    Miter,
    Round,
    Bevel,
}

/// Dash element from XML (start position and length)
#[derive(Debug, Clone, Default)]
pub struct Dash {
    pub start: f64,
    pub length: f64,
}

/// Symbol on line from XML
#[derive(Debug, Clone, Default)]
pub struct LineSymbol {
    pub reference: String,
    pub position: f64,
}

/// Dash pattern element (for rendering - gap-based)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashElement {
    pub length: f64,
    pub gap: f64,
}

/// Pen specification
#[derive(Debug, Clone, Default)]
pub struct Pen {
    pub width: f64,
    pub color_token: String,
    pub cap_style: CapStyle,
    pub join_style: JoinStyle,
}

/// Simple line style (from XML lineStyle element)
#[derive(Debug, Clone, Default)]
pub struct SimpleLineStyle {
    pub id: String,
    pub interval_length: f64,
    pub pen: Pen,
    pub dashes: Vec<Dash>,
    pub symbols: Vec<LineSymbol>,
}

impl SimpleLineStyle {
    /// Check if this is a solid line (no dash)
    pub fn is_solid(&self) -> bool {
        self.dashes.is_empty()
    }

    /// Get dash array for rendering (converts start/length to gap-based)
    pub fn dash_array(&self) -> Vec<f64> {
        if self.dashes.is_empty() {
            return Vec::new();
        }

        // Convert start/length format to length/gap format
        let mut result = Vec::new();
        let mut sorted_dashes: Vec<_> = self.dashes.iter().collect();
        sorted_dashes.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));

        let mut current_pos = 0.0;
        for dash in sorted_dashes {
            let gap = dash.start - current_pos;
            if gap > 0.0 {
                result.push(gap); // gap before dash
            }
            result.push(dash.length); // dash length
            current_pos = dash.start + dash.length;
        }

        // Final gap to complete the interval
        if self.interval_length > current_pos {
            result.push(self.interval_length - current_pos);
        }

        result
    }
}

/// Complex line style (multiple strokes)
#[derive(Debug, Clone, Default)]
pub struct ComplexLineStyle {
    pub id: String,
    pub strokes: Vec<SimpleLineStyle>,
}

/// Composite line style (from XML compositeLineStyle)
#[derive(Debug, Clone, Default)]
pub struct CompositeLineStyle {
    pub id: String,
    pub components: Vec<SimpleLineStyle>,
}

/// Line style (any type)
#[derive(Debug, Clone)]
pub enum LineStyle {
    Simple(SimpleLineStyle),
    Complex(ComplexLineStyle),
    Composite(CompositeLineStyle),
}

impl LineStyle {
    pub fn id(&self) -> &str {
        match self {
            LineStyle::Simple(s) => &s.id,
            LineStyle::Complex(c) => &c.id,
            LineStyle::Composite(c) => &c.id,
        }
    }
}
