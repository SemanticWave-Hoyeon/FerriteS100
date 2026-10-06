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
#[derive(Debug, Clone)]
pub struct LineSymbol {
    pub reference: String,
    pub position: f64,
    pub rotation: f64,
    pub crs_type: ferrite_kernel::LineSymbolCrs,
    pub scale_factor: f64,
}
impl Default for LineSymbol {
    fn default() -> Self {
        Self {
            reference: String::new(),
            position: 0.,
            rotation: 0.,
            crs_type: Default::default(),
            scale_factor: 1.,
        }
    }
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
    /// Millimetres, per S-100 Part 9-12.2.2.4.
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
    pub offset_mm: f64,
    pub pen: Pen,
    pub dashes: Vec<Dash>,
    pub symbols: Vec<LineSymbol>,
}

impl SimpleLineStyle {
    /// Check if this is a solid line (no dash)
    pub fn is_solid(&self) -> bool {
        self.dashes.is_empty()
    }

    /// Preserve S-100 start positions; alternating arrays cannot encode an initial gap safely.
    pub fn dash_cycle(&self) -> Result<Option<ferrite_kernel::DashCycle>, &'static str> {
        if self.dashes.is_empty() {
            Ok(None)
        } else {
            ferrite_kernel::DashCycle::new(
                self.interval_length,
                self.dashes.iter().map(|d| (d.start, d.length)),
            )
            .map(Some)
        }
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

#[cfg(test)]
mod dash_cycle_tests {
    use super::*;
    #[test]
    fn pc_initial_gap_and_multiple_dashes_keep_their_positions() {
        let style = SimpleLineStyle {
            interval_length: 10.,
            dashes: vec![
                Dash {
                    start: 2.,
                    length: 2.,
                },
                Dash {
                    start: 6.,
                    length: 1.,
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            style.dash_cycle().unwrap().unwrap().intervals,
            vec![(2., 4.), (6., 7.)]
        );
    }
}
