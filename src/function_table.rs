//! The functions and macros a schema publishes at `#/x-yaml-dsl-functions`, and
//! the calls written in keys.

use std::collections::BTreeMap;

use fancy_regex::Regex;
use serde_json::Value as JsonValue;

use crate::definition::MarkerFunction;

pub const FUNCTION_TABLE_POINTER: &str = "/x-yaml-dsl-functions";

#[derive(Clone, Debug, Default)]
pub struct FunctionTable {
    entries: BTreeMap<String, FunctionEntry>,
}

#[derive(Clone, Debug)]
pub struct FunctionEntry {
    pub arguments: Vec<Argument>,
    pub macro_entry: Option<MacroEntry>,
}

#[derive(Clone, Debug)]
pub struct Argument {
    pub name: String,
    pub repeated: bool,
    pub default: Option<JsonValue>,
}

#[derive(Clone, Debug)]
pub struct MacroEntry {
    pub kind: MacroKind,
    pub merge_behavior: MergeBehavior,
    pub name_suffix: Option<Regex>,
    roles: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacroKind {
    FunctionCall,
    Merge,
    Loop,
    Transform,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeBehavior {
    Shallow,
    Deep,
    Join,
}

const ROLE_NAMES: [&str; 6] = [
    "callee",
    "callee_arguments",
    "content",
    "loop_variables",
    "collection",
    "body",
];

impl MacroEntry {
    /// The name of the entry's argument that plays `role`.
    pub fn role(&self, role: &str) -> Option<&str> {
        self.roles.get(role).map(String::as_str)
    }
}

impl FunctionTable {
    pub fn from_schema(schema: &JsonValue) -> Result<Self, String> {
        let Some(table) = schema.pointer(FUNCTION_TABLE_POINTER) else {
            return Ok(Self::default());
        };
        let table = table.as_object().ok_or_else(|| {
            format!("{FUNCTION_TABLE_POINTER} is not a mapping of function names")
        })?;
        let mut entries = BTreeMap::new();
        for (name, entry) in table {
            entries.insert(name.clone(), function_entry(name, entry)?);
        }
        Ok(Self { entries })
    }

    /// The entry a call's function name calls, a suffixed macro name included.
    pub fn entry(&self, function_name: &str) -> Option<&FunctionEntry> {
        if let Some(entry) = self.entries.get(function_name)
            && entry
                .macro_entry
                .as_ref()
                .is_none_or(|macro_entry| macro_entry.name_suffix.is_none())
        {
            return Some(entry);
        }
        self.entries.iter().find_map(|(name, entry)| {
            let suffix_pattern = entry.macro_entry.as_ref()?.name_suffix.as_ref()?;
            let suffix = function_name.strip_prefix(name.as_str())?;
            suffix_pattern
                .is_match(suffix)
                .unwrap_or(false)
                .then_some(entry)
        })
    }
}

fn function_entry(name: &str, entry: &JsonValue) -> Result<FunctionEntry, String> {
    let arguments = entry
        .get("arguments")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| format!("function `{name}` has no arguments list"))?
        .iter()
        .map(|argument| {
            Ok(Argument {
                name: argument
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| format!("an argument of `{name}` has no name"))?
                    .to_owned(),
                repeated: argument
                    .get("repeated")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false),
                default: argument.get("default").cloned(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let macro_entry = match entry.get("macro") {
        None => None,
        Some(macro_value) => Some(macro_entry(name, macro_value)?),
    };
    Ok(FunctionEntry {
        arguments,
        macro_entry,
    })
}

fn macro_entry(name: &str, value: &JsonValue) -> Result<MacroEntry, String> {
    let kind = match value.get("kind").and_then(JsonValue::as_str) {
        Some("function_call") => MacroKind::FunctionCall,
        Some("merge") => MacroKind::Merge,
        Some("loop") => MacroKind::Loop,
        Some("transform") => MacroKind::Transform,
        other => return Err(format!("macro `{name}` has an unknown kind {other:?}")),
    };
    let merge_behavior = match value.get("merge_behavior").and_then(JsonValue::as_str) {
        Some("deep") => MergeBehavior::Deep,
        Some("join") => MergeBehavior::Join,
        Some("shallow") | None => MergeBehavior::Shallow,
        Some(other) => {
            return Err(format!(
                "macro `{name}` has an unknown merge_behavior `{other}`"
            ));
        }
    };
    let name_suffix = match value.get("name_suffix_pattern").and_then(JsonValue::as_str) {
        Some(pattern) => Some(
            Regex::new(&format!("^(?:{pattern})$"))
                .map_err(|error| format!("macro `{name}`: {error}"))?,
        ),
        None => None,
    };
    let roles = ROLE_NAMES
        .iter()
        .filter_map(|role| {
            value
                .get(*role)
                .and_then(JsonValue::as_str)
                .map(|argument| ((*role).to_owned(), argument.to_owned()))
        })
        .collect();
    Ok(MacroEntry {
        kind,
        merge_behavior,
        name_suffix,
        roles,
    })
}

/// A call written in a key: `<name> <marker><function>[splat]`, or the marker alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallKey {
    pub name: Option<String>,
    pub function: String,
    pub splat: bool,
}

pub fn parse_call_key(key: &str, marker: &MarkerFunction) -> Option<CallKey> {
    let trimmed = key.trim_end();
    let call_start = trimmed.rfind(' ').map_or(0, |space| space + 1);
    let call_text = &trimmed[call_start..];
    let rest = call_text.strip_prefix(marker.call_marker.as_str())?;
    let (function, splat) = match marker.splat_operator.as_deref() {
        Some(splat) if rest.ends_with(splat) => (&rest[..rest.len() - splat.len()], true),
        _ => (rest, false),
    };
    let valid = function
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && function.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | ':' | '-')
        });
    if !valid {
        return None;
    }
    let name = (call_start > 0).then(|| trimmed[..call_start].trim().to_owned());
    Some(CallKey {
        name,
        function: function.to_owned(),
        splat,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker() -> MarkerFunction {
        MarkerFunction {
            call_marker: "fn.".into(),
            splat_operator: Some("*".into()),
            calls_without_name_allowed: true,
        }
    }

    #[test]
    fn parses_named_and_nameless_calls() {
        assert_eq!(
            parse_call_key("tags fn.merge*", &marker()),
            Some(CallKey {
                name: Some("tags".into()),
                function: "merge".into(),
                splat: true
            })
        );
        assert_eq!(
            parse_call_key("fn.loop", &marker()),
            Some(CallKey {
                name: None,
                function: "loop".into(),
                splat: false
            })
        );
        assert_eq!(parse_call_key("plain key", &marker()), None);
    }

    #[test]
    fn reads_macro_roles_from_the_table() {
        let schema = serde_json::json!({
            "x-yaml-dsl-functions": {
                "loop": {
                    "arguments": [
                        {"name": "in", "repeated": false},
                        {"name": "body", "repeated": false},
                        {"name": "for", "repeated": false, "default": ["it"]}
                    ],
                    "macro": {
                        "kind": "loop", "loop_variables": "for", "collection": "in", "body": "body"
                    }
                }
            }
        });
        let table = FunctionTable::from_schema(&schema).unwrap();
        let entry = table.entry("loop").unwrap();
        let macro_entry = entry.macro_entry.as_ref().unwrap();
        assert_eq!(macro_entry.kind, MacroKind::Loop);
        assert_eq!(macro_entry.role("collection"), Some("in"));
        assert_eq!(entry.arguments[2].default, Some(serde_json::json!(["it"])));
    }
}
