//! Color handling for rendering
//!
//! Provides color types optimized for GPU rendering.

/// RGBA color with f32 components (0.0 - 1.0)
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const WHITE: Color = Color::rgb(1.0, 1.0, 1.0);
    pub const BLACK: Color = Color::rgb(0.0, 0.0, 0.0);
    pub const RED: Color = Color::rgb(1.0, 0.0, 0.0);
    pub const GREEN: Color = Color::rgb(0.0, 1.0, 0.0);
    pub const BLUE: Color = Color::rgb(0.0, 0.0, 1.0);
    pub const TRANSPARENT: Color = Color::rgba(0.0, 0.0, 0.0, 0.0);

    /// Create RGB color with full opacity
    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Color { r, g, b, a: 1.0 }
    }

    /// Create RGBA color
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Color { r, g, b, a }
    }

    /// Create color from u8 components (0-255)
    pub fn from_u8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: a as f32 / 255.0,
        }
    }

    /// Create color from hex string (e.g., "#FF0000" or "FF0000")
    pub fn from_hex(hex: &str) -> Option<Self> {
        let hex = hex.trim_start_matches('#');

        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color::from_u8(r, g, b, 255))
        } else if hex.len() == 8 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(Color::from_u8(r, g, b, a))
        } else {
            None
        }
    }

    /// Convert to u8 array [r, g, b, a]
    pub fn to_u8_array(&self) -> [u8; 4] {
        [
            (self.r * 255.0) as u8,
            (self.g * 255.0) as u8,
            (self.b * 255.0) as u8,
            (self.a * 255.0) as u8,
        ]
    }

    /// Convert to f32 array for GPU
    pub fn to_array(&self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }

    /// Blend with another color using alpha
    pub fn blend(&self, other: &Color, alpha: f32) -> Color {
        let inv_alpha = 1.0 - alpha;
        Color {
            r: self.r * inv_alpha + other.r * alpha,
            g: self.g * inv_alpha + other.g * alpha,
            b: self.b * inv_alpha + other.b * alpha,
            a: self.a * inv_alpha + other.a * alpha,
        }
    }

    /// Apply transparency
    pub fn with_alpha(&self, alpha: f32) -> Color {
        Color {
            r: self.r,
            g: self.g,
            b: self.b,
            a: self.a * alpha,
        }
    }
}

impl Default for Color {
    fn default() -> Self {
        Color::BLACK
    }
}

impl From<ferrite_portrayal_catalog::SrgbColor> for Color {
    fn from(srgb: ferrite_portrayal_catalog::SrgbColor) -> Self {
        Color::from_u8(srgb.r, srgb.g, srgb.b, 255)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_hex() {
        let color = Color::from_hex("#FF8000").unwrap();
        assert_eq!(color.to_u8_array(), [255, 128, 0, 255]);
    }

    #[test]
    fn test_from_u8() {
        let color = Color::from_u8(128, 64, 32, 255);
        assert!((color.r - 0.502).abs() < 0.01);
        assert!((color.g - 0.251).abs() < 0.01);
        assert!((color.b - 0.125).abs() < 0.01);
    }
}
