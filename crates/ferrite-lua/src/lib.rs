//! Lua Integration for S-100 Portrayal Rules
//!
//! This crate provides Lua scripting support for executing S-100 portrayal rules.
//! Based on S-100 standard's lua_session and host_functions implementation.
//!
//! ## Architecture
//!
//! ```text
//! S101Cell → PortrayalContext → LuaSession
//!                ↓                   ↓
//!           Host Functions ←→ Lua Scripts (main.lua, *.lua)
//!                ↓
//!        DrawingInstructions (parsed)
//! ```
//!
//! ## Host Functions
//!
//! All required S-100 standard host functions are implemented:
//!
//! - **Feature functions**: HostGetFeatureIDs, HostFeatureGetCode, HostFeatureGetSimpleAttribute,
//!   HostFeatureGetComplexAttributeCount, HostFeatureGetSpatialAssociations,
//!   HostFeatureGetAssociatedFeatureIDs, HostFeatureGetAssociatedInformationIDs
//!
//! - **Information type functions**: HostInformationTypeGetCode, HostInformationTypeGetSimpleAttribute,
//!   HostInformationTypeGetComplexAttributeCount
//!
//! - **Spatial functions**: HostGetSpatial, HostSpatialGetAssociatedFeatureIDs,
//!   HostSpatialGetAssociatedInformationIDs
//!
//! - **Type catalogue functions**: HostGetFeatureTypeCodes, HostGetInformationTypeCodes,
//!   HostGetSimpleAttributeTypeCodes, HostGetComplexAttributeTypeCodes, HostGetRoleTypeCodes,
//!   HostGetInformationAssociationTypeCodes, HostGetFeatureAssociationTypeCodes,
//!   HostGetFeatureTypeInfo, HostGetInformationTypeInfo, HostGetSimpleAttributeTypeInfo,
//!   HostGetComplexAttributeTypeInfo
//!
//! - **Output/debug**: HostPortrayalEmit, HostDebuggerEntry, HostGetContextParameter

mod error;
mod session;
mod host;
mod instruction;
mod context;

pub use error::*;
pub use session::*;
pub use host::*;
pub use instruction::*;
pub use context::*;
