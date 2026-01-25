//! S-421 Catalogue loader for Route Plugin
//!
//! Loads Feature Catalogue and Portrayal Catalogue for S-421 Route Plan Exchange Format.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

/// S-421 Catalogue status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CatalogueStatus {
    /// Not loaded yet
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

impl Default for CatalogueStatus {
    fn default() -> Self {
        Self::NotLoaded
    }
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

    /// Get symbol file path
    pub fn get_symbol_path(&self, symbol_ref: &str) -> Option<PathBuf> {
        let symbols_dir = self.pc_path().join("Symbols");
        let path = symbols_dir.join(format!("{}.svg", symbol_ref));
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    /// Get color by token
    pub fn get_color(&self, token: &str) -> Option<(u8, u8, u8)> {
        self.pc
            .as_ref()
            .and_then(|pc| pc.colors.get(token))
            .map(|c| (c.r, c.g, c.b))
    }
}

/// Parse Feature Catalogue XML
fn parse_feature_catalogue(path: &Path) -> Result<S421FeatureCatalogue, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read FC file: {}", e))?;

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

/// Parse Portrayal Catalogue XML
fn parse_portrayal_catalogue(path: &Path) -> Result<S421PortrayalCatalogue, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read PC file: {}", e))?;

    let mut reader = Reader::from_str(&content);
    reader.config_mut().trim_text(true);

    let mut pc = S421PortrayalCatalogue::default();
    let mut buf = Vec::new();
    let mut current_element = String::new();

    // Also scan for symbol files in the Symbols directory
    let symbols_dir = path.parent().unwrap_or(Path::new(".")).join("Symbols");
    if symbols_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&symbols_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().map(|e| e == "svg").unwrap_or(false) {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        pc.symbols.insert(
                            stem.to_string(),
                            SymbolInfo {
                                id: stem.to_string(),
                                file_path: path.display().to_string(),
                            },
                        );
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

    // Set default version if not found
    if pc.version.is_empty() {
        pc.version = "1.0.0".to_string();
    }

    Ok(pc)
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
}
