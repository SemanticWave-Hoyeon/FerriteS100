//! Portrayal rules definitions

use std::path::PathBuf;
use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// Rule type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleType {
    Lua,
    Xslt,
}

impl Default for RuleType {
    fn default() -> Self {
        RuleType::Lua
    }
}

/// Rule file reference
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleFile {
    pub id: String,
    pub file_path: PathBuf,
    #[serde(default)]
    pub rule_type: RuleType,
    #[serde(default)]
    pub description: Option<String>,
}

/// Context parameter definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextParameter {
    pub id: String,
    pub param_type: ContextParamType,
    #[serde(default)]
    pub default_value: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Context parameter type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ContextParamType {
    #[default]
    Boolean,
    Integer,
    Double,
    String,
    Date,
    Enumeration,
}

/// Collection of portrayal rules
#[derive(Debug, Clone, Default)]
pub struct PortrayalRules {
    pub rule_files: HashMap<String, RuleFile>,
    pub main_file: Option<PathBuf>,
    pub context_parameters: HashMap<String, ContextParameter>,
    pub base_path: PathBuf,
}

impl PortrayalRules {
    pub fn new(base_path: PathBuf) -> Self {
        PortrayalRules {
            rule_files: HashMap::new(),
            main_file: None,
            context_parameters: HashMap::new(),
            base_path,
        }
    }

    /// Get rule file by ID
    pub fn get_rule(&self, id: &str) -> Option<&RuleFile> {
        self.rule_files.get(id)
    }

    /// Get full path to main rule file
    pub fn get_main_path(&self) -> Option<PathBuf> {
        self.main_file.as_ref().map(|p| self.base_path.join(p))
    }

    /// Get full path to rule file
    pub fn get_rule_path(&self, id: &str) -> Option<PathBuf> {
        self.rule_files.get(id).map(|r| self.base_path.join(&r.file_path))
    }

    /// Get context parameter by ID
    pub fn get_context_param(&self, id: &str) -> Option<&ContextParameter> {
        self.context_parameters.get(id)
    }
}
