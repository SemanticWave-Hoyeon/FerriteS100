//! Portrayal Catalogue container and loading

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::{
    AreaFill, ColorDefinition, ColorProfile, ColorProfiles, ContextParamType, ContextParameter,
    Dash, DisplayMode, DisplayModes, LineStyle, LineSymbol, PCError, PortrayalRules, Result,
    RuleFile, RuleType, SimpleLineStyle, SrgbColor, Symbol, Symbols, VectorPoint, ViewingGroup,
    ViewingGroupLayer, ViewingGroupLayers, ViewingGroups,
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
    pub rules: PortrayalRules,
}

impl PortrayalCatalogue {
    /// Maximum allowed XML file size (50 MB)
    /// Security: Prevents resource exhaustion from oversized files
    const MAX_XML_SIZE: u64 = 50 * 1024 * 1024;

    /// Load Portrayal Catalogue from directory
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
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
            rules: PortrayalRules::new(path.join("Rules")),
        };

        // Find and parse portrayal_catalogue.xml
        let catalogue_file = path.join("portrayal_catalogue.xml");
        if catalogue_file.exists() {
            catalogue.parse_catalogue_xml(&catalogue_file)?;
        }

        // Load color profiles
        let colors_dir = path.join("ColorProfiles");
        if colors_dir.exists() {
            catalogue.load_color_profiles(&colors_dir)?;
        }

        // Load symbols
        let symbols_dir = path.join("Symbols");
        if symbols_dir.exists() {
            catalogue.load_symbols(&symbols_dir)?;
        }

        // Load line styles
        let lines_dir = path.join("LineStyles");
        if lines_dir.exists() {
            catalogue.load_line_styles(&lines_dir)?;
        }

        // Load area fills
        let fills_dir = path.join("AreaFills");
        if fills_dir.exists() {
            catalogue.load_area_fills(&fills_dir)?;
        }

        // Load rules
        let rules_dir = path.join("Rules");
        if rules_dir.exists() {
            catalogue.load_rules(&rules_dir)?;
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
    fn parse_catalogue_xml(&mut self, path: &Path) -> Result<()> {
        // Security: Check file size before loading
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > Self::MAX_XML_SIZE {
            return Err(PCError::InvalidValue(format!(
                "File too large: {} bytes (max {} bytes)",
                metadata.len(),
                Self::MAX_XML_SIZE
            )));
        }

        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut xml_reader = Reader::from_reader(reader);
        xml_reader.config_mut().trim_text(true);

        let mut buf = Vec::new();

        loop {
            match xml_reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    let local_name = get_local_name(e);
                    match local_name.as_str() {
                        "productId" => {
                            self.product_id = read_text_content(&mut xml_reader)?;
                        }
                        "version" | "versionNumber" => {
                            self.version = read_text_content(&mut xml_reader)?;
                        }
                        "viewingGroup" => {
                            let vg = self.parse_viewing_group(&mut xml_reader)?;
                            self.viewing_groups.groups.insert(vg.id, vg);
                        }
                        "viewingGroupLayer" => {
                            let vgl = self.parse_viewing_group_layer(&mut xml_reader)?;
                            self.viewing_group_layers.layers.insert(vgl.id.clone(), vgl);
                        }
                        "displayMode" => {
                            let dm = self.parse_display_mode(&mut xml_reader)?;
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
    fn load_color_profiles(&mut self, dir: &Path) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "xml") {
                if let Ok(profile) = self.parse_color_profile(&path) {
                    self.color_profiles
                        .profiles
                        .insert(profile.id.clone(), profile);
                }
            }
        }
        Ok(())
    }

    /// Parse color profile XML
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
    /// </colorProfile>
    /// ```
    fn parse_color_profile(&self, path: &Path) -> Result<ColorProfile> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut xml_reader = Reader::from_reader(reader);
        xml_reader.config_mut().trim_text(true);

        let mut profile = ColorProfile::new(
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            String::new(),
        );

        let mut buf = Vec::new();

        loop {
            match xml_reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    let local_name = get_local_name(e);
                    if local_name == "palette" {
                        // Get palette name (Day, Dusk, Night)
                        let palette_name = get_attribute(e, "name").unwrap_or_default();
                        // For now, use "Day" palette as default
                        if palette_name == "Day" || profile.colors.is_empty() {
                            profile.name = palette_name;
                            self.parse_palette(&mut xml_reader, &mut profile)?;
                        }
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        tracing::debug!(
            "Loaded color profile '{}' with {} colors",
            profile.name,
            profile.colors.len()
        );
        Ok(profile)
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

    /// Load symbols from directory
    fn load_symbols(&mut self, dir: &Path) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
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

    /// Load line styles from directory (dynamic XML parsing)
    fn load_line_styles(&mut self, dir: &Path) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "xml") {
                let id = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if let Ok(style) = self.parse_line_style_xml(&path, &id) {
                    self.line_styles.insert(id, style);
                }
            }
        }
        Ok(())
    }

    /// Parse line style XML file
    /// XML structure:
    /// ```xml
    /// <ls:lineStyle>
    ///    <intervalLength>32.3</intervalLength>
    ///    <pen width="0.32"><color>CHMGD</color></pen>
    ///    <dash><start>2</start><length>6</length></dash>
    ///    <symbol reference="EMAREMG1"><position>5</position></symbol>
    /// </ls:lineStyle>
    /// ```
    fn parse_line_style_xml(&self, path: &Path, id: &str) -> Result<LineStyle> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut xml_reader = Reader::from_reader(reader);
        xml_reader.config_mut().trim_text(true);

        let mut style = SimpleLineStyle {
            id: id.to_string(),
            ..Default::default()
        };

        let mut buf = Vec::new();
        let mut in_pen = false;
        let mut in_dash = false;
        let mut in_symbol = false;
        let mut current_dash = Dash::default();
        let mut current_symbol = LineSymbol::default();

        loop {
            match xml_reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    let local_name = get_local_name(e);
                    match local_name.as_str() {
                        "pen" => {
                            in_pen = true;
                            // Get width attribute
                            if let Some(width) = get_attribute(e, "width") {
                                style.pen.width = width.parse().unwrap_or(0.32);
                            }
                        }
                        "dash" => {
                            in_dash = true;
                            current_dash = Dash::default();
                        }
                        "symbol" => {
                            in_symbol = true;
                            current_symbol = LineSymbol::default();
                            if let Some(reference) = get_attribute(e, "reference") {
                                current_symbol.reference = reference;
                            }
                        }
                        "intervalLength" => {
                            let val = read_text_content(&mut xml_reader)?;
                            style.interval_length = val.parse().unwrap_or(0.0);
                        }
                        "color" if in_pen => {
                            style.pen.color_token = read_text_content(&mut xml_reader)?;
                        }
                        "start" if in_dash => {
                            let val = read_text_content(&mut xml_reader)?;
                            current_dash.start = val.parse().unwrap_or(0.0);
                        }
                        "length" if in_dash => {
                            let val = read_text_content(&mut xml_reader)?;
                            current_dash.length = val.parse().unwrap_or(0.0);
                        }
                        "position" if in_symbol => {
                            let val = read_text_content(&mut xml_reader)?;
                            current_symbol.position = val.parse().unwrap_or(0.0);
                        }
                        _ => {}
                    }
                }
                Event::End(ref e) => {
                    let local_name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                    match local_name.as_str() {
                        "pen" => in_pen = false,
                        "dash" => {
                            in_dash = false;
                            style.dashes.push(current_dash.clone());
                        }
                        "symbol" => {
                            in_symbol = false;
                            style.symbols.push(current_symbol.clone());
                        }
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        Ok(LineStyle::Simple(style))
    }

    /// Load area fills from directory (dynamic XML parsing)
    fn load_area_fills(&mut self, dir: &Path) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "xml") {
                let id = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if let Ok(fill) = self.parse_area_fill_xml(&path, &id) {
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
    fn parse_area_fill_xml(&self, path: &Path, id: &str) -> Result<AreaFill> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
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
    fn load_rules(&mut self, dir: &Path) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
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
