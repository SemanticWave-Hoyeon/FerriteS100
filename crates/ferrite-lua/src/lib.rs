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

mod context;
mod error;
mod host;
mod instruction;
mod resource_limits;
mod session;
pub use resource_limits::LuaResourceLimits;

pub use context::*;
pub use error::*;
pub use host::*;
pub use instruction::*;
pub use session::*;

// Re-export runtime types for product adapters without a second Lua version.
pub use ferrite_lua_runtime::{mlua, selected_version};
pub use ferrite_lua_runtime::{selected_version as lua_runtime_version, RuntimeVersion};

// Exact linked interpreter release plus conservative build-input fingerprint.
pub use ferrite_lua_runtime::{runtime_identity, RuntimeIdentity};

// Compiler retention policy can be injected without changing bound PC inputs.
pub use ferrite_lua_runtime::ChunkCache;
