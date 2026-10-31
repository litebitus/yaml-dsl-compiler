//! Finds a document's schema, reads its function table, and validates the
//! resolved document against it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use fancy_regex::Regex;
use jsonschema::error::ValidationErrorKind;
use serde_json::{Map, Value as JsonValue};

use crate::definition::DslDefinition;
use crate::diagnostic::Diagnostic;
use crate::function_table::FunctionTable;
use crate::node::number_json;
use crate::position::Position;
use crate::resolved::{Value, ValueKind};
use crate::source::FileSource;

const MODELINE_MARKER: &str = "yaml-language-server: $schema=";
const CONFIG_ROOT: &str = "{config_root}";

/// Where a document's schema is, and whether the document is a fragment of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaLocation {
    pub path: PathBuf,
    /// The modeline's fragment: a document that has one is a fragment, never a full document.
    pub fragment: Option<String>,
}

pub struct LoadedSchema {
    pub table: FunctionTable,
    validator: jsonschema::Validator,
}

/// A schema loaded once, by the first unit that reads it, or why it did not load.
type SchemaLoad = Arc<OnceLock<Result<Arc<LoadedSchema>, String>>>;

/// Schemas loaded once and shared by every unit that reads them.
#[derive(Default)]
pub struct SchemaCache {
    loaded: Mutex<BTreeMap<PathBuf, SchemaLoad>>,
}

impl SchemaCache {
    pub fn load(&self, path: &Path, source: &dyn FileSource) -> Result<Arc<LoadedSchema>, String> {
        let cell = {
            let mut loaded = self
                .loaded
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(loaded.entry(path.to_path_buf()).or_default())
        };
        cell.get_or_init(|| LoadedSchema::read(path, source).map(Arc::new))
            .clone()
    }
}

/// The schema of the document at `document_path`: its modeline first, then the
/// definition's search paths.
pub fn schema_location(
    definition: &DslDefinition,
    config_root: &Path,
    document_path: &Path,
    document_text: &str,
    source: &dyn FileSource,
) -> Result<SchemaLocation, Diagnostic> {
    let file: Arc<str> = Arc::from(document_path.to_string_lossy().as_ref());
    let directory = document_path.parent().unwrap_or(config_root);
    let modeline = document_text
        .lines()
        .enumerate()
        .find_map(|(index, line)| modeline_uri(line).map(|uri| (index + 1, uri)));
    if let Some((line, uri)) = &modeline {
        if uri.contains("://") {
            return Err(Diagnostic::at(
                &Position::new(&file, *line, 1),
                format!("schema URL `{uri}` is not read by the compiler"),
            ));
        }
        let (path, fragment) = match uri.split_once('#') {
            Some((path, fragment)) => (path, Some(fragment.to_owned())),
            None => (uri.as_str(), None),
        };
        let path = directory.join(path.split('?').next().unwrap_or(path));
        if source.exists(&path) {
            return Ok(SchemaLocation { path, fragment });
        }
    }
    for entry in &definition.schema_search_paths {
        let path = match entry.strip_prefix(CONFIG_ROOT) {
            Some(rest) => config_root.join(rest.trim_start_matches('/')),
            None => directory.join(entry),
        };
        if source.exists(&path) {
            return Ok(SchemaLocation {
                path,
                fragment: None,
            });
        }
    }
    let line = modeline.map_or(1, |(line, _)| line);
    Err(Diagnostic::at(
        &Position::new(&file, line, 1),
        "no schema found: neither the modeline nor schema_search_paths names a file on disk",
    ))
}

fn modeline_uri(line: &str) -> Option<String> {
    let comment = line.trim_start().strip_prefix('#')?;
    let uri = comment.trim_start().strip_prefix(MODELINE_MARKER)?;
    Some(uri.trim().to_owned())
}

impl LoadedSchema {
    fn read(path: &Path, source: &dyn FileSource) -> Result<Self, String> {
        let text = source
            .read(path)
            .map_err(|error| format!("reading schema {}: {error}", path.display()))?;
        let schema: JsonValue = serde_json::from_str(&text)
            .map_err(|error| format!("schema {}: {error}", path.display()))?;
        let table = FunctionTable::from_schema(&schema)
            .map_err(|error| format!("schema {}: {error}", path.display()))?;
        let validator = jsonschema::validator_for(&schema)
            .map_err(|error| format!("schema {}: {error}", path.display()))?;
        Ok(Self { table, validator })
    }

    /// Validates `document`. A value not known when the document compiles, such as
    /// a call or a reference to a declaration, is not checked.
    pub fn validate(&self, document: &Value, child_value_patterns: &[Regex]) -> Vec<Diagnostic> {
        let mut instance = Instance {
            child_value_patterns,
            positions: BTreeMap::new(),
            unknown: BTreeSet::new(),
        };
        let json = instance.json(document, String::new(), &document.position);
        let mut diagnostics: Vec<Diagnostic> = self
            .validator
            .iter_errors(&json)
            .filter(|error| !instance.reads_unknown(error.instance_path().as_str(), error.kind()))
            .map(|error| {
                let pointer = error.instance_path().as_str().to_owned();
                Diagnostic::at(
                    instance.position(&pointer),
                    format!("{}: {error}", display_pointer(&pointer)),
                )
            })
            .collect();
        diagnostics.dedup();
        diagnostics
    }
}

struct Instance<'p> {
    child_value_patterns: &'p [Regex],
    /// The position of the key holding each value, by JSON pointer.
    positions: BTreeMap<String, Position>,
    unknown: BTreeSet<String>,
}

impl Instance<'_> {
    fn json(&mut self, value: &Value, pointer: String, key_position: &Position) -> JsonValue {
        self.positions.insert(pointer.clone(), key_position.clone());
        match &value.local_value().kind {
            ValueKind::Null => JsonValue::Null,
            ValueKind::Bool(flag) => JsonValue::Bool(*flag),
            ValueKind::Number(text) => number_json(text),
            ValueKind::String(text) => JsonValue::String(text.clone()),
            ValueKind::List(items) => JsonValue::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        self.json(item, format!("{pointer}/{index}"), &item.position)
                    })
                    .collect(),
            ),
            ValueKind::Map(entries) => {
                let mut object = Map::new();
                for entry in entries {
                    let child = format!("{pointer}/{}", escape_pointer(&entry.key));
                    let mut json = self.json(&entry.value, child, &entry.key_position);
                    if let JsonValue::Object(fields) = &mut json {
                        self.set_child_value(&entry.key, fields);
                    }
                    object.insert(entry.key.clone(), json);
                }
                JsonValue::Object(object)
            }
            ValueKind::Reference(_) | ValueKind::Call(_) | ValueKind::Template(_) => {
                self.unknown.insert(pointer);
                JsonValue::Null
            }
        }
    }

    /// A key such as `name mock-orders` sets the field `name` of the entry it keys,
    /// unless the entry writes the field itself.
    fn set_child_value(&self, key: &str, fields: &mut Map<String, JsonValue>) {
        if let Some((field, value)) = child_value(key, self.child_value_patterns) {
            fields.entry(field).or_insert(JsonValue::String(value));
        }
    }

    /// Whether an error at `pointer` comes from a value not known when the document
    /// compiles: the value itself, or a check that reads a value below it through a
    /// subschema, or compares the values below it.
    fn reads_unknown(&self, pointer: &str, kind: &ValidationErrorKind) -> bool {
        if self.is_unknown(pointer) {
            return true;
        }
        let reads_below = matches!(
            kind,
            ValidationErrorKind::AnyOf { .. }
                | ValidationErrorKind::OneOfNotValid { .. }
                | ValidationErrorKind::OneOfMultipleValid { .. }
                | ValidationErrorKind::Not { .. }
                | ValidationErrorKind::Contains
                | ValidationErrorKind::UnevaluatedItems { .. }
                | ValidationErrorKind::UnevaluatedProperties { .. }
                | ValidationErrorKind::UniqueItems
        );
        let below = format!("{pointer}/");
        reads_below
            && self
                .unknown
                .iter()
                .any(|unknown| unknown.starts_with(&below))
    }

    fn is_unknown(&self, pointer: &str) -> bool {
        self.unknown
            .iter()
            .any(|unknown| pointer == unknown || pointer.starts_with(&format!("{unknown}/")))
    }

    fn position(&self, pointer: &str) -> &Position {
        let mut current = pointer;
        loop {
            if let Some(position) = self.positions.get(current) {
                return position;
            }
            current = current.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }
}

/// The field a key such as `name mock-orders` sets on the entry it keys, and the
/// value it sets, by the first of `patterns` with `field` and `value` groups that
/// matches.
pub fn child_value(key: &str, patterns: &[Regex]) -> Option<(String, String)> {
    patterns.iter().find_map(|pattern| {
        let captures = pattern.captures(key).ok()??;
        Some((
            captures.name("field")?.as_str().to_owned(),
            captures.name("value")?.as_str().to_owned(),
        ))
    })
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn display_pointer(pointer: &str) -> String {
    if pointer.is_empty() {
        return "the document".to_owned();
    }
    let steps: Vec<String> = pointer[1..]
        .split('/')
        .map(|step| step.replace("~1", "/").replace("~0", "~"))
        .collect();
    format!("`{}`", steps.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolved::{Call, MapEntry};
    use crate::source::MemorySource;

    fn position() -> Position {
        Position::file_start(&Arc::from("mock.yml"))
    }

    fn map(entries: Vec<(&str, Value)>) -> Value {
        Value::new(
            ValueKind::Map(
                entries
                    .into_iter()
                    .map(|(key, value)| MapEntry {
                        key: key.to_owned(),
                        key_position: position(),
                        value,
                    })
                    .collect(),
            ),
            position(),
        )
    }

    fn schema() -> LoadedSchema {
        let text = r#"{
            "type": "object",
            "properties": {
                "queue": {
                    "type": "object",
                    "additionalProperties": {
                        "type": "object",
                        "required": ["name", "visibility"],
                        "properties": {
                            "name": {"type": "string"},
                            "visibility": {"type": "integer"}
                        }
                    }
                }
            }
        }"#;
        let source = MemorySource::new().with_file("/root/mock.schema.json", text);
        LoadedSchema::read(Path::new("/root/mock.schema.json"), &source).unwrap()
    }

    #[test]
    fn a_key_sets_its_entry_field_before_validation() {
        let document = map(vec![(
            "queue",
            map(vec![(
                "name mock-orders",
                map(vec![(
                    "visibility",
                    Value::new(ValueKind::Number("30".into()), position()),
                )]),
            )]),
        )]);
        let patterns = [Regex::new("^(?<field>[a-z]+) (?<value>[a-z-]+)$").unwrap()];
        assert!(schema().validate(&document, &patterns).is_empty());
    }

    #[test]
    fn a_call_is_not_checked_and_a_wrong_type_is() {
        let call = Value::new(ValueKind::Call(Call::new("mock", Vec::new())), position());
        let document = map(vec![(
            "queue",
            map(vec![(
                "orders",
                map(vec![
                    ("name", call),
                    ("visibility", Value::string("thirty", position())),
                ]),
            )]),
        )]);
        let diagnostics = schema().validate(&document, &[]);
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0]
                .message
                .starts_with("`queue.orders.visibility`"),
            "{}",
            diagnostics[0].message
        );
    }

    #[test]
    fn a_check_through_a_subschema_reading_an_unknown_value_is_not_made() {
        let text = r#"{
            "type": "object",
            "properties": {
                "secrets": {
                    "type": "object",
                    "oneOf": [{"additionalProperties": {"type": "string"}}]
                },
                "count": {"oneOf": [{"type": "integer"}]}
            }
        }"#;
        let source = MemorySource::new().with_file("/root/mock.schema.json", text);
        let schema = LoadedSchema::read(Path::new("/root/mock.schema.json"), &source).unwrap();
        let call = Value::new(ValueKind::Call(Call::new("mock", Vec::new())), position());
        let document = map(vec![
            ("secrets", map(vec![("token", call)])),
            ("count", Value::string("three", position())),
        ]);
        let diagnostics = schema.validate(&document, &[]);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.starts_with("`count`"));
    }

    #[test]
    fn the_modeline_wins_over_the_search_paths() {
        let block = serde_json::json!({
            "id": "mock",
            "file_includes": ["**/mock-dsl.yml"],
            "schema_search_paths": ["{config_root}/search.json"]
        });
        let definition = DslDefinition::from_json(&block).unwrap();
        let source = MemorySource::new()
            .with_file("/root/a/local.json", "{}")
            .with_file("/root/search.json", "{}");
        let text = "# yaml-language-server: $schema=local.json#/properties/queue\nqueue: {}\n";
        let location = schema_location(
            &definition,
            Path::new("/root"),
            Path::new("/root/a/mock-dsl.yml"),
            text,
            &source,
        )
        .unwrap();
        assert_eq!(
            location,
            SchemaLocation {
                path: PathBuf::from("/root/a/local.json"),
                fragment: Some("/properties/queue".into())
            }
        );
        let location = schema_location(
            &definition,
            Path::new("/root"),
            Path::new("/root/a/mock-dsl.yml"),
            "queue: {}\n",
            &source,
        )
        .unwrap();
        assert_eq!(location.path, PathBuf::from("/root/search.json"));
    }
}
