//! Drawing Instruction Parser (S-100 Part 9a, clause 9a-11)
//!
//! Parses DEF-encoded drawing instruction strings returned from Lua portrayal rules
//! via HostPortrayalEmit. The DEF format is defined in Part 13, clause 13-6.1:
//!   - Elements separated by semicolons (;)
//!   - Each element: Item[:ParameterList]
//!   - Parameters separated by commas (,)
//!   - Special characters escaped: &s → ;  &c → :  &m → ,  &a → &
//!
//! The portrayal engine is a command-driven state machine (9a-11.1.1).
//! State commands modify variables; drawing commands consume them.
//! State is reset per feature instance.

use crate::Result;
use ferrite_kernel::{IntervalClosure, TemporalBounds, TemporalInterval};

// ──────────────────────────────────────────────────────
// Public types
// ──────────────────────────────────────────────────────

/// Parsed drawing instruction from Lua output (one per HostPortrayalEmit call)
#[derive(Debug, Clone)]
pub struct ParsedInstruction {
    pub feature_id: String,
    pub commands: Vec<DrawingCommand>,
}

/// Display plane for radar overlay (9a-11.2.2.1)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DisplayPlane {
    #[default]
    Unspecified,
    UnderRadar,
    OverRadar,
    Named(std::sync::Arc<str>),
}
impl DisplayPlane {
    /// Catalogue identifiers are case sensitive and resolved by the product adapter.
    pub fn reference(&self) -> Option<&str> {
        match self {
            Self::Unspecified => None,
            Self::UnderRadar => Some("UnderRadar"),
            Self::OverRadar => Some("OverRadar"),
            Self::Named(name) => Some(name.as_ref()),
        }
    }
}

/// Per-drawing-command visibility and ordering state (9a-11.2.2.1)
/// Captured as a snapshot when each drawing command is created.
#[derive(Debug, Clone)]
pub struct VisibilityState {
    pub viewing_groups: Vec<u32>,
    pub named_viewing_groups: Box<[String]>,
    pub drawing_priority: i32,
    pub display_plane: DisplayPlane,
    pub scale_minimum: Option<u32>,
    pub scale_maximum: Option<u32>,
    pub id: Option<String>,
    pub parent: Option<String>,
    pub hover: bool,
    pub time_intervals: Vec<TemporalInterval>,
}

impl Default for VisibilityState {
    fn default() -> Self {
        VisibilityState {
            viewing_groups: Vec::new(),
            named_viewing_groups: Box::default(),
            drawing_priority: 0,
            display_plane: DisplayPlane::Unspecified,
            scale_minimum: None,
            scale_maximum: None,
            id: None,
            parent: None,
            hover: false,
            time_intervals: Vec::new(),
        }
    }
}

/// Colour override entry (9a-11.2.2.5)
#[derive(Debug, Clone)]
pub struct ColorOverrideEntry {
    pub color_token: String,
    pub color_transparency: f64,
    pub override_token: String,
    pub override_transparency: f64,
}

/// Augmented geometry segment for AugmentedPath (9a-11.2.2.6)
#[derive(Debug, Clone)]
pub enum PathSegment {
    Polyline(Vec<(f64, f64)>),
    Arc3Points {
        start: (f64, f64),
        median: (f64, f64),
        end: (f64, f64),
    },
    ArcByRadius {
        center: (f64, f64),
        radius: f64,
        start_angle: f64,
        angular_distance: f64,
    },
    Annulus {
        center: (f64, f64),
        outer_radius: f64,
        inner_radius: f64,
        start_angle: f64,
        angular_distance: f64,
    },
}

/// Line symbol definition for LineStyle (9a-11.2.2.3)
#[derive(Debug, Clone)]
pub struct LineSymbolDef {
    pub reference: String,
    pub position: f64,
    pub rotation: f64,
    pub crs_type: String,
    pub scale_factor: f64,
}

/// Dash pattern (9a-11.2.2.3)
#[derive(Debug, Clone)]
pub struct DashPattern {
    pub start: f64,
    pub length: f64,
}

/// Coverage lookup entry (9a-11.2.2.8)
#[derive(Debug, Clone)]
pub struct LookupEntry {
    pub label: String,
    pub closure: ferrite_kernel::IntervalClosure,
    pub end_color: Option<(String, f64)>,
    pub pen_width: f64,
    pub range_min: f64,
    pub range_max: f64,
    pub color_token: Option<String>,
    pub transparency: f64,
    pub symbol: Option<String>,
    pub text: Option<String>,
}

/// Drawing command – all S-100 Part 9a drawing commands (Table 9a-3)
/// and state-carrying commands that the consumer needs to see.
#[derive(Debug, Clone)]
pub enum DrawingCommand {
    // ── Drawing Commands (9a-11.2.1) ──
    /// PointInstruction:symbol (9a-11.2.1, Table 9a-4)
    PointInstruction {
        symbol_ref: String,
        rotation: f32,
        rotation_crs: String,
        scale: f32,
        /// Explicit position from AugmentedPoint (overrides feature spatial)
        position: Option<(f64, f64)>,
        position_crs: Option<String>,
        local_offset: (f64, f64),
        scale_factor: f64,
        /// Color overrides to apply to this symbol
        color_overrides: Vec<ColorOverrideEntry>,
        /// Override all non-transparent colours
        override_all: Option<(String, f64)>,
        /// LinePlacement for placing symbol on a curve (9a-11.2.2.2)
        /// (mode, offset) where mode is "Relative" or "Absolute"
        line_placement: Option<(String, f64)>,
        line_placement_visible_parts: bool,
        /// Spatial references to use for curve placement
        spatial_refs: Vec<(String, bool)>,
        visibility: VisibilityState,
    },

    /// LineInstruction:lineStyle[,lineStyle,...] (9a-11.2.1)
    /// Line segments with higher drawing priority suppress coincident lower ones.
    LineInstruction {
        style_refs: Vec<String>,
        /// Inline simple line style: (width, color_token)
        simple_style: Option<ferrite_kernel::StrokeDefinition>,
        /// Spatial references to use instead of feature geometry
        spatial_refs: Vec<(String, bool)>,
        /// Augmented geometry segments to use instead of feature geometry
        augmented_segments: Vec<PathSegment>,
        augmented_crs: Option<AugmentedPathCrs>,
        /// AugmentedRay: line from feature point in given direction/length
        augmented_ray: Option<AugmentedRayDef>,
        visibility: VisibilityState,
    },

    /// LineInstructionUnsuppressed:lineStyle[,lineStyle,...] (9a-11.2.1)
    /// Same as LineInstruction but without line suppression.
    LineInstructionUnsuppressed {
        style_refs: Vec<String>,
        simple_style: Option<ferrite_kernel::StrokeDefinition>,
        spatial_refs: Vec<(String, bool)>,
        augmented_segments: Vec<PathSegment>,
        augmented_crs: Option<AugmentedPathCrs>,
        /// AugmentedRay: line from feature point in given direction/length
        augmented_ray: Option<AugmentedRayDef>,
        visibility: VisibilityState,
    },

    /// ColorFill:token[,transparency] (9a-11.2.1)
    ColorFill {
        color_token: String,
        transparency: f64,
        area_crs: String,
        spatial_refs: Vec<(String, bool)>,
        visibility: VisibilityState,
    },

    /// AreaFillReference:reference (9a-11.2.1)
    AreaFillReference {
        reference: String,
        area_crs: String,
        color_overrides: Vec<ColorOverrideEntry>,
        override_all: Option<(String, f64)>,
        spatial_refs: Vec<(String, bool)>,
        visibility: VisibilityState,
    },

    /// PixmapFill:reference (9a-11.2.1)
    PixmapFill {
        reference: String,
        area_crs: String,
        color_overrides: Vec<ColorOverrideEntry>,
        override_all: Option<(String, f64)>,
        spatial_refs: Vec<(String, bool)>,
        visibility: VisibilityState,
    },

    /// SymbolFill:symbol,v1,v2[,clipSymbols] (9a-11.2.1)
    SymbolFill {
        symbol: String,
        v1: (f64, f64),
        v2: (f64, f64),
        clip_symbols: bool,
        area_crs: String,
        color_overrides: Vec<ColorOverrideEntry>,
        override_all: Option<(String, f64)>,
        spatial_refs: Vec<(String, bool)>,
        visibility: VisibilityState,
    },

    /// HatchFill:direction,distance,lineStyle[,lineStyle] (9a-11.2.1)
    HatchFill {
        direction: (f64, f64),
        distance: f64,
        line_styles: Vec<String>,
        inline_styles: Vec<Option<ferrite_kernel::StrokeDefinition>>,
        area_crs: String,
        spatial_refs: Vec<(String, bool)>,
        visibility: VisibilityState,
    },

    /// TextInstruction:text (9a-11.2.1, Table 9a-5)
    TextInstruction {
        text: String,
        font_size: f32,
        color_token: String,
        color_transparency: f64,
        bg_color_token: String,
        bg_transparency: f64,
        bold: bool,
        italic: bool,
        font_proportion: String,
        serifs: bool,
        underline: bool,
        strikethrough: bool,
        upperline: bool,
        font_reference: String,
        h_align: String,
        v_align: String,
        vertical_offset: f64,
        local_offset: (f64, f64),
        rotation: f32,
        rotation_crs: String,
        scale_factor: f64,
        position: Option<(f64, f64)>,
        position_crs: Option<String>,
        spatial_refs: Vec<(String, bool)>,
        visibility: VisibilityState,
    },

    /// CoverageFill:attributeCode[,uom[,placement]] (9a-11.2.1)
    CoverageFill {
        attribute_code: String,
        uom: Option<String>,
        placement: Option<String>,
        lookup_entries: Vec<LookupEntry>,
        spatial_refs: Vec<(String, bool)>,
        visibility: VisibilityState,
    },

    /// NullInstruction (9a-11.2.1) – feature purposefully not portrayed.
    NullInstruction { visibility: VisibilityState },

    /// AlertReference:reference (9a-11.2.2.9)
    AlertReference {
        reference: String,
        visibility: VisibilityState,
    },

    // ── State-carrying commands the consumer needs to see ──
    /// AugmentedPoint – kept for backward compat (consumer may inspect it)
    AugmentedPoint { crs: String, x: f64, y: f64 },

    /// Dash pattern for line styles
    Dash { start: f32, length: f32 },

    /// SpatialReference – kept for consumer
    SpatialReference { spatial_id: String, forward: bool },
}

impl ParsedInstruction {
    pub fn new(feature_id: String) -> Self {
        ParsedInstruction {
            feature_id,
            commands: Vec::new(),
        }
    }

    // ── Backward-compatible accessors ──
    // These scan the commands for the first drawing command's visibility state.

    pub fn viewing_groups(&self) -> &[u32] {
        for cmd in &self.commands {
            if let Some(vis) = cmd.visibility() {
                return &vis.viewing_groups;
            }
        }
        &[]
    }

    pub fn drawing_priority(&self) -> i32 {
        for cmd in &self.commands {
            if let Some(vis) = cmd.visibility() {
                return vis.drawing_priority;
            }
        }
        0
    }

    pub fn display_plane(&self) -> DisplayPlane {
        for cmd in &self.commands {
            if let Some(vis) = cmd.visibility() {
                return vis.display_plane.clone();
            }
        }
        DisplayPlane::Unspecified
    }

    pub fn scale_minimum(&self) -> Option<u32> {
        for cmd in &self.commands {
            if let Some(vis) = cmd.visibility() {
                return vis.scale_minimum;
            }
        }
        None
    }
}

impl DrawingCommand {
    /// Get visibility state if this is a drawing command (not a state-only command).
    pub fn visibility(&self) -> Option<&VisibilityState> {
        match self {
            Self::PointInstruction { visibility, .. }
            | Self::LineInstruction { visibility, .. }
            | Self::LineInstructionUnsuppressed { visibility, .. }
            | Self::ColorFill { visibility, .. }
            | Self::AreaFillReference { visibility, .. }
            | Self::PixmapFill { visibility, .. }
            | Self::SymbolFill { visibility, .. }
            | Self::HatchFill { visibility, .. }
            | Self::TextInstruction { visibility, .. }
            | Self::CoverageFill { visibility, .. }
            | Self::NullInstruction { visibility, .. }
            | Self::AlertReference { visibility, .. } => Some(visibility),
            Self::AugmentedPoint { .. } | Self::Dash { .. } | Self::SpatialReference { .. } => None,
        }
    }
}

// ──────────────────────────────────────────────────────
// DEF string decoding (Part 13, clause 13-6.1.2)
// ──────────────────────────────────────────────────────

/// Decode DEF-encoded special characters: &s → ;  &c → :  &m → ,  &a → &
fn def_decode(s: &str) -> String {
    s.replace("&s", ";")
        .replace("&c", ":")
        .replace("&m", ",")
        .replace("&a", "&")
}

// ──────────────────────────────────────────────────────
// State machine (9a-11.1.1)
// ──────────────────────────────────────────────────────

/// Complete drawing state machine (9a-11.2.2)
/// Accumulated from state commands, consumed by drawing commands.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct DrawingState {
    // ── Visibility (9a-11.2.2.1) ──
    viewing_groups: Vec<u32>,
    named_viewing_groups: Vec<String>,
    display_plane: DisplayPlane,
    drawing_priority: i32,
    scale_minimum: Option<u32>,
    scale_maximum: Option<u32>,
    id: Option<String>,
    parent: Option<String>,
    hover: bool,

    // ── Transform (9a-11.2.2.2) ──
    local_offset: (f64, f64),    // mm (x, y)
    line_placement_mode: String, // Relative | Absolute
    line_placement_offset: f64,  // homogenous or mm
    line_placement_end: Option<f64>,
    line_placement_visible_parts: bool,
    area_placement: String, // VisibleParts | Geographic
    area_crs: String,       // GlobalGeometry | LocalGeometry | Global
    rotation_crs: String,   // PortrayalCRS | GeographicCRS | LocalCRS | LineCRS
    rotation: f64,          // degrees
    scale_factor: f64,

    // ── Line Style (9a-11.2.2.3) ──
    pending_dashes: Vec<DashPattern>,
    defined_line_styles: std::collections::HashMap<String, ferrite_kernel::StrokeDefinition>,
    pending_line_symbols: Vec<LineSymbolDef>,

    // ── Text Style (9a-11.2.2.4) ──
    font_color: String,
    font_color_transparency: f64,
    font_bg_color: String,
    font_bg_transparency: f64,
    font_size: f32,
    font_proportion: String,
    font_weight: String,
    font_slant: String,
    font_serifs: bool,
    font_underline: bool,
    font_strikethrough: bool,
    font_upperline: bool,
    font_reference: String,
    text_align_h: String,
    text_align_v: String,
    text_vertical_offset: f64,

    // ── Colour Override (9a-11.2.2.5) ──
    color_overrides: Vec<ColorOverrideEntry>,
    override_all: Option<(String, f64)>,

    // ── Geometry (9a-11.2.2.6) ──
    spatial_references: Vec<(String, bool)>,
    augmented_point: Option<(f64, f64)>,
    augmented_point_crs: Option<String>,
    augmented_ray: Option<AugmentedRayDef>,
    augmented_path: Option<AugmentedPathDef>,
    segment_list: Vec<PathSegment>,

    // ── Coverage (9a-11.2.2.8) ──
    lookup_entries: Vec<LookupEntry>,
    coverage_color: Option<(String, f64, Option<(String, f64)>, f64)>,
    coverage_symbol: Option<String>,
    coverage_text: Option<String>,

    // ── Time (9a-11.2.2.7) ──
    date: Option<TemporalBounds>,
    time: Option<TemporalBounds>,
    date_time: Option<TemporalBounds>,
    time_intervals: Vec<TemporalInterval>,

    // ── Alert (9a-11.2.2.9) ──
    alert_reference: Option<String>,
}

/// AugmentedRay geometry definition (S-100 Part 9a, clause 9a-11.2.15)
///
/// Defines a line from the position of a point feature to another position.
/// The endpoint is determined by the direction and length attributes.
/// Used for light sector lines, bearing lines, etc.
#[derive(Debug, Clone)]
pub struct AugmentedRayDef {
    pub direction_crs: String,
    pub direction: f64,
    pub length_crs: String,
    pub length: f64,
}

#[derive(Debug, Clone)]
pub struct AugmentedPathCrs {
    pub crs_position: String,
    pub crs_angle: String,
    pub crs_distance: String,
}
#[derive(Debug, Clone)]
struct AugmentedPathDef {
    crs: AugmentedPathCrs,
    segments: Vec<PathSegment>,
}

impl Default for DrawingState {
    fn default() -> Self {
        DrawingState {
            // Visibility
            viewing_groups: Vec::new(),
            named_viewing_groups: Vec::new(),
            display_plane: DisplayPlane::Unspecified,
            drawing_priority: 0,
            scale_minimum: None,
            scale_maximum: None,
            id: None,
            parent: None,
            hover: false,
            // Transform
            local_offset: (0.0, 0.0),
            line_placement_mode: "Relative".into(),
            line_placement_offset: 0.5,
            line_placement_end: None,
            line_placement_visible_parts: false,
            area_placement: "VisibleParts".into(),
            area_crs: "GlobalGeometry".into(),
            rotation_crs: "PortrayalCRS".into(),
            rotation: 0.0,
            scale_factor: 1.0,
            // Line Style
            pending_dashes: Vec::new(),
            defined_line_styles: Default::default(),
            pending_line_symbols: Vec::new(),
            // Text Style (S-100 9a-11.2.2.4 initial state)
            font_color: String::new(),
            font_color_transparency: 0.0,
            font_bg_color: String::new(),
            font_bg_transparency: 1.0,
            font_size: 10.0,
            font_proportion: "Proportional".into(),
            font_weight: "Medium".into(),
            font_slant: "Upright".into(),
            font_serifs: false,
            font_underline: false,
            font_strikethrough: false,
            font_upperline: false,
            font_reference: String::new(),
            text_align_h: "Start".into(),
            text_align_v: "Bottom".into(),
            text_vertical_offset: 0.0,
            // Colour Override
            color_overrides: Vec::new(),
            override_all: None,
            // Geometry
            spatial_references: Vec::new(),
            augmented_point: None,
            augmented_point_crs: None,
            augmented_ray: None,
            augmented_path: None,
            segment_list: Vec::new(),
            // Coverage
            lookup_entries: Vec::new(),
            coverage_color: None,
            coverage_symbol: None,
            coverage_text: None,
            // Time
            date: None,
            time: None,
            date_time: None,
            time_intervals: Vec::new(),
            // Alert
            alert_reference: None,
        }
    }
}

impl DrawingState {
    fn visibility_snapshot(&self) -> VisibilityState {
        VisibilityState {
            viewing_groups: self.viewing_groups.clone(),
            named_viewing_groups: self.named_viewing_groups.clone().into_boxed_slice(),
            drawing_priority: self.drawing_priority,
            display_plane: self.display_plane.clone(),
            scale_minimum: self.scale_minimum,
            scale_maximum: self.scale_maximum,
            id: self.id.clone(),
            parent: self.parent.clone(),
            hover: self.hover,
            time_intervals: self.time_intervals.clone(),
        }
    }

    /// SpatialReference state persists until ClearGeometry (Part 9a-11.2.2.6).
    fn take_spatial_refs(&mut self) -> Vec<(String, bool)> {
        self.spatial_references.clone()
    }

    /// Snapshot the active path. Pending segments become active only at AugmentedPath.
    fn take_augmented_segments(&mut self) -> Vec<PathSegment> {
        self.augmented_path
            .as_ref()
            .map(|path| path.segments.clone())
            .unwrap_or_default()
    }

    fn take_color_overrides(&self) -> Vec<ColorOverrideEntry> {
        self.color_overrides.clone()
    }

    fn take_override_all(&self) -> Option<(String, f64)> {
        self.override_all.clone()
    }

    fn take_lookup_entries(&mut self) -> Vec<LookupEntry> {
        std::mem::take(&mut self.lookup_entries)
    }
}

// ──────────────────────────────────────────────────────
// Parser entry point
// ──────────────────────────────────────────────────────

fn transparency_parameter(params: &[&str], index: usize, default: f64) -> Result<f64> {
    let value = params
        .get(index)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<f64>())
        .transpose()
        .map_err(|_| crate::LuaError::InvalidInstruction("Invalid colour transparency".into()))?
        .unwrap_or(default);
    if !value.is_finite() || !(0. ..=1.).contains(&value) {
        return Err(crate::LuaError::InvalidInstruction(
            "Colour transparency must be finite and in [0,1]".into(),
        ));
    }
    Ok(value)
}

fn geometry_number(params: &[&str], index: usize, default: Option<f64>) -> Result<f64> {
    let raw = params.get(index).filter(|p| !p.is_empty());
    let value = match raw {
        Some(raw) => raw.parse::<f64>().map_err(|_| crate::LuaError::InvalidInstruction(
            "Invalid geometry number".into()))?,
        None => default.ok_or_else(|| crate::LuaError::InvalidInstruction(
            "Missing geometry number".into()))?,
    };
    if !value.is_finite() {
        return Err(crate::LuaError::InvalidInstruction("Non-finite geometry number".into()));
    }
    Ok(value)
}
fn geometry_crs(value: &str) -> Result<String> {
    if matches!(value, "GeographicCRS" | "LocalCRS" | "PortrayalCRS") {
        Ok(value.to_owned())
    } else {
        Err(crate::LuaError::InvalidInstruction("Unsupported geometry CRS".into()))
    }
}
fn geometry_arity(params: &[&str], minimum: usize, maximum: usize) -> Result<()> {
    if (minimum..=maximum).contains(&params.len()) { Ok(()) } else {
        Err(crate::LuaError::InvalidInstruction("Wrong geometry argument count".into()))
    }
}

/// Parse drawing instruction string from Lua (DEF format, Part 13 clause 13-6.1)
///
/// Format: "ViewingGroup:21010;DrawingPriority:15;PointInstruction:LIGHTS01"
pub fn parse_instruction_string(
    feature_id: &str,
    instruction_str: &str,
) -> Result<ParsedInstruction> {
    let mut result = ParsedInstruction::new(feature_id.to_string());
    let mut state = DrawingState::default();

    // Step 1 (Part 13): split on semicolons
    for element in instruction_str.split(';') {
        let element = element.trim();
        if element.is_empty() {
            continue;
        }

        // Step 2: split into item and parameter list on first colon
        // Commands with no parameters (NullInstruction, ClearGeometry, ClearOverride, ClearTime)
        if let Some((item, params)) = element.split_once(':') {
            parse_command(&mut result, &mut state, item.trim(), params.trim())?;
        } else {
            // No-parameter commands
            parse_command(&mut result, &mut state, element.trim(), "")?;
        }
    }

    Ok(result)
}

// ──────────────────────────────────────────────────────
// Command parser (all 62 commands from S-100 9a-11)
// ──────────────────────────────────────────────────────

fn parse_command(
    result: &mut ParsedInstruction,
    state: &mut DrawingState,
    cmd: &str,
    value: &str,
) -> Result<()> {
    // Helper: split parameter list by comma (Step 3 of Part 13 parsing)
    let params: Vec<&str> = if value.is_empty() {
        Vec::new()
    } else {
        value.split(',').collect()
    };

    match cmd {
        // ════════════════════════════════════════════
        // Visibility State Commands (9a-11.2.2.1)
        // ════════════════════════════════════════════
        "ViewingGroup" => {
            state.viewing_groups.clear();
            state.named_viewing_groups.clear();
            // Empty state is the Part 9a initial state: no group restriction.
            // Empty entries in a non-empty list remain invalid.
            for v in &params {
                let id = v.trim();
                if id.is_empty() {
                    return Err(crate::LuaError::InvalidInstruction(
                        "Empty viewing-group identifier".into(),
                    ));
                }
                match id.parse::<u32>() {
                    Ok(group) if group.to_string() == id => state.viewing_groups.push(group),
                    _ => state.named_viewing_groups.push(id.to_owned()),
                }
            }
        }
        "DisplayPlane" => {
            if params.len() != 1 || value.is_empty() {
                return Err(crate::LuaError::InvalidInstruction("DisplayPlane requires one catalogue identifier".into()));
            }
            state.display_plane = match def_decode(value).as_str() {
                "UnderRadar" => DisplayPlane::UnderRadar,
                "OverRadar" => DisplayPlane::OverRadar,
                name => DisplayPlane::Named(std::sync::Arc::from(name)),
            };
        }
        "DrawingPriority" => {
            state.drawing_priority = value.parse::<i32>().map_err(|_| {
                crate::LuaError::InvalidInstruction("DrawingPriority requires one supported integer".into())
            })?;
        }
        "ScaleMinimum" => {
            state.scale_minimum = Some(value.parse::<u32>().map_err(|_| {
                crate::LuaError::InvalidInstruction("ScaleMinimum requires one nonnegative supported denominator".into())
            })?);
        }
        "ScaleMaximum" => {
            state.scale_maximum = Some(value.parse::<u32>().map_err(|_| {
                crate::LuaError::InvalidInstruction("ScaleMaximum requires one nonnegative supported denominator".into())
            })?);
        }
        "Id" => {
            state.id = if value.is_empty() {
                None
            } else {
                Some(def_decode(value))
            };
        }
        "Parent" => {
            state.parent = if value.is_empty() {
                None
            } else {
                Some(def_decode(value))
            };
        }
        "Hover" => {
            state.hover = match value {
                "true" => true,
                "false" => false,
                _ => return Err(crate::LuaError::InvalidInstruction("Hover requires true or false".into())),
            };
        }

        // ════════════════════════════════════════════
        // Transform State Commands (9a-11.2.2.2)
        // ════════════════════════════════════════════
        "LocalOffset" => {
            // Format: xOffsetMM,yOffsetMM
            state.local_offset = (
                params.first().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                params.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0),
            );
        }
        "LinePlacement" => {
            // Format: mode,offset[,endOffset][,visibleParts]
            state.line_placement_mode = params.first().unwrap_or(&"Relative").to_string();
            state.line_placement_offset = params.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.5);
            state.line_placement_end = params.get(2).and_then(|s| s.parse().ok());
            state.line_placement_visible_parts = params.get(3).is_some_and(|s| *s == "true");
        }
        "AreaPlacement" => {
            state.area_placement = params.first().unwrap_or(&"VisibleParts").to_string();
        }
        "AreaCRS" => {
            state.area_crs = params.first().unwrap_or(&"GlobalGeometry").to_string();
        }
        "Rotation" => {
            // Format: rotationCRS,rotation
            if params.len() >= 2 {
                state.rotation_crs = params[0].to_string();
                state.rotation = params[1].parse().unwrap_or(0.0);
            }
        }
        "ScaleFactor" => {
            state.scale_factor = value.parse().unwrap_or(1.0);
        }

        // ════════════════════════════════════════════
        // Line Style State Commands (9a-11.2.2.3)
        // ════════════════════════════════════════════
        "Dash" => {
            // Format: start,length  (accumulates for next LineStyle)
            let start = params.first().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let length = params.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            state.pending_dashes.push(DashPattern { start, length });

            // Also emit as DrawingCommand for backward compat
            result.commands.push(DrawingCommand::Dash {
                start: start as f32,
                length: length as f32,
            });
        }
        "LineSymbol" => {
            // Format: reference,position[,rotation[,crsType[,scaleFactor]]]
            let invalid = |text: &str| crate::LuaError::InvalidInstruction(text.into());
            if !(2..=5).contains(&params.len()) || params[0].is_empty() {
                return Err(invalid("Invalid LineSymbol reference/arity"));
            }
            let number = |i: usize, default: Option<f64>| -> Result<f64> {
                let value = match params.get(i) {
                    Some(value) => value
                        .parse::<f64>()
                        .map_err(|_| invalid("Invalid LineSymbol number"))?,
                    None => default.ok_or_else(|| invalid("Missing LineSymbol number"))?,
                };
                if !value.is_finite() {
                    return Err(invalid("Nonfinite LineSymbol number"));
                }
                Ok(value)
            };
            let position = number(1, None)?;
            let rotation = number(2, Some(0.))?;
            let scale_factor = number(4, Some(1.))?;
            let crs_type = params.get(3).copied().unwrap_or("LocalCRS");
            crs_type
                .parse::<ferrite_kernel::LineSymbolCrs>()
                .map_err(invalid)?;
            if state.pending_line_symbols.len() >= 4096 {
                return Err(invalid("Invalid LineSymbol dimensions/budget"));
            }
            state.pending_line_symbols.push(LineSymbolDef {
                reference: params[0].into(),
                position,
                rotation,
                crs_type: crs_type.into(),
                scale_factor,
            });
        }
        "LineStyle" => {
            // Format: name,intervalLength,width,token[,transparency[,capStyle[,joinStyle[,offset]]]]
            // OR inline: _simple_,dashOffset,width,color (from SimpleLineStyle helper)
            let parts_vec: Vec<&str> = value.split(',').collect();
            if let (Some(name), Some(width), Some(token)) =
                (parts_vec.first(), parts_vec.get(2), parts_vec.get(3))
            {
                let width: f32 = width.parse().map_err(|_| {
                    crate::LuaError::InvalidInstruction("Invalid LineStyle width".into())
                })?;
                let period: f64 = parts_vec
                    .get(1)
                    .filter(|x| !x.is_empty())
                    .map(|x| x.parse())
                    .transpose()
                    .map_err(|_| {
                        crate::LuaError::InvalidInstruction("Invalid LineStyle interval".into())
                    })?
                    .unwrap_or(0.);
                let pattern = if state.pending_dashes.is_empty() {
                    None
                } else {
                    Some(
                        ferrite_kernel::DashCycle::new(
                            period,
                            state.pending_dashes.iter().map(|d| (d.start, d.length)),
                        )
                        .map_err(|error| crate::LuaError::InvalidInstruction(error.into()))?,
                    )
                };
                let transparency: f32 = parts_vec
                    .get(4)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse())
                    .transpose()
                    .map_err(|_| {
                        crate::LuaError::InvalidInstruction("Invalid LineStyle transparency".into())
                    })?
                    .unwrap_or(0.);
                if !width.is_finite()
                    || width < 0.
                    || !transparency.is_finite()
                    || !(0. ..=1.).contains(&transparency)
                {
                    return Err(crate::LuaError::InvalidInstruction(
                        "Unsupported LineStyle width/transparency".into(),
                    ));
                }
                let invalid = |text: &str| crate::LuaError::InvalidInstruction(text.into());
                if !(4..=8).contains(&parts_vec.len())
                    || name.is_empty()
                    || token.is_empty()
                    || !period.is_finite()
                    || period < 0.
                {
                    return Err(invalid("Invalid LineStyle name/token/interval/arity"));
                }
                let cap = match parts_vec.get(5).copied().unwrap_or("Butt") {
                    "Butt" => ferrite_kernel::StrokeCap::Butt,
                    "Round" => ferrite_kernel::StrokeCap::Round,
                    "Square" => ferrite_kernel::StrokeCap::Square,
                    _ => return Err(invalid("Invalid LineStyle cap")),
                };
                let join = match parts_vec.get(6).copied().unwrap_or("Miter") {
                    "Miter" => ferrite_kernel::StrokeJoin::Miter,
                    "Round" => ferrite_kernel::StrokeJoin::Round,
                    "Bevel" => ferrite_kernel::StrokeJoin::Bevel,
                    _ => return Err(invalid("Invalid LineStyle join")),
                };
                let offset_mm = parts_vec
                    .get(7)
                    .map(|x| x.parse::<f64>())
                    .transpose()
                    .map_err(|_| invalid("Invalid LineStyle offset"))?
                    .unwrap_or(0.);
                if !offset_mm.is_finite() {
                    return Err(invalid("Invalid LineStyle offset/symbol interval"));
                }
                let symbols = state
                    .pending_line_symbols
                    .iter()
                    .map(|symbol| {
                        Ok(ferrite_kernel::StrokeSymbol {
                            reference: symbol.reference.clone(),
                            position_mm: symbol.position,
                            rotation_degrees: symbol.rotation,
                            crs: symbol.crs_type.parse().map_err(invalid)?,
                            scale_factor: symbol.scale_factor,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                state.defined_line_styles.insert(
                    (*name).into(),
                    ferrite_kernel::StrokeDefinition {
                        width_mm: width,
                        color_token: (*token).into(),
                        transparency,
                        dash_cycle: pattern,
                        cap,
                        join,
                        offset_mm,
                        interval_length_mm: period,
                        symbols,
                    },
                );
            } else {
                return Err(crate::LuaError::InvalidInstruction(
                    "Missing LineStyle mandatory fields".into(),
                ));
            }
            // Named LineStyle definitions are consumed by subsequent LineInstruction
            // Dashes and symbols accumulated so far apply to this style
            state.pending_dashes.clear();
            state.pending_line_symbols.clear();
        }

        // ════════════════════════════════════════════
        // Text Style State Commands (9a-11.2.2.4)
        // ════════════════════════════════════════════
        "FontColor" => {
            state.font_color = params.first().unwrap_or(&"").to_string();
            state.font_color_transparency = transparency_parameter(&params, 1, 0.)?;
        }
        "FontBackgroundColor" => {
            state.font_bg_color = params.first().unwrap_or(&"").to_string();
            state.font_bg_transparency = transparency_parameter(&params, 1, 1.)?;
        }
        "FontSize" => {
            state.font_size = value.parse().unwrap_or(10.0);
        }
        "FontProportion" => {
            state.font_proportion = value.to_string();
        }
        "FontWeight" => {
            state.font_weight = value.to_string();
        }
        "FontSlant" => {
            state.font_slant = value.to_string();
        }
        "FontSerifs" => {
            state.font_serifs = value == "true";
        }
        "FontUnderline" => {
            state.font_underline = value == "true";
        }
        "FontStrikethrough" => {
            state.font_strikethrough = value == "true";
        }
        "FontUpperline" => {
            state.font_upperline = value == "true";
        }
        "FontReference" => {
            state.font_reference = def_decode(value);
        }
        "TextAlignHorizontal" => {
            state.text_align_h = value.to_string();
        }
        "TextAlignVertical" => {
            state.text_align_v = value.to_string();
        }
        "TextVerticalOffset" => {
            state.text_vertical_offset = value.parse().unwrap_or(0.0);
        }

        // ════════════════════════════════════════════
        // Colour Override State Commands (9a-11.2.2.5)
        // ════════════════════════════════════════════
        "OverrideColor" => {
            // Format: colorToken,colorTransparency,overrideToken,overrideTransparency
            if params.len() >= 4 {
                state.color_overrides.push(ColorOverrideEntry {
                    color_token: params[0].to_string(),
                    color_transparency: params[1].parse().unwrap_or(0.0),
                    override_token: params[2].to_string(),
                    override_transparency: params[3].parse().unwrap_or(0.0),
                });
            }
        }
        "OverrideAll" => {
            // Format: token,transparency
            state.override_all = Some((
                params.first().unwrap_or(&"").to_string(),
                params.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0),
            ));
        }
        "ClearOverride" => {
            state.color_overrides.clear();
            state.override_all = None;
        }

        // ════════════════════════════════════════════
        // Geometry State Commands (9a-11.2.2.6)
        // ════════════════════════════════════════════
        "SpatialReference" => {
            geometry_arity(&params, 1, 2)?;
            let spatial_id = def_decode(params[0]);
            if spatial_id.is_empty() {
                return Err(crate::LuaError::InvalidInstruction("Missing spatial reference".into()));
            }
            let forward = match params.get(1).copied().unwrap_or("") {
                "" | "true" => true,
                "false" => false,
                _ => return Err(crate::LuaError::InvalidInstruction("SpatialReference forward requires a boolean".into())),
            };
            state.spatial_references.push((spatial_id.clone(), forward));
            result.commands.push(DrawingCommand::SpatialReference { spatial_id, forward });
        }
        "AugmentedPoint" => {
            if params.len() != 3
                || !matches!(params[0], "GeographicCRS" | "LocalCRS" | "PortrayalCRS")
            {
                return Err(crate::LuaError::InvalidInstruction(
                    "AugmentedPoint requires a supported CRS and two coordinates".into(),
                ));
            }
            let coordinate = |value: &str| -> Result<f64> {
                let value = value.parse::<f64>().map_err(|_| {
                    crate::LuaError::InvalidInstruction("Invalid augmented point coordinate".into())
                })?;
                if !value.is_finite() {
                    return Err(crate::LuaError::InvalidInstruction(
                        "Non-finite augmented point coordinate".into(),
                    ));
                }
                Ok(value)
            };
            let crs = params[0].to_string();
            let (x, y) = (coordinate(params[1])?, coordinate(params[2])?);
            state.augmented_point = Some((x, y));
            state.augmented_point_crs = Some(crs.clone());
            state.augmented_ray = None;
            state.augmented_path = None;
            result
                .commands
                .push(DrawingCommand::AugmentedPoint { crs, x, y });
        }
        "AugmentedRay" => {
            geometry_arity(&params, 4, 4)?;
            let direction_crs = geometry_crs(params[0])?;
            let direction = geometry_number(&params, 1, None)?;
            let length_crs = geometry_crs(params[2])?;
            let length = geometry_number(&params, 3, None)?;
            if length < 0. {
                return Err(crate::LuaError::InvalidInstruction("Negative augmented ray length".into()));
            }
            state.augmented_ray = Some(AugmentedRayDef { direction_crs, direction, length_crs, length });
            state.augmented_point = None;
            state.augmented_point_crs = None;
            state.augmented_path = None;
        }
        "AugmentedPath" => {
            geometry_arity(&params, 3, 3)?;
            let crs = AugmentedPathCrs {
                crs_position: geometry_crs(params[0])?,
                crs_angle: geometry_crs(params[1])?,
                crs_distance: geometry_crs(params[2])?,
            };
            state.augmented_path = Some(AugmentedPathDef {
                crs, segments: std::mem::take(&mut state.segment_list),
            });
            state.augmented_point = None;
            state.augmented_point_crs = None;
            state.augmented_ray = None;
        }
        "Polyline" => {
            if params.len() < 4 || params.len() % 2 != 0 {
                return Err(crate::LuaError::InvalidInstruction("Polyline requires at least two complete coordinate pairs".into()));
            }
            let mut points = Vec::with_capacity(params.len() / 2);
            for index in (0..params.len()).step_by(2) {
                points.push((geometry_number(&params, index, None)?, geometry_number(&params, index+1, None)?));
            }
            state.segment_list.push(PathSegment::Polyline(points));
        }
        "Arc3Points" => {
            geometry_arity(&params, 6, 6)?;
            let start = (geometry_number(&params, 0, None)?, geometry_number(&params, 1, None)?);
            let median = (geometry_number(&params, 2, None)?, geometry_number(&params, 3, None)?);
            let end = (geometry_number(&params, 4, None)?, geometry_number(&params, 5, None)?);
            state.segment_list.push(PathSegment::Arc3Points { start, median, end });
        }
        "ArcByRadius" => {
            geometry_arity(&params, 3, 5)?;
            let center = (geometry_number(&params, 0, None)?, geometry_number(&params, 1, None)?);
            let radius = geometry_number(&params, 2, None)?;
            let start_angle = geometry_number(&params, 3, Some(0.))?;
            let angular_distance = geometry_number(&params, 4, Some(360.))?;
            if radius < 0. {
                return Err(crate::LuaError::InvalidInstruction("Negative arc radius".into()));
            }
            state.segment_list.push(PathSegment::ArcByRadius { center, radius, start_angle, angular_distance });
        }
        "Annulus" => {
            geometry_arity(&params, 3, 6)?;
            let center = (geometry_number(&params, 0, None)?, geometry_number(&params, 1, None)?);
            let outer_radius = geometry_number(&params, 2, None)?;
            // Clause 9a-11.2.2.6 says omitted inner radius describes a sector.
            // Keep the application's established zero-radius policy. Table9a-12
            // lists outerRadius as its initial value; that discrepancy remains explicit.
            let inner_radius = geometry_number(&params, 3, Some(0.))?;
            let start_angle = geometry_number(&params, 4, Some(0.))?;
            let angular_distance = geometry_number(&params, 5, Some(360.))?;
            if inner_radius < 0. || outer_radius < inner_radius {
                return Err(crate::LuaError::InvalidInstruction("Invalid annulus radii".into()));
            }
            state.segment_list.push(PathSegment::Annulus { center, outer_radius, inner_radius, start_angle, angular_distance });
        }
        "ClearGeometry" => {
            if !value.is_empty() {
                return Err(crate::LuaError::InvalidInstruction("ClearGeometry takes no parameters".into()));
            }
            state.spatial_references.clear();
            state.augmented_point = None;
            state.augmented_point_crs = None;
            state.augmented_ray = None;
            state.augmented_path = None;
            state.segment_list.clear();
        }

        // ════════════════════════════════════════════
        // Coverage State Commands (9a-11.2.2.8)
        // ════════════════════════════════════════════
        "LookupEntry" => {
            if params.len() != 4 {
                return Err(crate::LuaError::InvalidInstruction(
                    "LookupEntry requires label,lower,upper,closure".into(),
                ));
            }
            let closure = ferrite_kernel::IntervalClosure::parse(params[3])
                .map_err(|e| crate::LuaError::InvalidInstruction(e.to_string()))?;
            let lower = if params[1].is_empty() {
                f64::NEG_INFINITY
            } else {
                params[1].parse().map_err(|_| {
                    crate::LuaError::InvalidInstruction("Invalid lookup lower bound".into())
                })?
            };
            let upper = if params[2].is_empty() {
                f64::INFINITY
            } else {
                params[2].parse().map_err(|_| {
                    crate::LuaError::InvalidInstruction("Invalid lookup upper bound".into())
                })?
            };
            let (color, transparency, end_color, pen_width) = state
                .coverage_color
                .clone()
                .map(|(c, t, e, w)| (Some(c), t, e, w))
                .unwrap_or((None, 0.0, None, 0.0));
            state.lookup_entries.push(LookupEntry {
                label: def_decode(params[0]),
                closure,
                range_min: lower,
                range_max: upper,
                color_token: color,
                transparency,
                end_color,
                pen_width,
                symbol: state.coverage_symbol.clone(),
                text: state.coverage_text.clone(),
            });
        }
        "CoverageColor" => {
            if params.len() < 2 {
                return Err(crate::LuaError::InvalidInstruction(
                    "CoverageColor requires startToken,startTransparency".into(),
                ));
            }
            let transparency = params[1].parse().map_err(|_| {
                crate::LuaError::InvalidInstruction("Invalid coverage transparency".into())
            })?;
            let end_color = if params.len() >= 4 {
                Some((
                    params[2].to_string(),
                    params[3].parse().map_err(|_| {
                        crate::LuaError::InvalidInstruction("Invalid end transparency".into())
                    })?,
                ))
            } else {
                None
            };
            let pen_width = if params.len() == 3 {
                params[2].parse().map_err(|_| {
                    crate::LuaError::InvalidInstruction("Invalid coverage pen width".into())
                })?
            } else {
                params.get(4).and_then(|v| v.parse().ok()).unwrap_or(0.0)
            };
            state.coverage_color =
                Some((params[0].to_string(), transparency, end_color, pen_width));
        }
        "NumericAnnotation" => {
            state.coverage_text = Some(value.to_string());
        }
        "SymbolAnnotation" => {
            state.coverage_symbol = params.first().map(|s| s.to_string());
        }

        // ════════════════════════════════════════════
        // Time State Commands (9a-11.2.2.7)
        // ════════════════════════════════════════════
        "Date" | "Time" | "DateTime" => {
            if params.is_empty() || params.len() > 2 {
                return Err(crate::LuaError::InvalidInstruction(format!(
                    "{cmd} requires begin and/or end"
                )));
            }
            let bound = |v: Option<&&str>| v.filter(|v| !v.is_empty()).map(|v| def_decode(v));
            let bounds = TemporalBounds::new(bound(params.first()), bound(params.get(1)))
                .map_err(|e| crate::LuaError::InvalidInstruction(e.to_string()))?;
            match cmd {
                "Date" => state.date = Some(bounds),
                "Time" => state.time = Some(bounds),
                _ => state.date_time = Some(bounds),
            }
        }
        "TimeValid" => {
            if params.len() > 1 {
                return Err(crate::LuaError::InvalidInstruction(
                    "TimeValid accepts one closure".into(),
                ));
            }
            let closure = IntervalClosure::parse(
                params
                    .first()
                    .copied()
                    .filter(|v| !v.is_empty())
                    .unwrap_or("closedInterval"),
            )
            .map_err(|e| crate::LuaError::InvalidInstruction(e.to_string()))?;
            let interval = TemporalInterval::new(
                state.date.clone(),
                state.time.clone(),
                state.date_time.clone(),
                closure,
            )
            .map_err(|e| crate::LuaError::InvalidInstruction(e.to_string()))?;
            state.time_intervals.push(interval);
            state.date = None;
            state.time = None;
            state.date_time = None;
        }
        "ClearTime" => {
            state.date = None;
            state.time = None;
            state.date_time = None;
            state.time_intervals.clear();
        }

        // ════════════════════════════════════════════
        // Drawing Commands (9a-11.2.1, Table 9a-3)
        // ════════════════════════════════════════════
        "PointInstruction" => {
            let symbol_ref = params.first().unwrap_or(&"").to_string();
            // Capture LinePlacement state for curve-based symbol placement (9a-11.2.2.2)
            let line_placement = Some((
                state.line_placement_mode.clone(),
                state.line_placement_offset,
            ));
            result.commands.push(DrawingCommand::PointInstruction {
                symbol_ref,
                rotation: state.rotation as f32,
                rotation_crs: state.rotation_crs.clone(),
                scale: state.scale_factor as f32,
                position: state.augmented_point,
                position_crs: state.augmented_point_crs.clone(),
                local_offset: state.local_offset,
                scale_factor: state.scale_factor,
                color_overrides: state.take_color_overrides(),
                override_all: state.take_override_all(),
                line_placement,
                line_placement_visible_parts: state.line_placement_visible_parts,
                spatial_refs: state.take_spatial_refs(),
                visibility: state.visibility_snapshot(),
            });
        }
        "LineInstruction" | "LineInstructionUnsuppressed" => {
            for name in &params {
                let style_refs = vec![name.to_string()];
                let simple_style = state.defined_line_styles.get(*name).cloned();
                let spatial_refs = state.take_spatial_refs();
                let augmented_segments = state.take_augmented_segments();
                let augmented_crs = state.augmented_path.as_ref().map(|p| p.crs.clone());
                let augmented_ray = state.augmented_ray.clone();
                let visibility = state.visibility_snapshot();
                if cmd == "LineInstruction" {
                    result.commands.push(DrawingCommand::LineInstruction {
                        style_refs,
                        simple_style,
                        spatial_refs,
                        augmented_segments,
                        augmented_crs,
                        augmented_ray,
                        visibility,
                    });
                } else {
                    result
                        .commands
                        .push(DrawingCommand::LineInstructionUnsuppressed {
                            style_refs,
                            simple_style,
                            spatial_refs,
                            augmented_segments,
                            augmented_crs,
                            augmented_ray,
                            visibility,
                        });
                }
            }
        }
        "ColorFill" => {
            let token = params.first().unwrap_or(&"").to_string();
            let transparency = transparency_parameter(&params, 1, 0.)?;
            result.commands.push(DrawingCommand::ColorFill {
                color_token: token,
                transparency,
                area_crs: state.area_crs.clone(),
                spatial_refs: state.take_spatial_refs(),
                visibility: state.visibility_snapshot(),
            });
        }
        "AreaFillReference" | "AreaInstruction" | "AreaFill" => {
            let reference = params.first().unwrap_or(&"").to_string();
            result.commands.push(DrawingCommand::AreaFillReference {
                reference,
                area_crs: state.area_crs.clone(),
                color_overrides: state.take_color_overrides(),
                override_all: state.take_override_all(),
                spatial_refs: state.take_spatial_refs(),
                visibility: state.visibility_snapshot(),
            });
        }
        "PixmapFill" => {
            result.commands.push(DrawingCommand::PixmapFill {
                reference: params.first().unwrap_or(&"").to_string(),
                area_crs: state.area_crs.clone(),
                color_overrides: state.take_color_overrides(),
                override_all: state.take_override_all(),
                spatial_refs: state.take_spatial_refs(),
                visibility: state.visibility_snapshot(),
            });
        }
        "SymbolFill" => {
            // Format: symbol,v1x,v1y,v2x,v2y[,clipSymbols]
            let symbol = params.first().unwrap_or(&"").to_string();
            let v1 = (
                params.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0),
                params.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.0),
            );
            let v2 = (
                params.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.0),
                params.get(4).and_then(|s| s.parse().ok()).unwrap_or(0.0),
            );
            let clip = params.get(5).is_none_or(|s| *s != "false");
            result.commands.push(DrawingCommand::SymbolFill {
                symbol,
                v1,
                v2,
                clip_symbols: clip,
                area_crs: state.area_crs.clone(),
                color_overrides: state.take_color_overrides(),
                override_all: state.take_override_all(),
                spatial_refs: state.take_spatial_refs(),
                visibility: state.visibility_snapshot(),
            });
        }
        "HatchFill" => {
            // Format: dirX,dirY,distance,lineStyle[,lineStyle]
            let direction = (
                params.first().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                params.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0),
            );
            let distance = params.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let line_styles: Vec<String> = params
                .get(3..)
                .unwrap_or(&[])
                .iter()
                .map(|s| s.to_string())
                .collect();
            let inline_styles = line_styles
                .iter()
                .map(|name| state.defined_line_styles.get(name).cloned())
                .collect();
            result.commands.push(DrawingCommand::HatchFill {
                direction,
                distance,
                line_styles,
                inline_styles,
                area_crs: state.area_crs.clone(),
                spatial_refs: state.take_spatial_refs(),
                visibility: state.visibility_snapshot(),
            });
        }
        "TextInstruction" => {
            result.commands.push(DrawingCommand::TextInstruction {
                text: def_decode(value),
                font_size: state.font_size,
                color_token: if state.font_color.is_empty() {
                    "CHBLK".to_string()
                } else {
                    state.font_color.clone()
                },
                color_transparency: state.font_color_transparency,
                bg_color_token: state.font_bg_color.clone(),
                bg_transparency: state.font_bg_transparency,
                bold: state.font_weight == "Bold",
                italic: state.font_slant == "Italics",
                font_proportion: state.font_proportion.clone(),
                serifs: state.font_serifs,
                underline: state.font_underline,
                strikethrough: state.font_strikethrough,
                upperline: state.font_upperline,
                font_reference: state.font_reference.clone(),
                h_align: state.text_align_h.clone(),
                v_align: state.text_align_v.clone(),
                vertical_offset: state.text_vertical_offset,
                local_offset: state.local_offset,
                rotation: state.rotation as f32,
                rotation_crs: state.rotation_crs.clone(),
                scale_factor: state.scale_factor,
                position: state.augmented_point,
                position_crs: state.augmented_point_crs.clone(),
                spatial_refs: state.take_spatial_refs(),
                visibility: state.visibility_snapshot(),
            });
        }
        "CoverageFill" => {
            // Format: attributeCode[,uom[,placement]]
            result.commands.push(DrawingCommand::CoverageFill {
                attribute_code: params.first().unwrap_or(&"").to_string(),
                uom: params.get(1).map(|s| s.to_string()),
                placement: params.get(2).map(|s| s.to_string()),
                lookup_entries: state.take_lookup_entries(),
                spatial_refs: state.take_spatial_refs(),
                visibility: state.visibility_snapshot(),
            });
        }
        "NullInstruction" => {
            result.commands.push(DrawingCommand::NullInstruction {
                visibility: state.visibility_snapshot(),
            });
        }

        // ════════════════════════════════════════════
        // Alert (9a-11.2.2.9)
        // ════════════════════════════════════════════
        "AlertReference" => {
            result.commands.push(DrawingCommand::AlertReference {
                reference: def_decode(value),
                visibility: state.visibility_snapshot(),
            });
        }

        _ => {
            tracing::trace!("Unknown DEF command: {}:{}", cmd, value);
        }
    }

    Ok(())
}

// ──────────────────────────────────────────────────────
// PortrayalResult
// ──────────────────────────────────────────────────────

/// Result from Lua portrayal emission
#[derive(Debug, Clone)]
pub struct PortrayalResult {
    pub feature_id: String,
    pub instructions: Vec<ParsedInstruction>,
    pub observed_parameters: Vec<String>,
}

impl PortrayalResult {
    pub fn new(feature_id: String) -> Self {
        PortrayalResult {
            feature_id,
            instructions: Vec::new(),
            observed_parameters: Vec::new(),
        }
    }

    /// Parse from Lua output (featureID, drawingInstructions, observedParams)
    pub fn parse(
        feature_id: &str,
        drawing_instructions: &str,
        observed_params: &str,
    ) -> Result<Self> {
        let mut result = PortrayalResult::new(feature_id.to_string());

        let instruction = parse_instruction_string(feature_id, drawing_instructions)?;
        result.instructions.push(instruction);

        for param in observed_params.split(',') {
            let param = param.trim();
            if !param.is_empty() {
                result.observed_parameters.push(param.to_string());
            }
        }

        Ok(result)
    }
}

// ──────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn viewing_group_strings_survive_and_reset() {
        let parsed=parse_instruction_string("1","ViewingGroup:31011,accuracy,01;PointInstruction:WRECKS01;ViewingGroup:31011;PointInstruction:WRECKS01").unwrap();
        let states = parsed
            .commands
            .iter()
            .filter_map(|c| c.visibility())
            .collect::<Vec<_>>();
        assert_eq!(states[0].viewing_groups, [31011]);
        assert_eq!(states[0].named_viewing_groups.as_ref(), ["accuracy", "01"]);
        assert_eq!(states[1].viewing_groups, [31011]);
        assert!(states[1].named_viewing_groups.is_empty());
        assert!(
            parse_instruction_string("1", "ViewingGroup:31011,;PointInstruction:WRECKS01").is_err()
        );
    }

    #[test]
    fn test_parse_simple() {
        let result = parse_instruction_string(
            "F123",
            "ViewingGroup:21010;DrawingPriority:15;NullInstruction",
        )
        .unwrap();

        assert_eq!(result.feature_id, "F123");
        assert_eq!(result.drawing_priority(), 15);
    }

    #[test]
    fn test_parse_point() {
        let result = parse_instruction_string(
            "F123",
            "ViewingGroup:23010;DrawingPriority:18;PointInstruction:LIGHTS01",
        )
        .unwrap();

        let drawing_cmds: Vec<_> = result
            .commands
            .iter()
            .filter(|c| c.visibility().is_some())
            .collect();
        assert_eq!(drawing_cmds.len(), 1);
        match &drawing_cmds[0] {
            DrawingCommand::PointInstruction {
                symbol_ref,
                visibility,
                ..
            } => {
                assert_eq!(symbol_ref, "LIGHTS01");
                assert_eq!(visibility.viewing_groups, vec![23010]);
                assert_eq!(visibility.drawing_priority, 18);
            }
            _ => panic!("Expected PointInstruction"),
        }
    }

    #[test]
    fn test_parse_line_style() {
        let result = parse_instruction_string(
            "F123",
            "Dash:0,3.6;LineStyle:_simple_,5.4,0.32,CSTLN;LineInstruction:_simple_",
        )
        .unwrap();

        let lines: Vec<_> = result
            .commands
            .iter()
            .filter_map(|c| match c {
                DrawingCommand::LineInstruction { simple_style, .. } => Some(simple_style),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "LineStyle defines a style; only LineInstruction draws"
        );
        assert_eq!(
            lines[0].as_ref().unwrap(),
            &ferrite_kernel::StrokeDefinition {
                width_mm: 0.32,
                color_token: "CSTLN".into(),
                transparency: 0.,
                dash_cycle: Some(ferrite_kernel::DashCycle::new(5.4, [(0., 3.6)]).unwrap()),
                interval_length_mm: 5.4,
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_parse_text_with_state() {
        let result = parse_instruction_string(
            "F1",
            "FontColor:CHGRF;FontSize:12;FontSlant:Italics;TextAlignHorizontal:Center;LocalOffset:3.51,0;TextInstruction:hello",
        )
        .unwrap();

        let text_cmds: Vec<_> = result
            .commands
            .iter()
            .filter(|c| matches!(c, DrawingCommand::TextInstruction { .. }))
            .collect();
        assert_eq!(text_cmds.len(), 1);
        match &text_cmds[0] {
            DrawingCommand::TextInstruction {
                color_token,
                font_size,
                italic,
                h_align,
                local_offset,
                text,
                ..
            } => {
                assert_eq!(color_token, "CHGRF");
                assert_eq!(*font_size, 12.0);
                assert!(italic);
                assert_eq!(h_align, "Center");
                assert_eq!(*local_offset, (3.51, 0.0));
                assert_eq!(text, "hello");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_def_decode() {
        assert_eq!(def_decode("Hello&m world!"), "Hello, world!");
        assert_eq!(def_decode("Foo&cbar"), "Foo:bar");
        assert_eq!(def_decode("a&sb&ac"), "a;b&c");
    }

    #[test]
    fn test_per_command_visibility() {
        let result = parse_instruction_string(
            "F1",
            "ViewingGroup:21010;DrawingPriority:5;PointInstruction:SYM1;ViewingGroup:23010;DrawingPriority:8;PointInstruction:SYM2",
        )
        .unwrap();

        let points: Vec<_> = result
            .commands
            .iter()
            .filter(|c| matches!(c, DrawingCommand::PointInstruction { .. }))
            .collect();
        assert_eq!(points.len(), 2);

        match &points[0] {
            DrawingCommand::PointInstruction { visibility, .. } => {
                assert_eq!(visibility.viewing_groups, vec![21010]);
                assert_eq!(visibility.drawing_priority, 5);
            }
            _ => unreachable!(),
        }
        match &points[1] {
            DrawingCommand::PointInstruction { visibility, .. } => {
                assert_eq!(visibility.viewing_groups, vec![23010]);
                assert_eq!(visibility.drawing_priority, 8);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_null_instruction() {
        let result = parse_instruction_string("F1", "NullInstruction").unwrap();
        assert!(matches!(
            result.commands[0],
            DrawingCommand::NullInstruction { .. }
        ));
    }

    #[test]
    fn test_geometry_commands() {
        let result = parse_instruction_string(
            "F1",
            "AugmentedPoint:GeographicCRS,127.5,-34.2;PointInstruction:SYM1;ClearGeometry;PointInstruction:SYM2",
        )
        .unwrap();

        let points: Vec<_> = result
            .commands
            .iter()
            .filter(|c| matches!(c, DrawingCommand::PointInstruction { .. }))
            .collect();
        assert_eq!(points.len(), 2);

        match &points[0] {
            DrawingCommand::PointInstruction { position, .. } => {
                assert_eq!(*position, Some((127.5, -34.2)));
            }
            _ => unreachable!(),
        }
        match &points[1] {
            DrawingCommand::PointInstruction { position, .. } => {
                assert_eq!(*position, None); // ClearGeometry reset it
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_color_override() {
        let result = parse_instruction_string(
            "F1",
            "OverrideColor:LITRD,0,LITGN,0;PointInstruction:LIGHTS01;ClearOverride;PointInstruction:LIGHTS02",
        )
        .unwrap();

        let points: Vec<_> = result
            .commands
            .iter()
            .filter(|c| matches!(c, DrawingCommand::PointInstruction { .. }))
            .collect();
        match &points[0] {
            DrawingCommand::PointInstruction {
                color_overrides, ..
            } => {
                assert_eq!(color_overrides.len(), 1);
                assert_eq!(color_overrides[0].color_token, "LITRD");
                assert_eq!(color_overrides[0].override_token, "LITGN");
            }
            _ => unreachable!(),
        }
        match &points[1] {
            DrawingCommand::PointInstruction {
                color_overrides, ..
            } => {
                assert!(color_overrides.is_empty()); // ClearOverride removed them
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod coverage_interval_tests {
    use super::*;
    #[test]
    fn official_s102_lookup_boundaries_and_preceding_colours() {
        let parsed=parse_instruction_string("depth", "CoverageColor:DEPIT,0;LookupEntry:Intertidal,,0,ltSemiInterval;CoverageColor:DEPVS,0;LookupEntry:Shallow,0,30,geLtInterval;CoverageColor:DEPDW,0;LookupEntry:Deep,30,,geSemiInterval;CoverageFill:depth").unwrap();
        let DrawingCommand::CoverageFill { lookup_entries, .. } = &parsed.commands[0] else {
            panic!("CoverageFill missing")
        };
        let select = |v| {
            lookup_entries
                .iter()
                .find(|e| e.closure.contains(v, e.range_min, e.range_max))
                .unwrap()
                .color_token
                .as_deref()
                .unwrap()
        };
        assert_eq!(select(-0.01), "DEPIT");
        assert_eq!(select(0.0), "DEPVS");
        assert_eq!(select(29.999), "DEPVS");
        assert_eq!(select(30.0), "DEPDW");
        assert_eq!(select(100.0), "DEPDW");
        assert!(lookup_entries.iter().all(|e| !e.closure.contains(
            f64::NAN,
            e.range_min,
            e.range_max
        )));
        assert_eq!(lookup_entries.len(), 3);
        assert!(parse_instruction_string("depth", "LookupEntry:Bad,0,1,inventedClosure").is_err());
    }
}

#[cfg(test)]
mod temporal_state_tests {
    use super::*;
    #[test]
    fn official_periodic_and_fixed_ranges_accumulate_and_clear() {
        let result = parse_instruction_string("f", "Date:----1101,----0331;TimeValid:closedInterval;PointInstruction:A;Date:20260101;TimeValid:geSemiInterval;PointInstruction:B;ClearTime;PointInstruction:C").unwrap();
        let states: Vec<_> = result
            .commands
            .iter()
            .filter_map(DrawingCommand::visibility)
            .collect();
        assert_eq!(
            states
                .iter()
                .map(|v| v.time_intervals.len())
                .collect::<Vec<_>>(),
            [1, 2, 0]
        );
        assert_eq!(
            states[0].time_intervals[0]
                .date
                .as_ref()
                .unwrap()
                .end
                .as_deref(),
            Some("----0331")
        );
        assert_eq!(
            states[1].time_intervals[1].closure,
            IntervalClosure::GreaterEqual
        );
        assert!(states[1].time_intervals[1]
            .date
            .as_ref()
            .unwrap()
            .end
            .is_none());
    }
    #[test]
    fn bound_state_is_consumed_and_closure_defaults_closed() {
        let result = parse_instruction_string(
            "f",
            "Date:,20261231;Time:12&c30&c00,13&c00&c00;TimeValid;PointInstruction:A",
        )
        .unwrap();
        let interval = &result.commands[0].visibility().unwrap().time_intervals[0];
        assert_eq!(interval.closure, IntervalClosure::Closed);
        assert!(interval.date.as_ref().unwrap().begin.is_none());
        assert_eq!(
            interval.time.as_ref().unwrap().begin.as_deref(),
            Some("12:30:00")
        );
        assert!(parse_instruction_string("f", "Date:20260101;TimeValid;TimeValid").is_err());
    }
    #[test]
    fn all_closures_and_datetime_timezone_preserved() {
        for closure in [
            "openInterval",
            "closedInterval",
            "geLtInterval",
            "gtLeInterval",
            "gtSemiInterval",
            "geSemiInterval",
            "ltSemiInterval",
            "leSemiInterval",
        ] {
            let src = format!("DateTime:2026-10-04T10&c00&c00+09&c00,2026-10-04T11&c00&c00+09&c00;TimeValid:{closure};NullInstruction");
            let result = parse_instruction_string("f", &src).unwrap();
            let interval = &result.commands[0].visibility().unwrap().time_intervals[0];
            assert_eq!(interval.closure, IntervalClosure::parse(closure).unwrap());
            assert_eq!(
                interval.date_time.as_ref().unwrap().begin.as_deref(),
                Some("2026-10-04T10:00:00+09:00")
            );
        }
    }
    #[test]
    fn malformed_time_commands_are_not_silently_ignored() {
        for src in [
            "Date",
            "Date:,",
            "Time:,,",
            "TimeValid",
            "Date:20260101;TimeValid:unknown",
            "Date:20260101;TimeValid:a,b",
            "Date:20260101;ClearTime;TimeValid",
        ] {
            assert!(parse_instruction_string("f", src).is_err(), "{src}");
        }
    }
}
#[cfg(test)]
mod style_definition_state_tests {
    use super::*;
    #[test]
    fn defining_a_style_preserves_ray_and_reuses_style_for_both_sector_limits() {
        let p=parse_instruction_string("Light", "AugmentedRay:GeographicCRS,90,LocalCRS,25;Dash:0,3.6;LineStyle:_simple_,5.4,0.32,CHBLK;LineInstruction:_simple_;AugmentedRay:GeographicCRS,180,LocalCRS,25;LineInstruction:_simple_").unwrap();
        let lines: Vec<_> = p
            .commands
            .iter()
            .filter_map(|c| match c {
                DrawingCommand::LineInstruction {
                    simple_style,
                    augmented_ray,
                    ..
                } => Some((
                    simple_style.as_ref().unwrap(),
                    augmented_ray.as_ref().unwrap(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, lines[1].0);
        assert_eq!(
            lines[0].0,
            &ferrite_kernel::StrokeDefinition {
                width_mm: 0.32,
                color_token: "CHBLK".into(),
                transparency: 0.,
                dash_cycle: Some(ferrite_kernel::DashCycle::new(5.4, [(0., 3.6)]).unwrap()),
                interval_length_mm: 5.4,
                ..Default::default()
            }
        );
        assert_eq!(lines[0].1.direction, 90.);
        assert_eq!(lines[1].1.direction, 180.);
        assert_eq!(lines[0].1.length, 25.);
    }
}

#[cfg(test)]
mod augmented_path_state_tests {
    use super::*;
    #[test]
    fn capture_then_reuse_and_clear_pending_segments() {
        let p=parse_instruction_string("1","Polyline:0,0,10,0;AugmentedPath:LocalCRS,LocalCRS,LocalCRS;LineInstruction:a;LineInstruction:b;Polyline:1,2,3,4;AugmentedPath:GeographicCRS,GeographicCRS,GeographicCRS;LineInstruction:c;ClearGeometry;LineInstruction:d").unwrap();
        let lines: Vec<_> = p
            .commands
            .iter()
            .filter_map(|c| {
                if let DrawingCommand::LineInstruction {
                    augmented_segments,
                    augmented_crs,
                    ..
                } = c
                {
                    Some((augmented_segments, augmented_crs))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(lines.len(), 4);
        for i in 0..2 {
            assert!(
                matches!(&lines[i].0[..],[PathSegment::Polyline(p)] if p==&vec![(0.,0.),(10.,0.)])
            );
            assert_eq!(lines[i].1.as_ref().unwrap().crs_position, "LocalCRS");
        }
        assert!(matches!(&lines[2].0[..],[PathSegment::Polyline(p)] if p==&vec![(1.,2.),(3.,4.)]));
        assert_eq!(lines[2].1.as_ref().unwrap().crs_position, "GeographicCRS");
        assert!(lines[3].0.is_empty());
        assert!(lines[3].1.is_none());
    }
    #[test]
    fn pending_segment_is_not_active_geometry() {
        let p = parse_instruction_string("1", "Polyline:0,0,10,0;LineInstruction:a").unwrap();
        assert!(
            matches!(&p.commands[0],DrawingCommand::LineInstruction{augmented_segments, ..} if augmented_segments.is_empty())
        );
    }
    #[test]
    fn omitted_annulus_inner_radius_is_sector() {
        let p = parse_instruction_string(
            "1",
            "Annulus:0,0,25,,30,60;AugmentedPath:LocalCRS,LocalCRS,LocalCRS;LineInstruction:a",
        )
        .unwrap();
        assert!(
            matches!(&p.commands[0],DrawingCommand::LineInstruction{augmented_segments,..} if matches!(&augmented_segments[0],PathSegment::Annulus{inner_radius,..} if *inner_radius==0.))
        );
    }
}

#[cfg(test)]
mod geometry_lifetime_tests {
    use super::*;
    #[test]
    fn spatial_references_survive_draw_until_clear() {
        let p=parse_instruction_string("1","SpatialReference:123,false;LineInstruction:a;LineInstruction:b;ClearGeometry;LineInstruction:c").unwrap();
        let refs: Vec<_> = p
            .commands
            .iter()
            .filter_map(|c| {
                if let DrawingCommand::LineInstruction { spatial_refs, .. } = c {
                    Some(spatial_refs)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(refs[0], &vec![("123".into(), false)]);
        assert_eq!(refs[1], refs[0]);
        assert!(refs[2].is_empty());
    }
    #[test]
    fn new_geometry_does_not_erase_pending_segments() {
        let p=parse_instruction_string("1","Polyline:0,0,2,3;AugmentedPoint:GeographicCRS,1,2;AugmentedRay:LocalCRS,90,LocalCRS,25;AugmentedPath:LocalCRS,LocalCRS,LocalCRS;LineInstruction:a").unwrap();
        assert!(p.commands.iter().any(|c|matches!(c,DrawingCommand::LineInstruction{augmented_segments,augmented_ray,..} if augmented_segments.len()==1 && augmented_ray.is_none())));
    }
}

#[cfg(test)]
mod multi_dash_definition_tests {
    use super::*;
    #[test]
    fn multiple_dash_definitions_and_nonzero_start_are_preserved() {
        let p = parse_instruction_string(
            "1",
            "Dash:2,2;Dash:6,1;LineStyle:offset,10,0.32,CHBLK;LineInstruction:offset",
        )
        .unwrap();
        assert!(p.commands.iter().any(|c|matches!(c,DrawingCommand::LineInstruction{simple_style:Some(ferrite_kernel::StrokeDefinition{dash_cycle:Some(cycle),..}),..} if cycle.period==10. && cycle.intervals==vec![(2.,4.),(6.,7.)])));
    }
}
#[cfg(test)]
mod stroke_definition_tests {
    use super::*;
    #[test]
    fn all_style_references_share_geometry_and_preserve_transparency() {
        let p=parse_instruction_string("1","AugmentedRay:LocalCRS,90,LocalCRS,25;LineStyle:base,,1.28,LITRD;LineStyle:overlay,,0.32,LITGN,0.5;LineInstruction:base,overlay;LineInstructionUnsuppressed:overlay,base").unwrap();
        let mut refs = Vec::new();
        let mut alpha = Vec::new();
        for cmd in p.commands {
            match cmd {
                DrawingCommand::LineInstruction {
                    style_refs,
                    simple_style,
                    augmented_ray,
                    ..
                }
                | DrawingCommand::LineInstructionUnsuppressed {
                    style_refs,
                    simple_style,
                    augmented_ray,
                    ..
                } => {
                    refs.push(style_refs[0].clone());
                    alpha.push(simple_style.unwrap().transparency);
                    assert_eq!(augmented_ray.unwrap().length, 25.);
                }
                _ => {}
            }
        }
        assert_eq!(refs, vec!["base", "overlay", "overlay", "base"]);
        assert_eq!(alpha, vec![0., 0.5, 0.5, 0.]);
    }
    #[test]
    fn unsupported_transparency_is_reported() {
        for value in ["NaN", "-0.1", "1.1"] {
            assert!(parse_instruction_string(
                "1",
                &format!("LineStyle:a,,1,CHBLK,{value};LineInstruction:a")
            )
            .is_err());
        }
    }
}

#[cfg(test)]
mod colour_transparency_tests {
    use super::*;
    #[test]
    fn invalid_colour_transparency_is_reported_instead_of_silently_changing_the_command() {
        for command in ["ColorFill", "FontColor", "FontBackgroundColor"] {
            for value in ["NaN", "inf", "-0.1", "1.1", "bad"] {
                assert!(
                    parse_instruction_string("1", &format!("{command}:CHBLK,{value}")).is_err(),
                    "{command} {value}"
                );
            }
        }
    }
    #[test]
    fn glyph_and_background_transparency_survive_text_state_reuse() {
        let parsed=parse_instruction_string("1","FontColor:CHBLK,0.25;FontBackgroundColor:LITRD,0.5;TextInstruction:first;TextInstruction:second").unwrap();
        let texts: Vec<_> = parsed
            .commands
            .iter()
            .filter_map(|c| {
                if let DrawingCommand::TextInstruction {
                    color_transparency,
                    bg_transparency,
                    bg_color_token,
                    ..
                } = c
                {
                    Some((
                        *color_transparency,
                        *bg_transparency,
                        bg_color_token.as_str(),
                    ))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(texts, vec![(0.25, 0.5, "LITRD"); 2]);
    }
}

#[cfg(test)]
mod line_symbol_visibility_tests {
    use super::*;
    #[test]
    fn visible_parts_is_carried_by_each_point_command_and_can_be_reset() {
        let p = parse_instruction_string("1", "LinePlacement:Relative,0.5,,true;PointInstruction:A;LinePlacement:Absolute,10;PointInstruction:B").unwrap();
        let points: Vec<_> = p
            .commands
            .iter()
            .filter_map(|c| {
                if let DrawingCommand::PointInstruction {
                    line_placement,
                    line_placement_visible_parts,
                    ..
                } = c
                {
                    Some((line_placement.clone(), *line_placement_visible_parts))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            points,
            vec![
                (Some(("Relative".into(), 0.5)), true),
                (Some(("Absolute".into(), 10.)), false)
            ]
        );
    }
}

#[cfg(test)]
mod augmented_origin_tests {
    use super::*;
    #[test]
    fn crs_survives_commands_and_resets_with_geometry() {
        let parsed=parse_instruction_string("f","AugmentedPoint:LocalCRS,3.2,4;PointInstruction:A;TextInstruction:B;ClearGeometry;PointInstruction:C;AugmentedPoint:PortrayalCRS,5,6;PointInstruction:D;AugmentedRay:LocalCRS,90,LocalCRS,5;PointInstruction:E").unwrap();
        let positions: Vec<_> = parsed
            .commands
            .iter()
            .filter_map(|command| match command {
                DrawingCommand::PointInstruction {
                    position,
                    position_crs,
                    ..
                }
                | DrawingCommand::TextInstruction {
                    position,
                    position_crs,
                    ..
                } => Some((*position, position_crs.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(
            positions,
            vec![
                (Some((3.2, 4.)), Some("LocalCRS")),
                (Some((3.2, 4.)), Some("LocalCRS")),
                (None, None),
                (Some((5., 6.)), Some("PortrayalCRS")),
                (None, None)
            ]
        );
    }
    #[test]
    fn malformed_augmented_coordinates_do_not_become_zero() {
        for value in [
            "AugmentedPoint:GeographicCRS,nope,0",
            "AugmentedPoint:GeographicCRS,NaN,0",
            "AugmentedPoint:GeographicCRS,0,inf",
            "AugmentedPoint:InventedCRS,0,0",
            "AugmentedPoint:LocalCRS,0",
            "AugmentedPoint:LocalCRS,0,0,0",
        ] {
            assert!(parse_instruction_string("f", value).is_err(), "{value}");
        }
    }
}

#[cfg(test)]
mod complete_line_style_tests {
    use super::*;
    #[test]
    fn symbol_cap_join_offset_and_interval_snapshots_survive_redefinition() {
        let parsed = parse_instruction_string("1", "LineSymbol:WRECKS01,1,30,LineCRS,1.5;Dash:1,2;LineStyle:L,5,0.32,CHBLK,0.2,Square,Bevel,-0.4;HatchFill:1,0,3,L;LineInstruction:L;LineStyle:L,,0.8,CHBLK;HatchFill:1,0,3,L").unwrap();
        let snapshots: Vec<_> = parsed
            .commands
            .iter()
            .filter_map(|c| match c {
                DrawingCommand::HatchFill { inline_styles, .. } => inline_styles[0].as_ref(),
                DrawingCommand::LineInstruction { simple_style, .. } => simple_style.as_ref(),
                _ => None,
            })
            .collect();
        assert_eq!(snapshots.len(), 3);
        assert_eq!(snapshots[0], snapshots[1]);
        let first = snapshots[0];
        assert_eq!(first.cap, ferrite_kernel::StrokeCap::Square);
        assert_eq!(first.join, ferrite_kernel::StrokeJoin::Bevel);
        assert_eq!(first.offset_mm, -0.4);
        assert_eq!(first.interval_length_mm, 5.);
        assert_eq!(
            first.symbols,
            vec![ferrite_kernel::StrokeSymbol {
                reference: "WRECKS01".into(),
                position_mm: 1.,
                rotation_degrees: 30.,
                crs: ferrite_kernel::LineSymbolCrs::LineCRS,
                scale_factor: 1.5,
            }]
        );
        let second = snapshots[2];
        assert!(second.symbols.is_empty());
        assert_eq!(second.offset_mm, 0.);
        assert_eq!(second.cap, ferrite_kernel::StrokeCap::Butt);
        assert_eq!(second.join, ferrite_kernel::StrokeJoin::Miter);
        assert_eq!(second.interval_length_mm, 0.);
    }
    #[test]
    fn malformed_symbol_and_style_fields_are_not_replaced_by_defaults() {
        for text in [
            "LineSymbol:R",
            "LineSymbol:,1",
            "LineSymbol:R,bad",
            "LineSymbol:R,NaN",
            "LineSymbol:R,1,bad",
            "LineSymbol:R,1,0,GeographicCRS",
            "LineSymbol:R,1,0,LineCRS,inf",
            "LineStyle:L",
            "LineStyle:L,5,1,C,0,unknown",
            "LineStyle:L,5,1,C,0,Butt,unknown",
            "LineStyle:L,5,1,C,0,Butt,Miter,NaN",
            "LineStyle:L,-1,1,C",
        ] {
            assert!(parse_instruction_string("1", text).is_err(), "{text}");
        }
        for value in [
            "LineSymbol:R,-1,0,LineCRS,-1;LineStyle:L,,1,C",
            "LineSymbol:R,1,0,LineCRS,0;LineStyle:L,5,1,C",
        ] {
            assert!(parse_instruction_string("1", value).is_ok(), "{value}");
        }
        for crs in ["LocalCRS", "LineCRS", "PortrayalCRS"] {
            assert!(parse_instruction_string(
                "1",
                &format!("LineSymbol:R,1,0,{crs},1;LineStyle:L,5,1,C")
            )
            .is_ok());
        }
    }
}

#[cfg(test)]
mod visibility_state_contract {
    use super::*;
    #[test]
    fn initial_state_and_per_emission_reset_follow_table_9a_7() {
        let first=parse_instruction_string("one", "ViewingGroup:31011;DrawingPriority:42;ScaleMinimum:10000;ScaleMaximum:100;Hover:true;PointInstruction:A").unwrap();
        let first=first.commands[0].visibility().unwrap();
        assert_eq!(first.drawing_priority,42); assert_eq!(first.viewing_groups,[31011]);
        assert_eq!(first.scale_minimum,Some(10000)); assert_eq!(first.scale_maximum,Some(100)); assert!(first.hover);
        let second=parse_instruction_string("two", "PointInstruction:A").unwrap();
        let v=second.commands[0].visibility().unwrap();
        assert!(v.viewing_groups.is_empty()); assert!(v.named_viewing_groups.is_empty());
        assert_eq!(v.drawing_priority,0); assert!(!v.hover);
        // None is the consumer's unbounded scale representation, rather than
        // a hard-coded arbitrary maximum/minimum denominator.
        assert_eq!(v.scale_minimum,None); assert_eq!(v.scale_maximum,None);
        assert_eq!(ParsedInstruction::new("empty".into()).drawing_priority(),0);
        assert!(VisibilityState::default().viewing_groups.is_empty());
        assert_eq!(VisibilityState::default().drawing_priority,0);
    }
    #[test]
    fn malformed_visibility_values_cannot_reuse_previous_or_clear_restriction() {
        for (command, values) in [
            ("DrawingPriority", vec!["", "1.5", "NaN", "2147483648", "4,5", "bad"]),
            ("ScaleMinimum",vec!["", "-1", "1.5", "4294967296", "1,2", "NaN"]),
            ("ScaleMaximum",vec!["", "-1", "1.5", "4294967296", "1,2", "NaN"]),
            ("Hover",vec!["", "TRUE", "0", "false,true", "bad"]),
        ] {
            for value in values {
                let def=format!("DrawingPriority:42;ScaleMinimum:1000;ScaleMaximum:100;Hover:true;{command}:{value};PointInstruction:A");
                assert!(parse_instruction_string("f", &def).is_err(),"{def}");
            }
        }
        let parsed=parse_instruction_string("f", "DrawingPriority:-2147483648;PointInstruction:A;DrawingPriority:2147483647;ScaleMinimum:4294967295;ScaleMaximum:0;Hover:false;PointInstruction:B").unwrap();
        assert_eq!(parsed.commands[0].visibility().unwrap().drawing_priority,i32::MIN);
        let v=parsed.commands[1].visibility().unwrap();assert_eq!(v.drawing_priority,i32::MAX);
        assert_eq!(v.scale_minimum,Some(u32::MAX));assert_eq!(v.scale_maximum,Some(0));assert!(!v.hover);
    }
}

#[cfg(test)]
mod display_plane_reference_tests {
    use super::*;
    #[test]
    fn case_sensitive_decoded_names_survive_snapshots_and_emission_reset() {
        let p=parse_instruction_string("f", "DisplayPlane:overRadar;PointInstruction:A;DisplayPlane:OverRadar;PointInstruction:B;DisplayPlane:plane&csection&sone&mtwo&aend;PointInstruction:C").unwrap();
        let refs:Vec<_>=p.commands.iter().map(|c|c.visibility().unwrap().display_plane.reference()).collect();
        assert_eq!(refs,[Some("overRadar"),Some("OverRadar"),Some("plane:section;one,two&end")]);
        assert_eq!(p.display_plane().reference(),Some("overRadar"));
        let shared=parse_instruction_string("s","DisplayPlane:OverRADAR;PointInstruction:A;PointInstruction:B").unwrap();
        let DisplayPlane::Named(a)=&shared.commands[0].visibility().unwrap().display_plane else { panic!() };
        let DisplayPlane::Named(b)=&shared.commands[1].visibility().unwrap().display_plane else { panic!() };
        assert_eq!(a.as_ref(),"OverRADAR");
        assert!(std::sync::Arc::ptr_eq(a,b),"immutable plane identifiers must not allocate once per drawing command");

        assert_eq!(parse_instruction_string("g","PointInstruction:A").unwrap().display_plane(),DisplayPlane::Unspecified);
        for def in ["DisplayPlane;PointInstruction:A","DisplayPlane:;PointInstruction:A","DisplayPlane:A,B;PointInstruction:A"] {
            assert!(parse_instruction_string("f",def).is_err(),"{def}");
        }
    }
}

#[cfg(test)]
mod geometry_validation_tests {
    use super::*;
    #[test]
    fn malformed_numbers_and_argument_lists_never_create_replacement_geometry() {
        for input in [
            "SpatialReference:", "SpatialReference:12,maybe", "SpatialReference:12,false,extra",
            "Polyline:", "Polyline:1,2", "Polyline:1,2,3", "Polyline:1,2,bad,4", "Polyline:1,2,3,NaN",
            "Arc3Points:1,2,3,4,5", "Arc3Points:1,2,3,4,5,6,7", "Arc3Points:1,2,3,4,NaN,6",
            "ArcByRadius:1,2", "ArcByRadius:1,2,bad", "ArcByRadius:1,2,-1", "ArcByRadius:1,2,3,bad", "ArcByRadius:1,2,3,0,inf", "ArcByRadius:1,2,3,0,90,extra",
            "Annulus:1,2,3,bad", "Annulus:1,2,3,4", "Annulus:1,2,3,-1", "Annulus:1,2,3,2,0,NaN", "Annulus:1,2,3,2,0,90,extra",
            "AugmentedRay:LocalCRS,foo,LocalCRS,10", "AugmentedRay:unknown,90,LocalCRS,10", "AugmentedRay:LocalCRS,90,LocalCRS,-1", "AugmentedRay:LocalCRS,90,LocalCRS,inf", "AugmentedRay:LocalCRS,90,LocalCRS,1,extra",
            "AugmentedPath:LocalCRS,LocalCRS", "AugmentedPath:LocalCRS,bogus,LocalCRS", "AugmentedPath:LocalCRS,LocalCRS,LocalCRS,extra",
            "ClearGeometry:extra",
        ] {
            assert!(parse_instruction_string("f", &format!("PointInstruction:VALID;{input};LineInstruction:L")).is_err(), "{input}");
        }
    }
    #[test]
    fn invalid_geometry_command_does_not_mutate_active_pending_or_drawn_state() {
        let mut state=DrawingState::default(); let mut result=ParsedInstruction::new("f".into());
        for (name,value) in [("SpatialReference","12,false"),("Polyline","0,0,1,1"),("AugmentedRay","GeographicCRS,30,PortrayalCRS,5"),("LineInstruction","L")] {
            parse_command(&mut result,&mut state,name,value).unwrap();
        }
        let before=format!("{state:?}");let commands=format!("{:?}",result.commands);
        for (name,value) in [("SpatialReference","14,maybe"),("Polyline","0,0,bad,1"),("Arc3Points","0,0,1,2,3,NaN"),("ArcByRadius","0,0,5,inf"),("Annulus","0,0,5,6"),("AugmentedPath","LocalCRS,unknown,LocalCRS"),("AugmentedRay","GeographicCRS,30,PortrayalCRS,-5"),("ClearGeometry","x")] {
            assert!(parse_command(&mut result,&mut state,name,value).is_err());
            assert_eq!(format!("{state:?}"),before,"{name}");assert_eq!(format!("{:?}",result.commands),commands,"{name}");
        }
    }
    #[test]
    fn finite_signed_geometry_defaults_and_segment_order_remain_intact() {
        let p=parse_instruction_string("f","SpatialReference:A&cB,false;Polyline:0,0,1,2;Arc3Points:0,1,1,0,0,-1;ArcByRadius:3,4,5,,-90;Annulus:0,0,9,,10,-30;AugmentedPath:LocalCRS,GeographicCRS,PortrayalCRS;LineInstruction:L;ClearGeometry;LineInstruction:M").unwrap();
        let lines=p.commands.iter().filter_map(|c|if let DrawingCommand::LineInstruction{augmented_segments,augmented_crs,spatial_refs,..}=c {Some((augmented_segments,augmented_crs,spatial_refs))}else{None}).collect::<Vec<_>>();
        assert_eq!(lines.len(),2);assert_eq!(lines[0].0.len(),4);
        assert_eq!(*lines[0].2,vec![("A:B".into(),false)]);
        assert!(matches!(&lines[0].0[0],PathSegment::Polyline(v) if v==&vec![(0.,0.),(1.,2.)]));
        assert!(matches!(&lines[0].0[1],PathSegment::Arc3Points{start,median,end} if *start==(0.,1.) && *median==(1.,0.) && *end==(0.,-1.)));
        assert!(matches!(&lines[0].0[2],PathSegment::ArcByRadius{start_angle,angular_distance,..} if *start_angle==0. && *angular_distance== -90.));
        assert!(matches!(&lines[0].0[3],PathSegment::Annulus{inner_radius,start_angle,angular_distance,..} if *inner_radius==0. && *start_angle==10. && *angular_distance== -30.));
        assert_eq!(lines[0].1.as_ref().unwrap().crs_angle,"GeographicCRS");
        assert!(lines[1].0.is_empty() && lines[1].1.is_none() && lines[1].2.is_empty());
    }
}
