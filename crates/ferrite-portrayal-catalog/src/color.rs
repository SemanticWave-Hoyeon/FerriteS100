//! Color profile definitions

use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// sRGB color value
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SrgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl SrgbColor {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        SrgbColor { r, g, b }
    }

    /// Convert to hex string
    pub fn to_hex(&self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }

    /// Parse from hex string
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.trim_start_matches('#');
        if s.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(SrgbColor { r, g, b })
    }
}

/// CIE color value
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CieColor {
    pub x: f64,
    pub y: f64,
    pub l: f64,
}

/// Color definition (can be sRGB or CIE)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorDefinition {
    pub token: String,
    pub srgb: Option<SrgbColor>,
    pub cie: Option<CieColor>,
}

impl ColorDefinition {
    /// Get sRGB color (convert from CIE if needed)
    pub fn get_srgb(&self) -> Option<SrgbColor> {
        self.srgb
    }
}

/// Color profile (e.g., Day, Dusk, Night)
#[derive(Debug, Clone, Default)]
pub struct ColorProfile {
    pub id: String,
    pub name: String,
    pub colors: HashMap<String, ColorDefinition>,
}

impl ColorProfile {
    pub fn new(id: String, name: String) -> Self {
        ColorProfile {
            id,
            name,
            colors: HashMap::new(),
        }
    }

    /// Get color by token
    pub fn get_color(&self, token: &str) -> Option<&ColorDefinition> {
        self.colors.get(token)
    }

    /// Get sRGB color by token
    pub fn get_srgb(&self, token: &str) -> Option<SrgbColor> {
        self.colors.get(token).and_then(|c| c.get_srgb())
    }
}

/// Collection of color profiles
#[derive(Debug, Clone, Default)]
pub struct ColorProfiles {
    pub profiles: HashMap<String, ColorProfile>,
    pub default_profile: Option<String>,
}

impl ColorProfiles {
    pub fn new() -> Self {
        ColorProfiles::default()
    }

    /// Get profile by ID
    pub fn get_profile(&self, id: &str) -> Option<&ColorProfile> {
        self.profiles.get(id)
    }

    /// Get default profile
    pub fn get_default(&self) -> Option<&ColorProfile> {
        self.default_profile
            .as_ref()
            .and_then(|id| self.profiles.get(id))
    }

    /// Get color from any profile
    pub fn get_color(&self, profile_id: &str, token: &str) -> Option<&ColorDefinition> {
        self.profiles.get(profile_id).and_then(|p| p.get_color(token))
    }
}
