//! Error types for Lua integration

use thiserror::Error;

#[derive(Error, Debug)]
pub enum LuaError {
    #[error("Lua runtime error: {0}")]
    Runtime(#[from] mlua::Error),

    #[error("Script not found: {0}")]
    ScriptNotFound(String),

    #[error("Function not found: {0}")]
    FunctionNotFound(String),

    #[error("Invalid instruction format: {0}")]
    InvalidInstruction(String),

    #[error("Feature not found: {0}")]
    FeatureNotFound(i64),

    #[error("Attribute not found: {0}")]
    AttributeNotFound(String),

    #[error("Portrayal error: {0}")]
    Portrayal(String),
}

pub type Result<T> = std::result::Result<T, LuaError>;
