//! A DSL definition: the shape of a DSL block of the YAML DSL editor's config.
//!
//! The fields read here mean what the editor's design doc says they mean. Fields
//! that only shape editing are not part of the type and are ignored when a block
//! holding them is read.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct DslDefinition {
    pub id: String,
    pub file_includes: Vec<String>,
    #[serde(default)]
    pub file_excludes: Vec<String>,
    #[serde(default)]
    pub schema_search_paths: Vec<String>,
    pub layers: Option<Layers>,
    pub placeholder: Option<Placeholder>,
    pub function: Option<FunctionSettings>,
    pub locals: Option<Locals>,
    pub loops: Option<Loops>,
    #[serde(default)]
    pub scopes: BTreeMap<String, Scope>,
    #[serde(default)]
    pub declarations: Vec<DeclarationRule>,
    #[serde(default)]
    pub references: Vec<ReferenceRule>,
    #[serde(default)]
    pub tag_keys: Vec<TagKey>,
    pub validator: Option<Validator>,
}

impl DslDefinition {
    /// Reads a DSL block, such as one from a parsed `yaml-dsl.yml`.
    pub fn from_json(block: &serde_json::Value) -> Result<Self, String> {
        serde_json::from_value(block.clone()).map_err(|error| error.to_string())
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Layers {
    #[serde(default)]
    pub overlay_folders: Vec<String>,
    pub common_layer_discovery: CommonLayerDiscovery,
    #[serde(default)]
    pub common_layer_files: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommonLayerDiscovery {
    Parent,
    Ancestor,
    OverlayIncludes,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Placeholder {
    pub pattern: String,
    #[serde(default)]
    pub unscanned_paths: Vec<PathEntry>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PathEntry {
    pub path: String,
    #[serde(default)]
    pub skip_keys: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FunctionSettings {
    #[serde(default)]
    pub definitions: Vec<String>,
    #[serde(default)]
    pub calls_not_allowed_at: Vec<PathEntry>,
    pub marker_function: Option<MarkerFunction>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MarkerFunction {
    pub call_marker: String,
    pub splat_operator: Option<String>,
    pub calls_without_name_allowed: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Locals {
    pub scope_name: String,
    pub path_separator: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Loops {
    pub scope_name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Scope {
    #[serde(default)]
    pub regions: Vec<String>,
    #[serde(default)]
    pub later_items_of_declaring_list: bool,
    #[serde(default)]
    pub named_by_parent_key: bool,
    #[serde(default)]
    pub builtin_names: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DeclarationRule {
    pub path: String,
    #[serde(default)]
    pub skip_keys: Vec<String>,
    #[serde(default)]
    pub exclude_candidates: Vec<String>,
    pub name_source: NameSource,
    pub key_token: Option<KeyToken>,
    pub name_spelling: NameSpelling,
    pub meta_argument_name: Option<String>,
    #[serde(default)]
    pub declares_every_name: bool,
    pub scope_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameSource {
    Key,
    Value,
    MetaArgument,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyToken {
    AllWords,
    FirstWord,
    LastWord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameSpelling {
    AsWritten,
    DashesAsUnderscores,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ReferenceRule {
    pub pattern: String,
    pub positions: Vec<ReferencePosition>,
    pub text_after_name_allowed: bool,
    pub scope_name: String,
    pub scope_group: Option<String>,
    pub name_group: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferencePosition {
    WholeScalar,
    WholePlaceholder,
    PlaceholderInText,
    AnywhereInScalar,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TagKey {
    pub tag: String,
    pub key: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Validator {
    #[serde(default)]
    pub child_values_from_parent_key: Vec<ChildValueRule>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChildValueRule {
    pub pattern: String,
}
