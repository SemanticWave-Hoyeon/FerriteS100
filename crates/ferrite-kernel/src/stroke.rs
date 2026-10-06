//! Product-neutral authored stroke properties, before catalogue colour resolution.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StrokeCap {
    #[default]
    Butt,
    Round,
    Square,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StrokeJoin {
    #[default]
    Miter,
    Round,
    Bevel,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LineSymbolCrs {
    #[default]
    LocalCRS,
    LineCRS,
    PortrayalCRS,
}
impl std::str::FromStr for LineSymbolCrs {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "LocalCRS" => Ok(Self::LocalCRS),
            "LineCRS" => Ok(Self::LineCRS),
            "PortrayalCRS" => Ok(Self::PortrayalCRS),
            _ => Err("Invalid LineSymbol CRS"),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrokeSymbol {
    pub reference: String,
    pub position_mm: f64,
    pub rotation_degrees: f64,
    pub crs: LineSymbolCrs,
    pub scale_factor: f64,
}
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StrokeDefinition {
    pub width_mm: f32,
    pub color_token: String,
    pub transparency: f32,
    pub dash_cycle: Option<crate::DashCycle>,
    #[serde(default)]
    pub cap: StrokeCap,
    #[serde(default)]
    pub join: StrokeJoin,
    #[serde(default)]
    pub offset_mm: f64,
    #[serde(default)]
    pub interval_length_mm: f64,
    #[serde(default)]
    pub symbols: Vec<StrokeSymbol>,
}
