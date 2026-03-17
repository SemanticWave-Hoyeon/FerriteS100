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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayPlane {
    #[default]
    UnderRadar,
    OverRadar,
}

/// Per-drawing-command visibility and ordering state (9a-11.2.2.1)
/// Captured as a snapshot when each drawing command is created.
#[derive(Debug, Clone)]
pub struct VisibilityState {
    pub viewing_groups: Vec<u32>,
    pub drawing_priority: i32,
    pub display_plane: DisplayPlane,
    pub scale_minimum: Option<u32>,
    pub scale_maximum: Option<u32>,
    pub id: Option<String>,
    pub parent: Option<String>,
    pub hover: bool,
}

impl Default for VisibilityState {
    fn default() -> Self {
        VisibilityState {
            viewing_groups: vec![21010],
            drawing_priority: 5,
            display_plane: DisplayPlane::UnderRadar,
            scale_minimum: None,
            scale_maximum: None,
            id: None,
            parent: None,
            hover: false,
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
        local_offset: (f64, f64),
        scale_factor: f64,
        /// Color overrides to apply to this symbol
        color_overrides: Vec<ColorOverrideEntry>,
        /// Override all non-transparent colours
        override_all: Option<(String, f64)>,
        /// LinePlacement for placing symbol on a curve (9a-11.2.2.2)
        /// (mode, offset) where mode is "Relative" or "Absolute"
        line_placement: Option<(String, f64)>,
        /// Spatial references to use for curve placement
        spatial_refs: Vec<(String, bool)>,
        visibility: VisibilityState,
    },

    /// LineInstruction:lineStyle[,lineStyle,...] (9a-11.2.1)
    /// Line segments with higher drawing priority suppress coincident lower ones.
    LineInstruction {
        style_refs: Vec<String>,
        /// Inline simple line style: (width, color_token)
        simple_style: Option<(f32, String)>,
        /// Spatial references to use instead of feature geometry
        spatial_refs: Vec<(String, bool)>,
        /// Augmented geometry segments to use instead of feature geometry
        augmented_segments: Vec<PathSegment>,
        /// AugmentedRay: line from feature point in given direction/length
        augmented_ray: Option<AugmentedRayDef>,
        visibility: VisibilityState,
    },

    /// LineInstructionUnsuppressed:lineStyle[,lineStyle,...] (9a-11.2.1)
    /// Same as LineInstruction but without line suppression.
    LineInstructionUnsuppressed {
        style_refs: Vec<String>,
        simple_style: Option<(f32, String)>,
        spatial_refs: Vec<(String, bool)>,
        augmented_segments: Vec<PathSegment>,
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
        5
    }

    pub fn display_plane(&self) -> DisplayPlane {
        for cmd in &self.commands {
            if let Some(vis) = cmd.visibility() {
                return vis.display_plane;
            }
        }
        DisplayPlane::UnderRadar
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
    augmented_ray: Option<AugmentedRayDef>,
    augmented_path: Option<AugmentedPathDef>,
    segment_list: Vec<PathSegment>,

    // ── Coverage (9a-11.2.2.8) ──
    lookup_entries: Vec<LookupEntry>,

    // ── Time (9a-11.2.2.7) ──
    date: Option<String>,
    time: Option<String>,
    date_time: Option<String>,
    time_valid: Option<(String, String)>,

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
#[allow(dead_code)]
struct AugmentedPathDef {
    crs_position: String,
    crs_angle: String,
    crs_distance: String,
}

impl Default for DrawingState {
    fn default() -> Self {
        DrawingState {
            // Visibility
            viewing_groups: vec![21010],
            display_plane: DisplayPlane::UnderRadar,
            drawing_priority: 5,
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
            augmented_ray: None,
            augmented_path: None,
            segment_list: Vec::new(),
            // Coverage
            lookup_entries: Vec::new(),
            // Time
            date: None,
            time: None,
            date_time: None,
            time_valid: None,
            // Alert
            alert_reference: None,
        }
    }
}

impl DrawingState {
    fn visibility_snapshot(&self) -> VisibilityState {
        VisibilityState {
            viewing_groups: self.viewing_groups.clone(),
            drawing_priority: self.drawing_priority,
            display_plane: self.display_plane,
            scale_minimum: self.scale_minimum,
            scale_maximum: self.scale_maximum,
            id: self.id.clone(),
            parent: self.parent.clone(),
            hover: self.hover,
        }
    }

    /// Collect spatial references; clears them after snapshot.
    fn take_spatial_refs(&mut self) -> Vec<(String, bool)> {
        std::mem::take(&mut self.spatial_references)
    }

    /// Collect augmented geometry segments; clears them after snapshot.
    fn take_augmented_segments(&mut self) -> Vec<PathSegment> {
        std::mem::take(&mut self.segment_list)
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
            for v in &params {
                if let Ok(vg) = v.trim().parse::<u32>() {
                    state.viewing_groups.push(vg);
                }
            }
        }
        "DisplayPlane" => {
            state.display_plane = if value == "OverRadar" {
                DisplayPlane::OverRadar
            } else {
                DisplayPlane::UnderRadar
            };
        }
        "DrawingPriority" => {
            if let Ok(p) = value.parse::<i32>() {
                state.drawing_priority = p;
            }
        }
        "ScaleMinimum" => {
            state.scale_minimum = value.parse().ok();
        }
        "ScaleMaximum" => {
            state.scale_maximum = value.parse().ok();
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
            state.hover = value == "true";
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
            state.pending_line_symbols.push(LineSymbolDef {
                reference: params.first().unwrap_or(&"").to_string(),
                position: params.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0),
                rotation: params.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.0),
                crs_type: params.get(3).unwrap_or(&"LocalCRS").to_string(),
                scale_factor: params.get(4).and_then(|s| s.parse().ok()).unwrap_or(1.0),
            });
        }
        "LineStyle" => {
            // Format: name,intervalLength,width,token[,transparency[,capStyle[,joinStyle[,offset]]]]
            // OR inline: _simple_,dashOffset,width,color (from SimpleLineStyle helper)
            let parts_vec: Vec<&str> = value.split(',').collect();
            if parts_vec.first() == Some(&"_simple_") {
                // Inline simple style from featurePortrayal:SimpleLineStyle()
                let width = parts_vec
                    .get(2)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.32);
                let color = parts_vec.get(3).unwrap_or(&"CSTLN").to_string();
                result.commands.push(DrawingCommand::LineInstruction {
                    style_refs: Vec::new(),
                    simple_style: Some((width, color)),
                    spatial_refs: state.take_spatial_refs(),
                    augmented_segments: state.take_augmented_segments(),
                    augmented_ray: state.augmented_ray.take(),
                    visibility: state.visibility_snapshot(),
                });
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
            state.font_color_transparency =
                params.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        }
        "FontBackgroundColor" => {
            state.font_bg_color = params.first().unwrap_or(&"").to_string();
            state.font_bg_transparency = params.get(1).and_then(|s| s.parse().ok()).unwrap_or(1.0);
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
            // Format: reference[,forward]
            let spatial_id = params.first().unwrap_or(&"").to_string();
            let forward = params.get(1).is_none_or(|s| *s != "false");
            state.spatial_references.push((spatial_id.clone(), forward));
            // Also emit for backward compat
            result.commands.push(DrawingCommand::SpatialReference {
                spatial_id,
                forward,
            });
        }
        "AugmentedPoint" => {
            // Format: crs,x,y
            if params.len() >= 3 {
                let crs = params[0].to_string();
                let x = params[1].parse::<f64>().unwrap_or(0.0);
                let y = params[2].parse::<f64>().unwrap_or(0.0);
                state.augmented_point = Some((x, y));
                state.augmented_ray = None;
                state.augmented_path = None;
                state.segment_list.clear();
                result
                    .commands
                    .push(DrawingCommand::AugmentedPoint { crs, x, y });
            }
        }
        "AugmentedRay" => {
            // Format: crsDirection,direction,crsLength,length
            if params.len() >= 4 {
                state.augmented_ray = Some(AugmentedRayDef {
                    direction_crs: params[0].to_string(),
                    direction: params[1].parse().unwrap_or(0.0),
                    length_crs: params[2].to_string(),
                    length: params[3].parse().unwrap_or(0.0),
                });
                state.augmented_point = None;
                state.augmented_path = None;
                state.segment_list.clear();
            }
        }
        "AugmentedPath" => {
            // Format: crsPosition,crsAngle,crsDistance
            if params.len() >= 3 {
                state.augmented_path = Some(AugmentedPathDef {
                    crs_position: params[0].to_string(),
                    crs_angle: params[1].to_string(),
                    crs_distance: params[2].to_string(),
                });
                state.augmented_point = None;
                state.augmented_ray = None;
                state.segment_list.clear();
            }
        }
        "Polyline" => {
            // Format: x1,y1,x2,y2,...
            let mut points = Vec::new();
            let mut i = 0;
            while i + 1 < params.len() {
                if let (Ok(x), Ok(y)) = (params[i].parse::<f64>(), params[i + 1].parse::<f64>()) {
                    points.push((x, y));
                }
                i += 2;
            }
            if !points.is_empty() {
                state.segment_list.push(PathSegment::Polyline(points));
            }
        }
        "Arc3Points" => {
            // Format: startX,startY,medianX,medianY,endX,endY
            if params.len() >= 6 {
                state.segment_list.push(PathSegment::Arc3Points {
                    start: (
                        params[0].parse().unwrap_or(0.0),
                        params[1].parse().unwrap_or(0.0),
                    ),
                    median: (
                        params[2].parse().unwrap_or(0.0),
                        params[3].parse().unwrap_or(0.0),
                    ),
                    end: (
                        params[4].parse().unwrap_or(0.0),
                        params[5].parse().unwrap_or(0.0),
                    ),
                });
            }
        }
        "ArcByRadius" => {
            // Format: centerX,centerY,radius[,startAngle[,angularDistance]]
            if params.len() >= 3 {
                state.segment_list.push(PathSegment::ArcByRadius {
                    center: (
                        params[0].parse().unwrap_or(0.0),
                        params[1].parse().unwrap_or(0.0),
                    ),
                    radius: params[2].parse().unwrap_or(0.0),
                    start_angle: params.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    angular_distance: params.get(4).and_then(|s| s.parse().ok()).unwrap_or(360.0),
                });
            }
        }
        "Annulus" => {
            // Format: centerX,centerY,outerRadius[,innerRadius[,startAngle[,angularDistance]]]
            if params.len() >= 3 {
                let outer = params[2].parse().unwrap_or(0.0);
                state.segment_list.push(PathSegment::Annulus {
                    center: (
                        params[0].parse().unwrap_or(0.0),
                        params[1].parse().unwrap_or(0.0),
                    ),
                    outer_radius: outer,
                    inner_radius: params.get(3).and_then(|s| s.parse().ok()).unwrap_or(outer),
                    start_angle: params.get(4).and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    angular_distance: params.get(5).and_then(|s| s.parse().ok()).unwrap_or(360.0),
                });
            }
        }
        "ClearGeometry" => {
            state.spatial_references.clear();
            state.augmented_point = None;
            state.augmented_ray = None;
            state.augmented_path = None;
            state.segment_list.clear();
        }

        // ════════════════════════════════════════════
        // Coverage State Commands (9a-11.2.2.8)
        // ════════════════════════════════════════════
        "LookupEntry" => {
            // Format depends on sub-type; typical: rangeMin,rangeMax,color,transparency
            if params.len() >= 2 {
                state.lookup_entries.push(LookupEntry {
                    range_min: params[0].parse().unwrap_or(0.0),
                    range_max: params[1].parse().unwrap_or(0.0),
                    color_token: params.get(2).map(|s| s.to_string()),
                    transparency: params.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    symbol: None,
                    text: None,
                });
            }
        }
        "CoverageColor" => {
            // Format: rangeMin,rangeMax,token[,transparency]
            if params.len() >= 3 {
                state.lookup_entries.push(LookupEntry {
                    range_min: params[0].parse().unwrap_or(0.0),
                    range_max: params[1].parse().unwrap_or(0.0),
                    color_token: Some(params[2].to_string()),
                    transparency: params.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    symbol: None,
                    text: None,
                });
            }
        }
        "NumericAnnotation" | "SymbolAnnotation" => {
            // Annotations add to the most recent lookup entry
            if let Some(entry) = state.lookup_entries.last_mut() {
                if cmd == "SymbolAnnotation" {
                    entry.symbol = params.first().map(|s| s.to_string());
                } else {
                    entry.text = params.first().map(|s| def_decode(s));
                }
            }
        }

        // ════════════════════════════════════════════
        // Time State Commands (9a-11.2.2.7)
        // ════════════════════════════════════════════
        "Date" => {
            state.date = Some(def_decode(value));
        }
        "Time" => {
            state.time = Some(def_decode(value));
        }
        "DateTime" => {
            state.date_time = Some(def_decode(value));
        }
        "TimeValid" => {
            // Format: dateTimeStart,dateTimeEnd
            state.time_valid = if params.len() >= 2 {
                Some((def_decode(params[0]), def_decode(params[1])))
            } else {
                None
            };
        }
        "ClearTime" => {
            state.date = None;
            state.time = None;
            state.date_time = None;
            state.time_valid = None;
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
                local_offset: state.local_offset,
                scale_factor: state.scale_factor,
                color_overrides: state.take_color_overrides(),
                override_all: state.take_override_all(),
                line_placement,
                spatial_refs: state.take_spatial_refs(),
                visibility: state.visibility_snapshot(),
            });
        }
        "LineInstruction" => {
            let style_refs: Vec<String> = params.iter().map(|s| s.to_string()).collect();
            result.commands.push(DrawingCommand::LineInstruction {
                style_refs,
                simple_style: None,
                spatial_refs: state.take_spatial_refs(),
                augmented_segments: state.take_augmented_segments(),
                augmented_ray: state.augmented_ray.take(),
                visibility: state.visibility_snapshot(),
            });
        }
        "LineInstructionUnsuppressed" => {
            let style_refs: Vec<String> = params.iter().map(|s| s.to_string()).collect();
            result
                .commands
                .push(DrawingCommand::LineInstructionUnsuppressed {
                    style_refs,
                    simple_style: None,
                    spatial_refs: state.take_spatial_refs(),
                    augmented_segments: state.take_augmented_segments(),
                    augmented_ray: state.augmented_ray.take(),
                    visibility: state.visibility_snapshot(),
                });
        }
        "ColorFill" => {
            let token = params.first().unwrap_or(&"").to_string();
            let transparency = params.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
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
            result.commands.push(DrawingCommand::HatchFill {
                direction,
                distance,
                line_styles,
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
    fn test_parse_simple() {
        let result =
            parse_instruction_string("F123", "ViewingGroup:21010;DrawingPriority:15").unwrap();

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

        // Should have Dash + LineInstruction (from LineStyle _simple_) + LineInstruction
        assert!(result.commands.len() >= 2);
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
