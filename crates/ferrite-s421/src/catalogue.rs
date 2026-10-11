//! S-421 Catalogue loader for Route Plugin
//!
//! Loads Feature Catalogue and Portrayal Catalogue for S-421 Route Plan Exchange Format.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

/// S-421 Catalogue status
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum CatalogueStatus {
    /// Not loaded yet
    #[default]
    NotLoaded,
    /// Loading in progress
    Loading,
    /// Successfully loaded
    Loaded {
        version: String,
        feature_count: usize,
    },
    /// Failed to load
    Error(String),
}

/// S-421 Feature Catalogue (simplified)
#[derive(Debug, Clone, Default)]
pub struct S421FeatureCatalogue {
    /// Catalogue version
    pub version: String,
    /// Feature types
    pub feature_types: HashMap<String, FeatureType>,
    /// Simple attributes
    pub simple_attributes: HashMap<String, SimpleAttribute>,
}

/// Feature type definition
#[derive(Debug, Clone)]
pub struct FeatureType {
    pub code: String,
    pub name: String,
    pub definition: String,
    pub geometry_type: String,
}

/// Simple attribute definition
#[derive(Debug, Clone)]
pub struct SimpleAttribute {
    pub code: String,
    pub name: String,
    pub value_type: String,
}

/// S-421 Portrayal Catalogue (simplified)
#[derive(Debug, Clone, Default)]
pub struct S421PortrayalCatalogue {
    /// Catalogue version
    pub version: String,
    /// Symbol references
    pub symbols: HashMap<String, SymbolInfo>,
    /// Line styles
    pub line_styles: HashMap<String, LineStyleInfo>,
    /// Color profiles
    pub colors: HashMap<String, ColorInfo>,
}

/// Symbol information
#[derive(Debug, Clone)]
pub struct SymbolInfo {
    pub id: String,
    pub file_path: String,
}

/// Line style information
#[derive(Debug, Clone)]
pub struct LineStyleInfo {
    pub id: String,
    pub width: f32,
    pub color_token: String,
    /// Dash pattern: [(start, length), ...]
    pub dashes: Vec<(f32, f32)>,
    /// Interval length for dash pattern (0 = solid)
    pub interval_length: f32,
}

/// Color information
#[derive(Debug, Clone)]
pub struct ColorInfo {
    pub token: String,
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Catalogue manager for S-421
pub struct CatalogueManager {
    /// Base path for catalogues (e.g., ./Catalogues)
    pub base_path: PathBuf,
    /// Feature Catalogue
    pub fc: Option<S421FeatureCatalogue>,
    /// Portrayal Catalogue
    pub pc: Option<S421PortrayalCatalogue>,
    /// FC loading status
    pub fc_status: CatalogueStatus,
    /// PC loading status
    pub pc_status: CatalogueStatus,
}

impl Default for CatalogueManager {
    fn default() -> Self {
        Self {
            base_path: PathBuf::from("./Catalogues"),
            fc: None,
            pc: None,
            fc_status: CatalogueStatus::NotLoaded,
            pc_status: CatalogueStatus::NotLoaded,
        }
    }
}

impl CatalogueManager {
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            ..Default::default()
        }
    }

    /// Get FC path
    pub fn fc_path(&self) -> PathBuf {
        self.base_path.join("FC").join("S-421")
    }

    /// Get PC path
    pub fn pc_path(&self) -> PathBuf {
        self.base_path.join("PC").join("S-421")
    }

    /// Check if catalogues exist
    pub fn check_catalogues(&self) -> (bool, bool) {
        let fc_exists = self.find_fc_file().is_some();
        let pc_exists = self.pc_path().join("portrayal_catalogue.xml").exists()
            || self.find_pc_file().is_some();
        (fc_exists, pc_exists)
    }

    /// Find FC XML file
    fn find_fc_file(&self) -> Option<PathBuf> {
        let fc_dir = self.fc_path();
        if !fc_dir.exists() {
            return None;
        }

        // Look for S-421 FC XML file
        for entry in std::fs::read_dir(&fc_dir).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().map(|e| e == "xml").unwrap_or(false) {
                let name = path.file_name()?.to_str()?;
                if name.contains("421") || name.contains("Feature") || name.contains("FC") {
                    return Some(path);
                }
            }
        }
        None
    }

    /// Find PC XML file
    fn find_pc_file(&self) -> Option<PathBuf> {
        let pc_dir = self.pc_path();
        if !pc_dir.exists() {
            return None;
        }

        let portrayal_file = pc_dir.join("portrayal_catalogue.xml");
        if portrayal_file.exists() {
            return Some(portrayal_file);
        }

        // Look for alternative PC XML file
        for entry in std::fs::read_dir(&pc_dir).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().map(|e| e == "xml").unwrap_or(false) {
                return Some(path);
            }
        }
        None
    }

    /// Load Feature Catalogue
    pub fn load_fc(&mut self) -> Result<(), String> {
        self.fc_status = CatalogueStatus::Loading;

        let fc_file = self.find_fc_file().ok_or_else(|| {
            let msg = format!("S-421 FC not found in {:?}", self.fc_path());
            self.fc_status = CatalogueStatus::Error(msg.clone());
            msg
        })?;

        match parse_feature_catalogue(&fc_file) {
            Ok(fc) => {
                let feature_count = fc.feature_types.len();
                let version = fc.version.clone();
                self.fc = Some(fc);
                self.fc_status = CatalogueStatus::Loaded {
                    version,
                    feature_count,
                };
                Ok(())
            }
            Err(e) => {
                let msg = format!("Failed to parse S-421 FC: {}", e);
                self.fc_status = CatalogueStatus::Error(msg.clone());
                Err(msg)
            }
        }
    }

    /// Load Portrayal Catalogue
    pub fn load_pc(&mut self) -> Result<(), String> {
        self.pc_status = CatalogueStatus::Loading;

        let pc_file = self.find_pc_file().ok_or_else(|| {
            let msg = format!("S-421 PC not found in {:?}", self.pc_path());
            self.pc_status = CatalogueStatus::Error(msg.clone());
            msg
        })?;

        match parse_portrayal_catalogue(&pc_file) {
            Ok(pc) => {
                let symbol_count = pc.symbols.len();
                let version = pc.version.clone();
                self.pc = Some(pc);
                self.pc_status = CatalogueStatus::Loaded {
                    version,
                    feature_count: symbol_count,
                };
                Ok(())
            }
            Err(e) => {
                let msg = format!("Failed to parse S-421 PC: {}", e);
                self.pc_status = CatalogueStatus::Error(msg.clone());
                Err(msg)
            }
        }
    }

    /// Load both catalogues
    pub fn load_all(&mut self) -> (Result<(), String>, Result<(), String>) {
        let fc_result = self.load_fc();
        let pc_result = self.load_pc();
        (fc_result, pc_result)
    }

    /// Resolve a PC-declared symbol only. This path is a lookup hint, not a
    /// captured/authenticated resource; consumers must capture bytes before use.
    pub fn get_symbol_path(&self, symbol_ref: &str) -> Option<PathBuf> {
        let symbol = self.pc.as_ref()?.symbols.get(symbol_ref)?;
        let root = self.pc_path().canonicalize().ok()?;
        let symbols = root.join("Symbols");
        let directory = std::fs::symlink_metadata(&symbols).ok()?;
        if !directory.is_dir() || directory.file_type().is_symlink() {
            return None;
        }
        let declared = Path::new(&symbol.file_path);
        let filename = declared.file_name()?.to_str()?;
        if !portable_svg_filename(filename) {
            return None;
        }
        let path = symbols.join(filename);
        // The parsed declaration's directory must agree with this PC owner.
        if declared.parent()?.canonicalize().ok()? != symbols {
            return None;
        }
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return None;
        }
        let canonical = path.canonicalize().ok()?;
        (canonical.parent() == Some(symbols.as_path())).then_some(canonical)
    }

    /// Get color by token
    pub fn get_color(&self, token: &str) -> Option<(u8, u8, u8)> {
        self.pc
            .as_ref()
            .and_then(|pc| pc.colors.get(token))
            .map(|c| (c.r, c.g, c.b))
    }

    /// Get color as RGBA u32 (0xRRGGBBAA format)
    pub fn get_color_rgba(&self, token: &str) -> Option<u32> {
        self.get_color(token)
            .map(|(r, g, b)| ((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | 0xFF)
    }

    /// Get line style by ID
    pub fn get_line_style(&self, id: &str) -> Option<&LineStyleInfo> {
        self.pc.as_ref().and_then(|pc| pc.line_styles.get(id))
    }
}

// Root admission is deliberately separate from generic XML resource safety.
// Legacy published PC blank productId/version remain unknown, never invented.
fn check_catalogue_root(content: &str, feature: bool) -> Result<(), String> {
    let document = roxmltree::Document::parse_with_options(
        content,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 100_000,
        },
    )
    .map_err(|error| error.to_string())?;
    let root = document.root_element();
    let correct = if feature {
        root.tag_name().name() == "S100_FC_FeatureCatalogue"
            && matches!(
                root.tag_name().namespace(),
                Some("http://www.iho.int/S100FC" | "http://www.iho.int/S100FC/5.0")
            )
    } else {
        root.tag_name().name() == "portrayalCatalog" && root.tag_name().namespace().is_none()
    };
    if !correct {
        return Err("Unsupported S421 catalogue root/namespace".into());
    }
    let identifiers = if feature {
        root.children()
            .filter(|node| {
                node.is_element()
                    && node.tag_name().name() == "productId"
                    && node.tag_name().namespace() == root.tag_name().namespace()
            })
            .map(|node| node.text().unwrap_or("").trim())
            .collect::<Vec<_>>()
    } else {
        root.attribute("productId")
            .into_iter()
            .map(str::trim)
            .collect()
    };
    if identifiers.len() > 1
        || identifiers
            .iter()
            .any(|id| !id.is_empty() && !matches!(*id, "S-421" | "S421"))
    {
        return Err("Catalogue productId contradicts S421".into());
    }
    Ok(())
}

/// Parse Feature Catalogue XML
fn parse_feature_catalogue(path: &Path) -> Result<S421FeatureCatalogue, String> {
    let content = crate::s421::read_xml_bounded(path)
        .map_err(|e| format!("Failed to read FC file: {}", e))?;
    check_catalogue_root(&content, true)?;

    let mut reader = Reader::from_str(&content);
    reader.config_mut().trim_text(true);

    let mut fc = S421FeatureCatalogue::default();
    let mut buf = Vec::new();
    let mut current_feature: Option<FeatureType> = None;
    let mut current_attr: Option<SimpleAttribute> = None;
    let mut in_feature_type = false;
    let mut in_simple_attr = false;
    let mut current_element = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                current_element = name.clone();

                match name.as_str() {
                    "S100FC:S100_FC_FeatureType" | "FeatureType" => {
                        in_feature_type = true;
                        current_feature = Some(FeatureType {
                            code: String::new(),
                            name: String::new(),
                            definition: String::new(),
                            geometry_type: String::new(),
                        });
                    }
                    "S100FC:S100_FC_SimpleAttribute" | "SimpleAttribute" => {
                        in_simple_attr = true;
                        current_attr = Some(SimpleAttribute {
                            code: String::new(),
                            name: String::new(),
                            value_type: String::new(),
                        });
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();

                if in_feature_type {
                    if let Some(ref mut ft) = current_feature {
                        match current_element.as_str() {
                            "code" | "S100FC:code" => ft.code = text,
                            "name" | "S100FC:name" => ft.name = text,
                            "definition" | "S100FC:definition" => ft.definition = text,
                            "geometryType" | "S100FC:geometryType" => ft.geometry_type = text,
                            _ => {}
                        }
                    }
                } else if in_simple_attr {
                    if let Some(ref mut attr) = current_attr {
                        match current_element.as_str() {
                            "code" | "S100FC:code" => attr.code = text,
                            "name" | "S100FC:name" => attr.name = text,
                            "valueType" | "S100FC:valueType" => attr.value_type = text,
                            _ => {}
                        }
                    }
                } else {
                    match current_element.as_str() {
                        "versionNumber" | "S100FC:versionNumber" => fc.version = text,
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();

                match name.as_str() {
                    "S100FC:S100_FC_FeatureType" | "FeatureType" => {
                        if let Some(ft) = current_feature.take() {
                            if !ft.code.is_empty() {
                                fc.feature_types.insert(ft.code.clone(), ft);
                            }
                        }
                        in_feature_type = false;
                    }
                    "S100FC:S100_FC_SimpleAttribute" | "SimpleAttribute" => {
                        if let Some(attr) = current_attr.take() {
                            if !attr.code.is_empty() {
                                fc.simple_attributes.insert(attr.code.clone(), attr);
                            }
                        }
                        in_simple_attr = false;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    Ok(fc)
}

// Portable single-file receiver policy; unsupported names are not claimed to
// violate S-421. No separators, ADS, Windows device names or hidden dot names.
fn portable_svg_filename(name: &str) -> bool {
    if name.len() > 255
        || !name.ends_with(".svg")
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return false;
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    !stem.is_empty()
        && !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !(stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

/// Parse Portrayal Catalogue XML
fn parse_portrayal_catalogue(path: &Path) -> Result<S421PortrayalCatalogue, String> {
    let content = crate::s421::read_xml_bounded(path)
        .map_err(|e| format!("Failed to read PC file: {}", e))?;
    check_catalogue_root(&content, false)?;

    let mut reader = Reader::from_str(&content);
    reader.config_mut().trim_text(true);

    let mut pc = S421PortrayalCatalogue::default();
    let document = roxmltree::Document::parse_with_options(
        &content,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 100_000,
        },
    )
    .map_err(|error| error.to_string())?;
    pc.version = document
        .root_element()
        .attribute("version")
        .unwrap_or("")
        .to_owned();
    let mut buf = Vec::new();
    let mut current_element = String::new();

    let pc_dir = path.parent().unwrap_or(Path::new("."));

    // Catalogue declarations are authoritative; a directory census is not.
    // Keep ID and filename distinct: a PC may deliberately use different names.
    for group in document.root_element().children().filter(|node| {
        node.is_element()
            && node.tag_name().namespace().is_none()
            && node.tag_name().name() == "symbols"
    }) {
        for declaration in group.children().filter(|node| {
            node.is_element()
                && node.tag_name().namespace().is_none()
                && node.tag_name().name() == "symbol"
        }) {
            let id = declaration
                .attribute("id")
                .ok_or("Symbol declaration missing ID")?;
            if id.is_empty() || id.trim() != id || id.len() > 255 {
                return Err("Unsupported symbol declaration ID".into());
            }
            let files: Vec<_> = declaration
                .children()
                .filter(|node| {
                    node.is_element()
                        && node.tag_name().namespace().is_none()
                        && node.tag_name().name() == "fileName"
                })
                .collect();
            if files.len() != 1
                || files[0].children().any(|node| node.is_element())
                || files[0].children().filter(|node| node.is_text()).count() != 1
            {
                return Err("Symbol declaration requires one scalar filename".into());
            }
            let filename = files[0].text().unwrap_or("");
            if !portable_svg_filename(filename) {
                return Err("Unsupported symbol resource filename".into());
            }
            let symbol = SymbolInfo {
                id: id.to_owned(),
                file_path: pc_dir
                    .join("Symbols")
                    .join(filename)
                    .to_string_lossy()
                    .into_owned(),
            };
            if pc.symbols.insert(id.to_owned(), symbol).is_some() {
                return Err("Duplicate symbol declaration ID".into());
            }
        }
    }

    // Parse ColorProfile from ColorProfiles directory
    let color_profile_path = pc_dir.join("ColorProfiles").join("colorProfile.xml");
    if color_profile_path.exists() {
        if let Ok(colors) = parse_color_profile(&color_profile_path) {
            pc.colors = colors;
        }
    }

    // Parse LineStyles from LineStyles directory
    let line_styles_dir = pc_dir.join("LineStyles");
    if line_styles_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&line_styles_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().map(|e| e == "xml").unwrap_or(false) {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        // Skip sample files
                        if stem.starts_with("s100") {
                            continue;
                        }
                        if let Ok(style) = parse_line_style(&path, stem) {
                            pc.line_styles.insert(stem.to_string(), style);
                        }
                    }
                }
            }
        }
    }

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                current_element = String::from_utf8_lossy(e.name().as_ref()).to_string();
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                match current_element.as_str() {
                    "versionNumber" | "version" => pc.version = text,
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    Ok(pc)
}

/// Parse ColorProfile XML (Day palette)
fn parse_color_profile(path: &Path) -> Result<HashMap<String, ColorInfo>, String> {
    let content = crate::s421::read_xml_bounded(path)
        .map_err(|e| format!("Failed to read color profile: {}", e))?;

    let mut reader = Reader::from_str(&content);
    reader.config_mut().trim_text(true);

    let mut colors = HashMap::new();
    let mut buf = Vec::new();
    let mut current_element = String::new();
    let mut current_token = String::new();
    let mut in_day_palette = false;
    let mut in_item = false;
    let mut in_srgb = false;
    let mut r: u8 = 0;
    let mut g: u8 = 0;
    let mut b: u8 = 0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                current_element = name.clone();

                match name.as_str() {
                    "palette" => {
                        // Check if it's the Day palette
                        for attr in e.attributes().filter_map(|a| a.ok()) {
                            if attr.key.as_ref() == b"name" {
                                let value = String::from_utf8_lossy(&attr.value).to_string();
                                in_day_palette = value == "Day";
                            }
                        }
                    }
                    "item" if in_day_palette => {
                        in_item = true;
                        // Get token attribute
                        for attr in e.attributes().filter_map(|a| a.ok()) {
                            if attr.key.as_ref() == b"token" {
                                current_token = String::from_utf8_lossy(&attr.value).to_string();
                            }
                        }
                    }
                    "srgb" if in_item => {
                        in_srgb = true;
                        r = 0;
                        g = 0;
                        b = 0;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(e)) => {
                if in_srgb {
                    let text = e.unescape().unwrap_or_default().to_string();
                    match current_element.as_str() {
                        "red" => r = text.parse().unwrap_or(0),
                        "green" => g = text.parse().unwrap_or(0),
                        "blue" => b = text.parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match name.as_str() {
                    "palette" => in_day_palette = false,
                    "item" => {
                        if in_item && !current_token.is_empty() {
                            colors.insert(
                                current_token.clone(),
                                ColorInfo {
                                    token: current_token.clone(),
                                    r,
                                    g,
                                    b,
                                },
                            );
                        }
                        in_item = false;
                        current_token.clear();
                    }
                    "srgb" => in_srgb = false,
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(colors)
}

/// Parse LineStyle XML
fn parse_line_style(path: &Path, id: &str) -> Result<LineStyleInfo, String> {
    let content = crate::s421::read_xml_bounded(path)
        .map_err(|e| format!("Failed to read line style: {}", e))?;

    let mut reader = Reader::from_str(&content);
    reader.config_mut().trim_text(true);

    let mut style = LineStyleInfo {
        id: id.to_string(),
        width: 0.64, // default
        color_token: String::new(),
        dashes: Vec::new(),
        interval_length: 0.0,
    };

    let mut buf = Vec::new();
    let mut current_element = String::new();
    let mut in_pen = false;
    let mut in_dash = false;
    let mut dash_start: f32 = 0.0;
    let mut dash_length: f32 = 0.0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                current_element = name.clone();

                match name.as_str() {
                    "pen" => {
                        in_pen = true;
                        // Get width attribute
                        for attr in e.attributes().filter_map(|a| a.ok()) {
                            if attr.key.as_ref() == b"width" {
                                let value = String::from_utf8_lossy(&attr.value).to_string();
                                style.width = value.parse().unwrap_or(0.64);
                            }
                        }
                    }
                    "dash" => {
                        in_dash = true;
                        dash_start = 0.0;
                        dash_length = 0.0;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                match current_element.as_str() {
                    "color" if in_pen => style.color_token = text,
                    "intervalLength" => style.interval_length = text.parse().unwrap_or(0.0),
                    "start" if in_dash => dash_start = text.parse().unwrap_or(0.0),
                    "length" if in_dash => dash_length = text.parse().unwrap_or(0.0),
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match name.as_str() {
                    "pen" => in_pen = false,
                    "dash" => {
                        if in_dash && dash_length > 0.0 {
                            style.dashes.push((dash_start, dash_length));
                        }
                        in_dash = false;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(style)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalogue_manager_default() {
        let mgr = CatalogueManager::default();
        assert_eq!(mgr.fc_status, CatalogueStatus::NotLoaded);
        assert_eq!(mgr.pc_status, CatalogueStatus::NotLoaded);
    }

    struct PrivateXml {
        directory: PathBuf,
        file: PathBuf,
    }
    impl PrivateXml {
        fn new(raw: &[u8]) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            loop {
                let directory = std::env::temp_dir().join(format!(
                    "ferrite-s421-catalogue-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                match std::fs::create_dir(&directory) {
                    Ok(()) => {
                        let file = directory.join("input.xml");
                        std::fs::write(&file, raw).unwrap();
                        return Self { directory, file };
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("{error}"),
                }
            }
        }
    }
    impl Drop for PrivateXml {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
    #[test]
    fn published_fc_is_catalogue_not_route_and_retains_original_version() {
        let raw = include_bytes!("../tests/fixtures/published-FC.xml");
        let input = PrivateXml::new(raw);
        let fc = parse_feature_catalogue(&input.file).unwrap();
        assert_eq!(fc.version, "1.0.0");
        assert!(fc.feature_types.contains_key("Route"));
        assert!(fc.feature_types.contains_key("RouteWaypoint"));
        assert_eq!(std::fs::read(&input.file).unwrap().as_slice(), &raw[..]);
    }
    #[test]
    fn published_pc_blank_metadata_stays_blank_without_route_validation() {
        let raw = include_bytes!("../tests/fixtures/published-PC.xml");
        let input = PrivateXml::new(raw);
        let pc = parse_portrayal_catalogue(&input.file).unwrap();
        assert!(pc.version.is_empty());
        assert_eq!(std::fs::read(&input.file).unwrap().as_slice(), &raw[..]);
    }
    #[test]
    fn generic_resource_xml_and_catalogue_roots_are_separate() {
        let input = PrivateXml::new(b"<colorProfile/>");
        assert!(crate::s421::read_xml_bounded(&input.file).is_ok());
        assert!(parse_feature_catalogue(&input.file).is_err());
        assert!(parse_portrayal_catalogue(&input.file).is_err());
        for xml in [
            "<S100_FC_FeatureCatalogue xmlns='http://foreign.invalid'/>",
            "<portrayalCatalog productId='S-101'/>",
            "<portrayalCatalog xmlns='http://foreign.invalid'/>",
        ] {
            assert!(check_catalogue_root(xml, false).is_err());
        }
        assert!(check_catalogue_root("<S100_FC_FeatureCatalogue xmlns='http://www.iho.int/S100FC'><productId>S-101</productId></S100_FC_FeatureCatalogue>",true).is_err());
    }
    #[test]
    fn generic_xml_still_rejects_dtd_malformed_entity_and_deep_tree() {
        for raw in [
            "<!DOCTYPE x [<!ENTITY e 'x'>]><x>&e;</x>".to_owned(),
            "<x>&unknown;</x>".to_owned(),
            format!("{}{}", "<x>".repeat(129), "</x>".repeat(129)),
        ] {
            let input = PrivateXml::new(raw.as_bytes());
            assert!(crate::s421::read_xml_bounded(&input.file).is_err());
        }
    }
    fn symbol_pc(xml: &str) -> (PrivateXml, CatalogueManager) {
        let input = PrivateXml::new(b"unused");
        let pc_dir = input.directory.join("PC/S-421");
        std::fs::create_dir_all(pc_dir.join("Symbols")).unwrap();
        let file = pc_dir.join("portrayal_catalogue.xml");
        std::fs::write(&file, xml).unwrap();
        let mut manager = CatalogueManager::new(input.directory.clone());
        manager.load_pc().unwrap();
        (input, manager)
    }
    #[test]
    fn only_declared_symbol_id_resolves_its_declared_filename() {
        let (input, manager) = symbol_pc("<portrayalCatalog><symbols><symbol id='MARKER'><fileName>actual.svg</fileName></symbol></symbols></portrayalCatalog>");
        let symbols = input.directory.join("PC/S-421/Symbols");
        std::fs::write(symbols.join("actual.svg"), "<svg/>").unwrap();
        std::fs::write(symbols.join("EXTRA.svg"), "<svg/>").unwrap();
        assert_eq!(manager.pc.as_ref().unwrap().symbols.len(), 1);
        assert_eq!(
            manager.get_symbol_path("MARKER"),
            Some(symbols.join("actual.svg").canonicalize().unwrap())
        );
        assert!(manager.get_symbol_path("actual").is_none());
        assert!(manager.get_symbol_path("EXTRA").is_none());
        assert!(manager.get_symbol_path("../input").is_none());
    }
    #[test]
    fn ambiguous_and_escaping_symbol_declarations_are_rejected() {
        for filename in [
            "../outside.svg",
            "/outside.svg",
            "C:evil.svg",
            "a\\b.svg",
            "CON.svg",
            "LPT1.svg",
            ".svg",
            "a.svg ",
        ] {
            let input = PrivateXml::new(format!("<portrayalCatalog><symbols><symbol id='X'><fileName>{filename}</fileName></symbol></symbols></portrayalCatalog>").as_bytes());
            assert!(
                parse_portrayal_catalogue(&input.file).is_err(),
                "{filename}"
            );
        }
        for declaration in ["<symbol id='X'><fileName>a.svg</fileName></symbol><symbol id='X'><fileName>b.svg</fileName></symbol>", "<symbol id='X'><fileName>a.svg</fileName><fileName>b.svg</fileName></symbol>", "<symbol id='X'><fileName><name>a.svg</name></fileName></symbol>"] {
            let input = PrivateXml::new(format!("<portrayalCatalog><symbols>{declaration}</symbols></portrayalCatalog>").as_bytes());
            assert!(parse_portrayal_catalogue(&input.file).is_err());
        }
    }
    #[cfg(unix)]
    #[test]
    fn symbol_file_and_directory_symlinks_cannot_escape_owner() {
        let (input, manager) = symbol_pc("<portrayalCatalog><symbols><symbol id='X'><fileName>actual.svg</fileName></symbol></symbols></portrayalCatalog>");
        let symbols = input.directory.join("PC/S-421/Symbols");
        std::os::unix::fs::symlink(&input.file, symbols.join("actual.svg")).unwrap();
        assert!(manager.get_symbol_path("X").is_none());
        std::fs::remove_file(symbols.join("actual.svg")).unwrap();
        std::fs::remove_dir(&symbols).unwrap();
        std::os::unix::fs::symlink(&input.directory, &symbols).unwrap();
        assert!(manager.get_symbol_path("X").is_none());
    }
}
