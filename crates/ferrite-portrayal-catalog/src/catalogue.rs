//! Portrayal Catalogue container and loading

use crate::{BoundPortrayalCatalogue, CatalogueSources};
use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::{
    AreaFill, ColorDefinition, ColorProfile, ColorProfiles, ContextParamType, ContextParameter,
    DisplayMode, DisplayModes, LineStyle, PCError, PortrayalRules, Result, RuleFile, RuleType,
    SrgbColor, Symbol, Symbols, VectorPoint, ViewingGroup, ViewingGroupLayer, ViewingGroupLayers,
    ViewingGroups,
};

/// Portrayal Catalogue
#[derive(Debug)]
pub struct PortrayalCatalogue {
    pub root_path: PathBuf,
    pub product_id: String,
    pub version: String,
    pub color_profiles: ColorProfiles,
    pub symbols: Symbols,
    pub line_styles: HashMap<String, LineStyle>,
    pub area_fills: HashMap<String, AreaFill>,
    pub viewing_groups: ViewingGroups,
    pub viewing_group_layers: ViewingGroupLayers,
    pub display_modes: DisplayModes,
    pub display_planes: crate::DisplayPlanes,
    pub foundation_mode: Vec<u32>,
    pub rules: PortrayalRules,
}

impl PortrayalCatalogue {
    /// Maximum allowed XML file size (50 MB)
    /// Security: Prevents resource exhaustion from oversized files
    const MAX_XML_SIZE: u64 = 50 * 1024 * 1024;

    /// Load Portrayal Catalogue from directory
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Self::load_bound(path)?.catalogue)
    }

    pub fn load_bound<P: AsRef<Path>>(path: P) -> Result<BoundPortrayalCatalogue> {
        let sources = CatalogueSources::capture(path.as_ref())?;
        let catalogue = Self::parse_sources(path.as_ref(), &sources)?;
        Ok(BoundPortrayalCatalogue { catalogue, sources })
    }

    fn parse_sources(path: &Path, sources: &CatalogueSources) -> Result<Self> {
        tracing::info!("Loading Portrayal Catalogue: {}", path.display());

        let mut catalogue = PortrayalCatalogue {
            root_path: path.to_path_buf(),
            product_id: String::new(),
            version: String::new(),
            color_profiles: ColorProfiles::new(),
            symbols: Symbols::new(path.join("Symbols")),
            line_styles: HashMap::new(),
            area_fills: HashMap::new(),
            viewing_groups: ViewingGroups::new(),
            viewing_group_layers: ViewingGroupLayers::new(),
            display_modes: DisplayModes::new(),
            display_planes: crate::DisplayPlanes::default(),
            foundation_mode: Vec::new(),
            rules: PortrayalRules::new(path.join("Rules")),
        };

        // Find and parse portrayal_catalogue.xml
        let catalogue_file = path.join("portrayal_catalogue.xml");
        if sources.contains_path(&catalogue_file) {
            catalogue.parse_catalogue_xml(&catalogue_file, sources)?;
        }

        // Load color profiles
        let colors_dir = path.join("ColorProfiles");
        if sources.has_directory(&colors_dir) {
            catalogue.load_color_profiles(&colors_dir, sources)?;
        }

        // Load symbols
        let symbols_dir = path.join("Symbols");
        if sources.has_directory(&symbols_dir) {
            catalogue.load_symbols(&symbols_dir, sources)?;
        }

        // Load line styles
        let lines_dir = path.join("LineStyles");
        if sources.has_directory(&lines_dir) {
            catalogue.load_line_styles(&lines_dir, sources)?;
        }

        // Load area fills
        let fills_dir = path.join("AreaFills");
        if sources.has_directory(&fills_dir) {
            catalogue.load_area_fills(&fills_dir, sources)?;
        }

        // Load rules
        let rules_dir = path.join("Rules");
        if sources.has_directory(&rules_dir) {
            catalogue.load_rules(&rules_dir, sources)?;
        }

        tracing::info!(
            "PC loaded: {} color profiles, {} symbols, {} line styles, {} area fills",
            catalogue.color_profiles.profiles.len(),
            catalogue.symbols.symbols.len(),
            catalogue.line_styles.len(),
            catalogue.area_fills.len()
        );

        Ok(catalogue)
    }

    /// Parse main catalogue XML
    ///
    /// Security: File size is checked to prevent resource exhaustion
    fn parse_catalogue_xml(&mut self, path: &Path, sources: &CatalogueSources) -> Result<()> {
        let bytes = sources.read_path(path)?;
        if bytes.len() as u64 > Self::MAX_XML_SIZE {
            return Err(PCError::InvalidValue("PC XML byte limit exceeded".into()));
        }
        let reader = BufReader::new(bytes.as_ref());
        let mut xml_reader = Reader::from_reader(reader);
        xml_reader.config_mut().trim_text(true);

        let mut buf = Vec::new();

        loop {
            match xml_reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    let local_name = get_local_name(e);
                    match local_name.as_str() {
                        // Root element - read productId and version from attributes
                        "portrayalCatalog" | "portrayalCatalogue" => {
                            if let Some(pid) = get_attribute(e, "productId") {
                                self.product_id = pid;
                            }
                            if let Some(ver) = get_attribute(e, "version") {
                                self.version = ver;
                            }
                        }
                        // Also support productId/version as child elements (older format)
                        "productId" => {
                            if self.product_id.is_empty() {
                                self.product_id = read_text_content(&mut xml_reader)?;
                            }
                        }
                        "version" | "versionNumber" => {
                            if self.version.is_empty() {
                                self.version = read_text_content(&mut xml_reader)?;
                            }
                        }
                        "foundationMode" | "viewingGroups" | "viewingGroupLayers" => {
                            let mut skipped = Vec::new();
                            xml_reader.read_to_end_into(e.name(), &mut skipped)?;
                        }
                        "viewingGroup" => {
                            let attr_id = get_attribute(e, "id").unwrap_or_default();
                            let mut vg = self.parse_viewing_group(&mut xml_reader)?;
                            if vg.id == 0 {
                                if let Ok(id) = attr_id.parse::<u32>() {
                                    vg.id = id;
                                }
                            }
                            self.viewing_groups.groups.insert(vg.id, vg);
                        }
                        "viewingGroupLayer" => {
                            let attr_id = get_attribute(e, "id").unwrap_or_default();
                            let mut vgl = self.parse_viewing_group_layer(&mut xml_reader)?;
                            if vgl.id.is_empty() && !attr_id.is_empty() {
                                vgl.id = attr_id;
                            }
                            self.viewing_group_layers.layers.insert(vgl.id.clone(), vgl);
                        }
                        "displayMode" => {
                            let attr_id = get_attribute(e, "id").unwrap_or_default();
                            let mut dm = self.parse_display_mode(&mut xml_reader)?;
                            if dm.id.is_empty() && !attr_id.is_empty() {
                                dm.id = attr_id;
                            }
                            self.display_modes.modes.insert(dm.id.clone(), dm);
                        }
                        "context" => {
                            self.parse_context(&mut xml_reader)?;
                        }
                        "parameter" => {
                            // Handle parameter outside context element (shouldn't happen but be safe)
                            if let Some(id) = get_attribute(e, "id") {
                                let param = self.parse_context_parameter(&mut xml_reader, &id)?;
                                self.rules.context_parameters.insert(id, param);
                            }
                        }
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        let xml = crate::context_validation::decode_metadata_xml(bytes.as_ref().to_vec())?;
        let doc = roxmltree::Document::parse(&xml)
            .map_err(|e| PCError::InvalidValue(format!("PC metadata XML: {e}")))?;
        crate::context_validation::read_metadata_document(
            &doc,
            &mut self.rules.context_parameters,
        )?;
        let (groups, layers) = crate::viewing_metadata::read(&doc)?;
        self.viewing_groups = groups;
        self.viewing_group_layers = layers;
        self.foundation_mode = crate::viewing::read_foundation_mode(&doc, &self.viewing_groups)?;
        self.display_planes = crate::display_plane::read(&doc)?;

        Ok(())
    }

    /// Parse context section with context parameters
    fn parse_context<R: std::io::BufRead>(&mut self, reader: &mut Reader<R>) -> Result<()> {
        let mut buf = Vec::new();
        let mut depth = 1;

        loop {
            match reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    depth += 1;
                    let local_name = get_local_name(e);
                    if local_name == "parameter" {
                        if let Some(id) = get_attribute(e, "id") {
                            let param = self.parse_context_parameter(reader, &id)?;
                            self.rules.context_parameters.insert(id, param);
                            depth -= 1; // parse_context_parameter consumes the </parameter>
                        }
                    }
                }
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        tracing::debug!(
            "Loaded {} context parameters from PC",
            self.rules.context_parameters.len()
        );
        Ok(())
    }

    /// Parse context parameter element
    fn parse_context_parameter<R: std::io::BufRead>(
        &self,
        reader: &mut Reader<R>,
        id: &str,
    ) -> Result<ContextParameter> {
        let mut param = ContextParameter {
            id: id.to_string(),
            param_type: ContextParamType::String,
            default_value: None,
            description: None,
            enable: None,
            constraints: Vec::new(),
            validations: Vec::new(),
        };

        let mut buf = Vec::new();
        let mut depth = 1;

        loop {
            match reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    depth += 1;
                    let local_name = get_local_name(e);
                    match local_name.as_str() {
                        "type" => {
                            let type_str = read_text_content(reader)?;
                            param.param_type = match type_str.as_str() {
                                "Boolean" => ContextParamType::Boolean,
                                "Integer" => ContextParamType::Integer,
                                "Double" => ContextParamType::Double,
                                "String" => ContextParamType::String,
                                "Date" => ContextParamType::Date,
                                "Enumeration" => ContextParamType::Enumeration,
                                _ => ContextParamType::String,
                            };
                            depth -= 1;
                        }
                        "default" => {
                            param.default_value = Some(read_text_content(reader)?);
                            depth -= 1;
                        }
                        "name" => {
                            // Inside description block - just the name
                            param.description = Some(read_text_content(reader)?);
                            depth -= 1;
                        }
                        _ => {}
                    }
                }
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        Ok(param)
    }

    /// Parse viewing group element
    fn parse_viewing_group<R: std::io::BufRead>(
        &self,
        reader: &mut Reader<R>,
    ) -> Result<ViewingGroup> {
        let mut vg = ViewingGroup {
            id: 0,
            name: String::new(),
            description: None,
            parent_id: None,
            catalogue_id: String::new(),
        };

        let mut buf = Vec::new();
        let mut depth = 1;

        loop {
            match reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    depth += 1;
                    let local_name = get_local_name(e);
                    match local_name.as_str() {
                        "id" | "viewingGroup" => {
                            let val = read_text_content(reader)?;
                            vg.id = val.parse().unwrap_or(0);
                            depth -= 1; // read_text_content consumes End event
                        }
                        "name" => {
                            vg.name = read_text_content(reader)?;
                            depth -= 1;
                        }
                        "description" => {
                            vg.description = Some(read_text_content(reader)?);
                            depth -= 1;
                        }
                        _ => {}
                    }
                }
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        Ok(vg)
    }

    /// Parse viewing group layer element
    fn parse_viewing_group_layer<R: std::io::BufRead>(
        &self,
        reader: &mut Reader<R>,
    ) -> Result<ViewingGroupLayer> {
        let mut vgl = ViewingGroupLayer {
            id: String::new(),
            name: String::new(),
            viewing_group_ids: Vec::new(),
            default_on: true,
        };

        let mut buf = Vec::new();
        let mut depth = 1;

        loop {
            match reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    depth += 1;
                    let local_name = get_local_name(e);
                    match local_name.as_str() {
                        "id" => {
                            vgl.id = read_text_content(reader)?;
                            depth -= 1; // read_text_content consumes End event
                        }
                        "name" => {
                            vgl.name = read_text_content(reader)?;
                            depth -= 1;
                        }
                        "viewingGroup" => {
                            let val = read_text_content(reader)?;
                            if let Ok(id) = val.parse() {
                                vgl.viewing_group_ids.push(id);
                            }
                            depth -= 1;
                        }
                        "defaultOn" => {
                            vgl.default_on = read_text_content(reader)? == "true";
                            depth -= 1;
                        }
                        _ => {}
                    }
                }
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        Ok(vgl)
    }

    /// Parse display mode element
    fn parse_display_mode<R: std::io::BufRead>(
        &self,
        reader: &mut Reader<R>,
    ) -> Result<DisplayMode> {
        let mut dm = DisplayMode {
            id: String::new(),
            name: String::new(),
            viewing_group_layers: Vec::new(),
        };

        let mut buf = Vec::new();
        let mut depth = 1;

        loop {
            match reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    depth += 1;
                    let local_name = get_local_name(e);
                    match local_name.as_str() {
                        "id" => {
                            dm.id = read_text_content(reader)?;
                            depth -= 1; // read_text_content consumes End event
                        }
                        "name" => {
                            dm.name = read_text_content(reader)?;
                            depth -= 1;
                        }
                        "viewingGroupLayer" => {
                            dm.viewing_group_layers.push(read_text_content(reader)?);
                            depth -= 1;
                        }
                        _ => {}
                    }
                }
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        Ok(dm)
    }

    /// Load color profiles from directory
    /// Loads all palettes (Day, Dusk, Night) as separate profiles
    fn load_color_profiles(&mut self, dir: &Path, sources: &CatalogueSources) -> Result<()> {
        for path in sources.children(dir)? {
            if path.extension().is_some_and(|e| e == "xml") {
                // Parse all palettes from the XML file
                let profiles = self.parse_color_profiles_all(&path, sources)?;
                for profile in profiles {
                    tracing::debug!(
                        "Loaded color profile '{}' with {} colors",
                        profile.name,
                        profile.colors.len()
                    );
                    self.color_profiles
                        .profiles
                        .insert(profile.name.clone(), profile);
                }
            }
        }
        // Set Day as default profile
        if self.color_profiles.profiles.contains_key("Day") {
            self.color_profiles.default_profile = Some("Day".to_string());
        }
        Ok(())
    }

    /// Parse color profile XML - returns ALL palettes (Day, Dusk, Night)
    ///
    /// colorProfile.xml structure:
    /// ```xml
    /// <colorProfile>
    ///   <colors>
    ///     <color token="DEPVS" name="medium_blue">...</color>
    ///   </colors>
    ///   <palette name="Day">
    ///     <item token="DEPVS">
    ///       <srgb><red>R</red><green>G</green><blue>B</blue></srgb>
    ///     </item>
    ///   </palette>
    ///   <palette name="Dusk">...</palette>
    ///   <palette name="Night">...</palette>
    /// </colorProfile>
    /// ```
    fn parse_color_profiles_all(
        &self,
        path: &Path,
        sources: &CatalogueSources,
    ) -> Result<Vec<ColorProfile>> {
        let bytes = sources.read_path(path)?;
        let reader = BufReader::new(bytes.as_ref());
        let mut xml_reader = Reader::from_reader(reader);
        xml_reader.config_mut().trim_text(true);

        let mut profiles = Vec::new();
        let mut buf = Vec::new();

        loop {
            match xml_reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    let local_name = get_local_name(e);
                    if local_name == "palette" {
                        // Get palette name (Day, Dusk, Night)
                        let palette_name = get_attribute(e, "name").unwrap_or_default();
                        if !palette_name.is_empty() {
                            let mut profile = ColorProfile::new(palette_name.clone(), palette_name);
                            self.parse_palette(&mut xml_reader, &mut profile)?;
                            profiles.push(profile);
                        }
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        Ok(profiles)
    }

    /// Parse palette section with sRGB values
    fn parse_palette<R: std::io::BufRead>(
        &self,
        reader: &mut Reader<R>,
        profile: &mut ColorProfile,
    ) -> Result<()> {
        let mut buf = Vec::new();
        let mut depth = 1;

        loop {
            match reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    depth += 1;
                    let local_name = get_local_name(e);
                    if local_name == "item" {
                        let token = get_attribute(e, "token").unwrap_or_default();
                        if !token.is_empty() {
                            if let Ok(color) = self.parse_palette_item(reader, &token) {
                                profile.colors.insert(token, color);
                            }
                            depth -= 1; // parse_palette_item consumes the </item>
                        }
                    }
                }
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        Ok(())
    }

    /// Parse palette item with sRGB values
    fn parse_palette_item<R: std::io::BufRead>(
        &self,
        reader: &mut Reader<R>,
        token: &str,
    ) -> Result<ColorDefinition> {
        let mut r: Option<u8> = None;
        let mut g: Option<u8> = None;
        let mut b: Option<u8> = None;

        let mut buf = Vec::new();
        let mut depth = 1;

        loop {
            match reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    let local_name = get_local_name(e);
                    match local_name.as_str() {
                        // read_text_content consumes both text and end event,
                        // so don't increment depth for these
                        "red" => {
                            let val = read_text_content(reader)?;
                            r = val.parse().ok();
                        }
                        "green" => {
                            let val = read_text_content(reader)?;
                            g = val.parse().ok();
                        }
                        "blue" => {
                            let val = read_text_content(reader)?;
                            b = val.parse().ok();
                        }
                        // Other elements need depth tracking
                        _ => {
                            depth += 1;
                        }
                    }
                }
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        let srgb = match (r, g, b) {
            (Some(r), Some(g), Some(b)) => Some(SrgbColor::new(r, g, b)),
            _ => None,
        };

        Ok(ColorDefinition {
            token: token.to_string(),
            srgb,
            cie: None,
        })
    }

    /// Load symbols from directory.
    /// Security: rejects symlinks to prevent loading files from outside the catalogue tree.
    fn load_symbols(&mut self, dir: &Path, sources: &CatalogueSources) -> Result<()> {
        for path in sources.children(dir)? {
            if path.extension().is_some_and(|e| e == "svg") {
                let id = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let svg_ref = PathBuf::from(path.file_name().unwrap());
                self.symbols
                    .symbols
                    .insert(id.clone(), Symbol::new(id, svg_ref));
            }
        }
        Ok(())
    }

    /// Read every definition before resolving catalogue IDs. Forward references
    /// and ordered composites are materialized atomically (S-100 9-12.4.1.5/6).
    fn load_line_styles(&mut self, dir: &Path, sources: &CatalogueSources) -> Result<()> {
        let mut definitions = HashMap::new();
        let mut xml_bytes = 0u64;
        for path in sources.children(dir)? {
            if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("._"))
            {
                continue;
            }
            if path.extension().is_some_and(|e| e == "xml") {
                let id = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                xml_bytes = xml_bytes
                    .checked_add(sources.read_path(&path)?.len() as u64)
                    .ok_or_else(|| {
                        PCError::InvalidValue("LineStyle XML byte budget overflow".into())
                    })?;
                if xml_bytes > 32 * 1024 * 1024 || definitions.len() >= 4096 {
                    return Err(PCError::InvalidValue(
                        "LineStyle catalogue input budget exceeded".into(),
                    ));
                }
                let definition =
                    crate::line_style_xml::parse_bytes(&sources.read_path(&path)?, &id)?;
                if definitions.insert(id.clone(), definition).is_some() {
                    return Err(PCError::InvalidValue(format!(
                        "Duplicate LineStyle ID {id}"
                    )));
                }
            }
        }
        let resolved = crate::line_style_xml::resolve(&definitions)?;
        self.line_styles.extend(resolved);
        Ok(())
    }

    #[cfg(test)]
    fn parse_line_style_xml(&self, path: &Path, id: &str) -> Result<LineStyle> {
        let mut definitions = HashMap::new();
        definitions.insert(id.into(), crate::line_style_xml::parse(path, id)?);
        crate::line_style_xml::resolve(&definitions)?
            .remove(id)
            .ok_or_else(|| PCError::ResourceNotFound(id.into()))
    }

    /// Load area fills from directory (dynamic XML parsing)
    fn load_area_fills(&mut self, dir: &Path, sources: &CatalogueSources) -> Result<()> {
        for path in sources.children(dir)? {
            if path.extension().is_some_and(|e| e == "xml") {
                let id = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if let Ok(fill) = self.parse_area_fill_xml(&path, &id, sources) {
                    self.area_fills.insert(id, fill);
                }
            }
        }
        Ok(())
    }

    /// Parse area fill XML file
    /// XML structure:
    /// ```xml
    /// <af:symbolFill>
    ///   <areaCRS>GlobalGeometry</areaCRS>
    ///   <symbol reference="DIAMOND1P"/>
    ///   <v1><x>22.5</x><y>0.0</y></v1>
    ///   <v2><x>0</x><y>43.13</y></v2>
    /// </af:symbolFill>
    /// ```
    fn parse_area_fill_xml(
        &self,
        path: &Path,
        id: &str,
        sources: &CatalogueSources,
    ) -> Result<AreaFill> {
        let bytes = sources.read_path(path)?;
        let reader = BufReader::new(bytes.as_ref());
        let mut xml_reader = Reader::from_reader(reader);
        xml_reader.config_mut().trim_text(true);

        let mut area_crs = String::new();
        let mut symbol_ref = String::new();
        let mut v1 = VectorPoint::default();
        let mut v2 = VectorPoint::default();

        let mut buf = Vec::new();
        let mut in_v1 = false;
        let mut in_v2 = false;

        loop {
            match xml_reader.read_event_into(&mut buf)? {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let local_name = get_local_name(e);
                    match local_name.as_str() {
                        "areaCRS" => {
                            area_crs = read_text_content(&mut xml_reader)?;
                        }
                        "symbol" => {
                            if let Some(reference) = get_attribute(e, "reference") {
                                symbol_ref = reference;
                            }
                        }
                        "v1" => in_v1 = true,
                        "v2" => in_v2 = true,
                        "x" if in_v1 => {
                            let val = read_text_content(&mut xml_reader)?;
                            v1.x = val.parse().unwrap_or(0.0);
                        }
                        "y" if in_v1 => {
                            let val = read_text_content(&mut xml_reader)?;
                            v1.y = val.parse().unwrap_or(0.0);
                        }
                        "x" if in_v2 => {
                            let val = read_text_content(&mut xml_reader)?;
                            v2.x = val.parse().unwrap_or(0.0);
                        }
                        "y" if in_v2 => {
                            let val = read_text_content(&mut xml_reader)?;
                            v2.y = val.parse().unwrap_or(0.0);
                        }
                        _ => {}
                    }
                }
                Event::End(ref e) => {
                    let local_name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                    match local_name.as_str() {
                        "v1" => in_v1 = false,
                        "v2" => in_v2 = false,
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        if !symbol_ref.is_empty() {
            Ok(AreaFill::symbol(
                id.to_string(),
                symbol_ref,
                area_crs,
                v1,
                v2,
            ))
        } else {
            // Fallback to default if no symbol found (shouldn't happen with valid XML)
            Ok(AreaFill::solid(id.to_string(), "NODTA".to_string()))
        }
    }

    /// Load rules from directory
    fn load_rules(&mut self, dir: &Path, sources: &CatalogueSources) -> Result<()> {
        for path in sources.children(dir)? {
            if path.extension().is_some_and(|e| e == "lua") {
                let id = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let file_name = PathBuf::from(path.file_name().unwrap());

                // Check if this is the main file
                if id == "main" || id == "PortrayalMain" {
                    self.rules.main_file = Some(file_name.clone());
                }

                let rule = RuleFile {
                    id: id.clone(),
                    file_path: file_name,
                    rule_type: RuleType::Lua,
                    description: None,
                };
                self.rules.rule_files.insert(id, rule);
            }
        }
        Ok(())
    }

    /// Get color by token from any profile
    pub fn get_color(&self, profile: &str, token: &str) -> Option<SrgbColor> {
        self.color_profiles
            .get_profile(profile)
            .and_then(|p| p.get_srgb(token))
    }

    /// Get symbol by ID
    pub fn get_symbol(&self, id: &str) -> Option<&Symbol> {
        self.symbols.get(id)
    }

    /// Get line style by ID
    pub fn get_line_style(&self, id: &str) -> Option<&LineStyle> {
        self.line_styles.get(id)
    }

    /// Get area fill by ID
    pub fn get_area_fill(&self, id: &str) -> Option<&AreaFill> {
        self.area_fills.get(id)
    }

    /// Get all context parameters (from PC XML)
    pub fn get_context_parameters(&self) -> &HashMap<String, ContextParameter> {
        &self.rules.context_parameters
    }

    /// Get context parameter by ID
    pub fn get_context_parameter(&self, id: &str) -> Option<&ContextParameter> {
        self.rules.context_parameters.get(id)
    }

    /// Check if a viewing group is visible for a given display mode
    ///
    /// # Arguments
    /// * `viewing_group_id` - The viewing group ID (e.g., 21010 for DISPLBASE)
    /// * `display_mode_id` - The display mode ID ("DisplayBase", "StandardDisplay", "OtherInformation")
    ///
    /// # Returns
    /// `true` if the viewing group should be displayed in the given mode
    pub fn is_viewing_group_visible(&self, viewing_group_id: u32, display_mode_id: &str) -> bool {
        if self.foundation_mode.contains(&viewing_group_id) {
            return true;
        }
        crate::viewing::is_viewing_group_visible(
            viewing_group_id,
            display_mode_id,
            &self.viewing_group_layers,
            &self.display_modes,
        )
    }

    /// Get the display mode ID for a given mode type
    ///
    /// # Arguments
    /// * `mode` - 0 = Base, 1 = Standard, 2 = All
    ///
    /// # Returns
    /// The display mode ID string
    pub fn get_display_mode_id(mode: u8) -> &'static str {
        match mode {
            0 => "DisplayBase",
            1 => "StandardDisplay",
            _ => "OtherInformation",
        }
    }
}

/// Get local name without namespace prefix
fn get_local_name(e: &BytesStart) -> String {
    let name = e.local_name();
    String::from_utf8_lossy(name.as_ref()).to_string()
}

/// Get attribute value by name
fn get_attribute(e: &BytesStart, name: &str) -> Option<String> {
    for attr in e.attributes().flatten() {
        let key = String::from_utf8_lossy(attr.key.as_ref());
        if key == name {
            return Some(String::from_utf8_lossy(&attr.value).to_string());
        }
    }
    None
}

/// Read text content of current element
fn read_text_content<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<String> {
    let mut buf = Vec::new();
    let mut text = String::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Text(e) => {
                text = e.unescape().map_err(PCError::Xml)?.to_string();
            }
            Event::End(_) => break,
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(text)
}

#[cfg(test)]
mod line_metadata_xml_tests {
    use super::*;
    fn parse(xml: &str) -> Result<LineStyle> {
        let pc = PortrayalCatalogue {
            root_path: Default::default(),
            product_id: String::new(),
            version: String::new(),
            color_profiles: ColorProfiles::new(),
            symbols: Symbols::new(Default::default()),
            line_styles: Default::default(),
            area_fills: Default::default(),
            viewing_groups: ViewingGroups::new(),
            viewing_group_layers: ViewingGroupLayers::new(),
            display_modes: DisplayModes::new(),
            display_planes: crate::DisplayPlanes::default(),
            foundation_mode: vec![],
            rules: PortrayalRules::new(Default::default()),
        };
        static NEXT_TEST_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferrite-line-{}-{}-{}.xml",
            std::process::id(),
            NEXT_TEST_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, xml).unwrap();
        let result = pc.parse_line_style_xml(&path, "L");
        std::fs::remove_file(path).unwrap();
        result
    }
    #[test]
    fn xml_attributes_and_signed_symbol_transform_are_preserved() {
        let style=parse(r#"<lineStyle capStyle="Square" joinStyle="Bevel" offset="-0.4"><intervalLength>5</intervalLength><pen width="0.32"><color>CHBLK</color></pen><symbol reference="R" rotation="30" crsType="LineCRS" scaleFactor="-1.5"><position>-2</position></symbol></lineStyle>"#).unwrap();
        let LineStyle::Simple(style) = style else {
            panic!()
        };
        assert_eq!(style.pen.cap_style, crate::CapStyle::Square);
        assert_eq!(style.pen.join_style, crate::JoinStyle::Bevel);
        assert_eq!(style.offset_mm, -0.4);
        assert_eq!(style.interval_length, 5.);
        let symbol = &style.symbols[0];
        assert_eq!(symbol.rotation, 30.);
        assert_eq!(symbol.position, -2.);
        assert_eq!(symbol.scale_factor, -1.5);
        assert_eq!(symbol.crs_type, ferrite_kernel::LineSymbolCrs::LineCRS);
        let LineStyle::Simple(style)=parse(r#"<lineStyle><pen width="0.32"><color>CHBLK</color></pen><symbol reference="R" scaleFactor="0"><position>0</position></symbol></lineStyle>"#).unwrap() else {panic!()};
        assert_eq!(style.symbols[0].scale_factor, 0.);
        assert_eq!(style.symbols[0].rotation, 0.);
        assert_eq!(
            style.symbols[0].crs_type,
            ferrite_kernel::LineSymbolCrs::LocalCRS
        );
    }
    #[test]
    fn catalogue_retains_signed_and_zero_dash_values_without_rewriting_them() {
        let LineStyle::Simple(style) = parse(r#"<lineStyle><intervalLength>22</intervalLength><pen width="0.32"><color>CHMGD</color></pen><dash><start>5.1</start><length>-3.1</length></dash><dash><start>1</start><length>0</length></dash></lineStyle>"#).unwrap() else { panic!() };
        assert_eq!(style.dashes[0].start, 5.1);
        assert_eq!(style.dashes[0].length, -3.1);
        assert_eq!(style.dashes[1].length, 0.);
        assert!(
            style.dash_cycle().is_err(),
            "Unsupported interval renderer must not silently rewrite authored values"
        );
    }
    #[test]
    fn malformed_xml_style_metadata_is_rejected() {
        for attribute in [
            "capStyle=\"invalid\"",
            "joinStyle=\"invalid\"",
            "offset=\"NaN\"",
        ] {
            assert!(parse(&format!(
                "<lineStyle {attribute}><pen width=\"1\"><color>C</color></pen></lineStyle>"
            ))
            .is_err());
        }
        for symbol in [
            r#"<symbol reference="R"><position>bad</position></symbol>"#,
            r#"<symbol reference="R" rotation="inf"><position>1</position></symbol>"#,
            r#"<symbol reference="R" crsType="GeographicCRS"><position>1</position></symbol>"#,
            r#"<symbol reference="R" scaleFactor="NaN"><position>1</position></symbol>"#,
            r#"<symbol reference="R"></symbol>"#,
            r#"<symbol><position>1</position></symbol>"#,
        ] {
            assert!(parse(&format!(
                "<lineStyle><pen width=\"1\"><color>C</color></pen>{symbol}</lineStyle>"
            ))
            .is_err());
        }
    }
}
