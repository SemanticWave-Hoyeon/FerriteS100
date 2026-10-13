//! Portrayal rules definitions

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Rule type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RuleType {
    #[default]
    Lua,
    Xslt,
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
    /// `ruleType` is `TopLevelTemplate` (S-100 Part 9 entry point).
    #[serde(default)]
    pub top_level: bool,
}

impl RuleType {
    /// Map a catalogue `fileFormat` value (`XSLT`, `LUA`) or a file extension
    /// (`xsl`, `xslt`, `lua`) to a rule language.
    pub fn from_format(format: &str) -> Option<Self> {
        match format.trim().to_ascii_lowercase().as_str() {
            "xslt" | "xsl" => Some(RuleType::Xslt),
            "lua" => Some(RuleType::Lua),
            _ => None,
        }
    }
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
    #[serde(default)]
    pub enable: Option<String>,
    #[serde(default)]
    pub constraints: Vec<ContextEnumeration>,
    #[serde(default)]
    pub validations: Vec<ContextValidation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextText {
    pub text: String,
    pub language: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEnumeration {
    pub value: String,
    pub labels: Vec<ContextText>,
    pub icon: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContextExpression {
    XPath(String),
    Regex(String),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextValidation {
    pub enable: Option<String>,
    pub expression: ContextExpression,
    pub errors: Vec<ContextText>,
    pub icon: Option<String>,
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
        self.rule_files
            .get(id)
            .map(|r| self.base_path.join(&r.file_path))
    }

    /// Language of the portrayal rules: the declared format of the top-level
    /// rule file, else inferred from the main file extension.
    pub fn rule_type(&self) -> Option<RuleType> {
        self.rule_files
            .values()
            .find(|r| r.top_level)
            .map(|r| r.rule_type)
            .or_else(|| {
                self.main_file
                    .as_ref()
                    .and_then(|p| p.extension())
                    .and_then(|e| RuleType::from_format(&e.to_string_lossy()))
            })
    }

    /// The top-level rule file entry (e.g. `main.xsl` for S-124/S-129).
    pub fn main_rule(&self) -> Option<&RuleFile> {
        let main = self.main_file.as_ref()?;
        self.rule_files
            .values()
            .filter(|r| &r.file_path == main)
            .min_by_key(|r| !r.top_level)
    }

    /// Get context parameter by ID
    pub fn get_context_param(&self, id: &str) -> Option<&ContextParameter> {
        self.context_parameters.get(id)
    }
}

fn named_children<'a, 'i>(
    node: roxmltree::Node<'a, 'i>,
    name: &'static str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'i>> {
    node.children()
        .filter(move |n| n.is_element() && n.tag_name().name() == name)
}

fn child_text(node: roxmltree::Node<'_, '_>, name: &'static str) -> Option<String> {
    named_children(node, name)
        .next()
        .and_then(|n| n.text())
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

/// Read the catalogue `<rules>` section (S-100 Part 9 `ruleFile` entries).
/// Declarations are additive: entries without a usable file name or with a
/// path that escapes the Rules directory are ignored.
pub(crate) fn read_declared_rule_files(doc: &roxmltree::Document<'_>) -> Vec<RuleFile> {
    let mut out = Vec::new();
    for rules in named_children(doc.root_element(), "rules") {
        for rf in named_children(rules, "ruleFile") {
            let Some(file_name) = child_text(rf, "fileName") else {
                continue;
            };
            let path = PathBuf::from(&file_name);
            if path.is_absolute()
                || path
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                continue;
            }
            let id = rf
                .attribute("id")
                .map(str::to_owned)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    path.file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
            let rule_type = child_text(rf, "fileFormat")
                .and_then(|f| RuleType::from_format(&f))
                .or_else(|| {
                    path.extension()
                        .and_then(|e| RuleType::from_format(&e.to_string_lossy()))
                })
                .unwrap_or_default();
            let top_level = child_text(rf, "ruleType").as_deref() == Some("TopLevelTemplate");
            let description = named_children(rf, "description")
                .next()
                .and_then(|d| child_text(d, "description").or_else(|| child_text(d, "name")));
            out.push(RuleFile {
                id,
                file_path: path,
                rule_type,
                description,
                top_level,
            });
        }
    }
    out
}
