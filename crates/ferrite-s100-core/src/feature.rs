//! Feature record types for S-100
//!
//! Contains feature records with attributes and associations.

use crate::{RecordId, SpatialPrimitiveType};

/// Feature Record Identifier (FRID)
#[derive(Debug, Clone)]
pub struct FRID {
    /// Record identifier
    pub rcid: u32,
    /// Numeric feature type code
    pub nftc: u16,
    /// Record version
    pub rver: u16,
    /// Record update instruction
    pub ruin: u8,
}

/// Feature Object Identifier (FOID)
#[derive(Debug, Clone)]
pub struct FOID {
    /// Producing agency code
    pub agen: u16,
    /// Feature identification number
    pub fidn: u32,
    /// Feature identification subdivision
    pub fids: u16,
}

/// Attribute value types
#[derive(Debug, Clone)]
pub enum AttributeValue {
    Text(String),
    Integer(i64),
    Real(f64),
    Boolean(bool),
    Enumeration(u32, String), // code, label
    Date(String),
    Time(String),
    DateTime(String),
    List(Vec<AttributeValue>),
}

impl std::fmt::Display for AttributeValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttributeValue::Text(s) => write!(f, "{}", s),
            AttributeValue::Integer(i) => write!(f, "{}", i),
            AttributeValue::Real(r) => write!(f, "{}", r),
            AttributeValue::Boolean(b) => write!(f, "{}", b),
            AttributeValue::Enumeration(code, label) => write!(f, "{}({})", label, code),
            AttributeValue::Date(d) => write!(f, "{}", d),
            AttributeValue::Time(t) => write!(f, "{}", t),
            AttributeValue::DateTime(dt) => write!(f, "{}", dt),
            AttributeValue::List(list) => {
                let items: Vec<String> = list.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", items.join(", "))
            }
        }
    }
}

/// Single attribute
#[derive(Debug, Clone)]
pub struct Attribute {
    /// Numeric attribute code
    pub natc: u16,
    /// Attribute index (position in list)
    pub atix: u16,
    /// Parent attribute index (0 = root)
    pub paix: u16,
    /// Attribute value (raw text)
    pub atvl: String,
    /// Resolved value (after FC binding)
    pub value: Option<AttributeValue>,
    /// Attribute code string (after mapping)
    pub code: Option<String>,
}

impl Attribute {
    /// Check if this is a root attribute
    pub fn is_root(&self) -> bool {
        self.paix == 0
    }
}

/// Spatial association type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialAssociationType {
    /// No topology
    None = 0,
    /// Begin point of edge
    Begin = 1,
    /// End point of edge
    End = 2,
    /// Left face
    Left = 3,
    /// Right face
    Right = 4,
    /// Skin of face
    Skin = 5,
}

/// Spatial association
#[derive(Debug, Clone)]
pub struct SpatialAssociation {
    /// Referenced spatial record
    pub spatial_id: RecordId,
    /// Orientation (forward/reverse)
    pub ornt: i8,
    /// Usage indicator
    pub usag: u8,
    /// Mask pointer
    pub mask: u8,
}

/// Information association
#[derive(Debug, Clone)]
pub struct InformationAssociation {
    /// Numeric information association code
    pub niac: u16,
    /// Numeric association role code
    pub narc: u16,
    /// Referenced information record
    pub info_id: RecordId,
}

/// Feature association
#[derive(Debug, Clone)]
pub struct FeatureAssociation {
    /// Numeric feature association code
    pub nfac: u16,
    /// Numeric association role code
    pub narc: u16,
    /// Referenced feature record
    pub feature_id: RecordId,
}

/// Mask record
#[derive(Debug, Clone)]
pub struct MaskRecord {
    /// Mask type
    pub mask_type: u8,
    /// Referenced spatial
    pub spatial_id: RecordId,
}

/// Complete feature record
#[derive(Debug, Clone)]
pub struct FeatureRecord {
    pub frid: FRID,
    pub foid: Option<FOID>,
    pub attributes: Vec<Attribute>,
    pub spatial_associations: Vec<SpatialAssociation>,
    pub information_associations: Vec<InformationAssociation>,
    pub feature_associations: Vec<FeatureAssociation>,
    pub masks: Vec<MaskRecord>,
    /// Feature type code string (after mapping)
    pub feature_code: Option<String>,
    /// Primitive type (resolved from spatial associations)
    pub primitive_type: SpatialPrimitiveType,
}

impl FeatureRecord {
    /// Get record ID
    pub fn record_id(&self) -> RecordId {
        RecordId::new(100, self.frid.rcid) // 100 = feature record name
    }

    /// Get attribute by code
    pub fn get_attribute(&self, code: &str) -> Option<&Attribute> {
        self.attributes
            .iter()
            .find(|a| a.code.as_deref() == Some(code))
    }

    /// Get all root attributes
    pub fn root_attributes(&self) -> Vec<&Attribute> {
        self.attributes.iter().filter(|a| a.is_root()).collect()
    }
}
