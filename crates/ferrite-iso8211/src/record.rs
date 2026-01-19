//! ISO 8211 Record types
//!
//! Contains DDR (Data Descriptive Record) and DR (Data Record) parsing.

use crate::{Leader, Directory, RawField, Iso8211Error, Result};

/// Data Descriptive Record (DDR)
/// Contains field definitions for the file
#[derive(Debug, Clone)]
pub struct DDR {
    pub leader: Leader,
    pub directory: Directory,
    pub field_control: Option<FieldControlField>,
    pub field_definitions: Vec<DataDescriptiveField>,
}

/// Field control field from DDR
#[derive(Debug, Clone)]
pub struct FieldControlField {
    pub data_structure: char,
    pub data_type: char,
}

/// Data descriptive field definition
#[derive(Debug, Clone)]
pub struct DataDescriptiveField {
    pub tag: String,
    pub field_controls: String,
    pub field_name: String,
    pub array_descriptor: String,
    pub format_controls: String,
}

impl DDR {
    /// Parse DDR from data
    pub fn parse(data: &[u8]) -> Result<Self> {
        let leader = Leader::parse(data)?;

        if !leader.is_ddr() {
            return Err(Iso8211Error::InvalidRecord(
                "Expected DDR (leader identifier 'L')".to_string()
            ));
        }

        // Parse directory (starts after leader)
        let dir_start = Leader::SIZE;
        let dir_end = leader.base_address as usize;
        let directory = Directory::parse(&data[dir_start..dir_end], &leader)?;

        // Parse field control field (first entry "0000")
        let mut field_control = None;
        let mut field_definitions = Vec::new();

        for entry in &directory.entries {
            let field_start = leader.base_address as usize + entry.position as usize;
            let field_end = field_start + entry.length as usize;

            if field_end > data.len() {
                return Err(Iso8211Error::UnexpectedEof);
            }

            let field_data = &data[field_start..field_end];

            if entry.tag == "0000" {
                // Field control field
                if field_data.len() >= 2 {
                    field_control = Some(FieldControlField {
                        data_structure: field_data[0] as char,
                        data_type: field_data[1] as char,
                    });
                }
            } else {
                // Data descriptive field
                let def = parse_ddf(entry.tag.clone(), field_data)?;
                field_definitions.push(def);
            }
        }

        Ok(DDR {
            leader,
            directory,
            field_control,
            field_definitions,
        })
    }

    /// Find field definition by tag
    pub fn find_field_def(&self, tag: &str) -> Option<&DataDescriptiveField> {
        self.field_definitions.iter().find(|f| f.tag == tag)
    }
}

/// Parse Data Descriptive Field
fn parse_ddf(tag: String, data: &[u8]) -> Result<DataDescriptiveField> {
    // Format: field_controls ^ field_name ^ array_descriptor ^ format_controls
    // ^ = unit terminator (0x1F)

    let parts: Vec<&[u8]> = data.split(|&b| b == 0x1F).collect();

    let field_controls = if !parts.is_empty() {
        String::from_utf8_lossy(parts[0]).to_string()
    } else {
        String::new()
    };

    let field_name = if parts.len() > 1 {
        String::from_utf8_lossy(parts[1]).to_string()
    } else {
        String::new()
    };

    let array_descriptor = if parts.len() > 2 {
        String::from_utf8_lossy(parts[2]).to_string()
    } else {
        String::new()
    };

    let format_controls = if parts.len() > 3 {
        // Remove field terminator
        let fmt = parts[3];
        let end = fmt.iter().position(|&b| b == 0x1E).unwrap_or(fmt.len());
        String::from_utf8_lossy(&fmt[..end]).to_string()
    } else {
        String::new()
    };

    Ok(DataDescriptiveField {
        tag,
        field_controls,
        field_name,
        array_descriptor,
        format_controls,
    })
}

/// Data Record (DR)
/// Contains actual data values
#[derive(Debug, Clone)]
pub struct DR {
    pub leader: Leader,
    pub directory: Directory,
    pub fields: Vec<RawField>,
}

impl DR {
    /// Parse DR from data
    pub fn parse(data: &[u8]) -> Result<Self> {
        let leader = Leader::parse(data)?;

        if !leader.is_dr() {
            return Err(Iso8211Error::InvalidRecord(
                "Expected DR (leader identifier 'D')".to_string()
            ));
        }

        // Parse directory (starts after leader)
        let dir_start = Leader::SIZE;
        let dir_end = leader.base_address as usize;
        let directory = Directory::parse(&data[dir_start..dir_end], &leader)?;

        // Parse fields
        let mut fields = Vec::new();

        for entry in &directory.entries {
            let field_start = leader.base_address as usize + entry.position as usize;
            let field_end = field_start + entry.length as usize;

            if field_end > data.len() {
                return Err(Iso8211Error::UnexpectedEof);
            }

            let field_data = data[field_start..field_end].to_vec();
            fields.push(RawField::new(entry.tag.clone(), field_data));
        }

        Ok(DR {
            leader,
            directory,
            fields,
        })
    }

    /// Find field by tag
    pub fn find_field(&self, tag: &str) -> Option<&RawField> {
        self.fields.iter().find(|f| f.tag == tag)
    }

    /// Find all fields with given tag
    pub fn find_fields(&self, tag: &str) -> Vec<&RawField> {
        self.fields.iter().filter(|f| f.tag == tag).collect()
    }
}
