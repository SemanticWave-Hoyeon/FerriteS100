//! Drawing Instruction Parser
//!
//! Parses the semicolon-delimited drawing instruction strings returned from Lua.
//! Format: "cmd1:val1;cmd2:val2,val3;..."
//!
//! Based on S-100 standard's LUA_ParsingDrawingInstructions()

use crate::Result;

/// Parsed drawing instruction from Lua output
#[derive(Debug, Clone)]
pub struct ParsedInstruction {
    pub feature_id: String,
    pub viewing_groups: Vec<u32>,
    pub drawing_priority: i32,
    pub display_plane: DisplayPlane,
    pub scale_minimum: Option<u32>,
    pub scale_maximum: Option<u32>,
    pub commands: Vec<DrawingCommand>,
}

/// Display plane for radar overlay
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayPlane {
    #[default]
    UnderRadar,
    OverRadar,
}

/// Individual drawing command
#[derive(Debug, Clone)]
pub enum DrawingCommand {
    /// Point symbol with optional explicit position (from AugmentedPoint)
    PointInstruction {
        symbol_ref: String,
        rotation: f32,
        scale: f32,
        /// Explicit position from AugmentedPoint (overrides feature spatial)
        position: Option<(f64, f64)>,
    },
    /// AugmentedPoint sets explicit coordinates for subsequent PointInstructions
    AugmentedPoint { crs: String, x: f64, y: f64 },
    /// Line style
    LineInstruction {
        style_ref: Option<String>,
        /// Inline simple line style: (width, color_token)
        simple_style: Option<(f32, String)>,
    },
    /// Area fill
    AreaInstruction {
        fill_ref: Option<String>,
        /// Inline solid color fill
        color_fill: Option<String>,
    },
    /// Text label
    TextInstruction {
        text: String,
        font_size: f32,
        color_token: String,
    },
    /// Dash pattern for lines
    Dash { start: f32, length: f32 },
    /// Spatial reference (geometry)
    SpatialReference { spatial_id: String, forward: bool },
}

impl ParsedInstruction {
    pub fn new(feature_id: String) -> Self {
        ParsedInstruction {
            feature_id,
            viewing_groups: vec![21010], // Default: DISPLBASE
            drawing_priority: 5,
            display_plane: DisplayPlane::UnderRadar,
            scale_minimum: None,
            scale_maximum: None,
            commands: Vec::new(),
        }
    }
}

/// Parse drawing instruction string from Lua
///
/// Format: "ViewingGroup:21010;DrawingPriority:15;PointInstruction:LIGHTS01"
pub fn parse_instruction_string(
    feature_id: &str,
    instruction_str: &str,
) -> Result<ParsedInstruction> {
    let mut result = ParsedInstruction::new(feature_id.to_string());

    // Track the last AugmentedPoint position for Sounding symbols
    let mut current_augmented_point: Option<(f64, f64)> = None;
    // Track current rotation state (set by Rotation command)
    let mut current_rotation: f32 = 0.0;

    // Split by semicolon and process each command
    for part in instruction_str.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        // Split command:value(s)
        if let Some((cmd, value)) = part.split_once(':') {
            parse_command(
                &mut result,
                cmd.trim(),
                value.trim(),
                &mut current_augmented_point,
                &mut current_rotation,
            )?;
        }
    }

    Ok(result)
}

/// Parse a single command
fn parse_command(
    result: &mut ParsedInstruction,
    cmd: &str,
    value: &str,
    current_augmented_point: &mut Option<(f64, f64)>,
    current_rotation: &mut f32,
) -> Result<()> {
    match cmd {
        "Rotation" => {
            // Format: CRS,angle (e.g., "PortrayalCRS,135" or "GeographicCRS,45")
            let parts: Vec<&str> = value.split(',').collect();
            if parts.len() >= 2 {
                if let Ok(angle) = parts[1].parse::<f32>() {
                    *current_rotation = angle;
                }
            }
        }
        "ViewingGroup" => {
            // Can have multiple comma-separated values
            result.viewing_groups.clear();
            for v in value.split(',') {
                if let Ok(vg) = v.trim().parse::<u32>() {
                    result.viewing_groups.push(vg);
                }
            }
        }
        "DrawingPriority" => {
            if let Ok(priority) = value.parse::<i32>() {
                result.drawing_priority = priority;
            }
        }
        "DisplayPlane" => {
            result.display_plane = match value {
                "OverRadar" => DisplayPlane::OverRadar,
                _ => DisplayPlane::UnderRadar,
            };
        }
        "ScaleMinimum" => {
            result.scale_minimum = value.parse().ok();
        }
        "ScaleMaximum" => {
            result.scale_maximum = value.parse().ok();
        }
        "AugmentedPoint" => {
            // Format: CRS,x,y (e.g., "GeographicCRS,127.5,-34.2")
            // Sets position for subsequent PointInstructions
            let parts: Vec<&str> = value.split(',').collect();
            if parts.len() >= 3 {
                let crs = parts[0].to_string();
                let x = parts[1].parse::<f64>().unwrap_or(0.0);
                let y = parts[2].parse::<f64>().unwrap_or(0.0);
                *current_augmented_point = Some((x, y));
                result
                    .commands
                    .push(DrawingCommand::AugmentedPoint { crs, x, y });
            }
        }
        "PointInstruction" => {
            // Format: symbol_ref or symbol_ref,rotation,scale
            let parts: Vec<&str> = value.split(',').collect();
            let symbol_ref = parts.first().unwrap_or(&"").to_string();
            // Use inline rotation if specified, otherwise use current_rotation from Rotation command
            let rotation = parts
                .get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(*current_rotation);
            let scale = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(1.0);

            // Use the current augmented point position if available
            result.commands.push(DrawingCommand::PointInstruction {
                symbol_ref,
                rotation,
                scale,
                position: *current_augmented_point,
            });
        }
        "LineInstruction" => {
            if value == "_simple_" {
                // Simple line uses previous LineStyle command
                result.commands.push(DrawingCommand::LineInstruction {
                    style_ref: None,
                    simple_style: None,
                });
            } else {
                result.commands.push(DrawingCommand::LineInstruction {
                    style_ref: Some(value.to_string()),
                    simple_style: None,
                });
            }
        }
        "LineStyle" => {
            // Format: style_ref or _simple_,dash_offset,width,color
            let parts: Vec<&str> = value.split(',').collect();
            if parts.first() == Some(&"_simple_") {
                let width = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.32);
                let color = parts.get(3).unwrap_or(&"CSTLN").to_string();
                result.commands.push(DrawingCommand::LineInstruction {
                    style_ref: None,
                    simple_style: Some((width, color)),
                });
            } else {
                result.commands.push(DrawingCommand::LineInstruction {
                    style_ref: Some(value.to_string()),
                    simple_style: None,
                });
            }
        }
        "AreaInstruction" | "AreaFill" => {
            result.commands.push(DrawingCommand::AreaInstruction {
                fill_ref: Some(value.to_string()),
                color_fill: None,
            });
        }
        "ColorFill" => {
            result.commands.push(DrawingCommand::AreaInstruction {
                fill_ref: None,
                color_fill: Some(value.to_string()),
            });
        }
        "TextInstruction" => {
            result.commands.push(DrawingCommand::TextInstruction {
                text: value.to_string(),
                font_size: 10.0,
                color_token: "CHBLK".to_string(),
            });
        }
        "Dash" => {
            // Format: start,length
            let parts: Vec<&str> = value.split(',').collect();
            let start = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let length = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(3.6);
            result.commands.push(DrawingCommand::Dash { start, length });
        }
        "SpatialReference" => {
            // Format: spatial_id or spatial_id,false (for reverse)
            let parts: Vec<&str> = value.split(',').collect();
            let spatial_id = parts.first().unwrap_or(&"").to_string();
            let forward = parts.get(1).is_none_or(|s| *s != "false");
            result.commands.push(DrawingCommand::SpatialReference {
                spatial_id,
                forward,
            });
        }
        "ClearGeometry" => {
            // Clears current augmented point - used after processing a sounding set
            *current_augmented_point = None;
        }
        "AlertReference" => {
            // Alert references (NavHazard, etc.) - currently ignored for rendering
            tracing::trace!("AlertReference: {}", value);
        }
        _ => {
            tracing::trace!("Unknown instruction command: {}:{}", cmd, value);
        }
    }

    Ok(())
}

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

        // Parse the main instruction string
        let instruction = parse_instruction_string(feature_id, drawing_instructions)?;
        result.instructions.push(instruction);

        // Parse observed parameters
        for param in observed_params.split(',') {
            let param = param.trim();
            if !param.is_empty() {
                result.observed_parameters.push(param.to_string());
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple() {
        let result =
            parse_instruction_string("F123", "ViewingGroup:21010;DrawingPriority:15").unwrap();

        assert_eq!(result.feature_id, "F123");
        assert_eq!(result.viewing_groups, vec![21010]);
        assert_eq!(result.drawing_priority, 15);
    }

    #[test]
    fn test_parse_point() {
        let result = parse_instruction_string(
            "F123",
            "ViewingGroup:23010;DrawingPriority:18;PointInstruction:LIGHTS01",
        )
        .unwrap();

        assert_eq!(result.commands.len(), 1);
        match &result.commands[0] {
            DrawingCommand::PointInstruction { symbol_ref, .. } => {
                assert_eq!(symbol_ref, "LIGHTS01");
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

        assert!(result.commands.len() >= 2);
    }
}
