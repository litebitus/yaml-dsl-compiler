//! Finds a document's schema, reads its function table, and validates the
//! resolved document against it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use fancy_regex::Regex;
use jsonschema::error::{TypeKind, ValidationErrorKind};
use jsonschema::{JsonType, ValidationError};
use serde_json::{Map, Value as JsonValue};

use crate::diagnostic::Diagnostic;
use crate::function_table::FunctionTable;
use crate::node::number_json;
use crate::position::Position;
use crate::resolved::{Value, ValueKind};
use crate::schema_url_copies::{SchemaUrlCopies, is_schema_url};
use crate::source::FileSource;

const MODELINE_MARKER: &str = "yaml-language-server: $schema=";
const CONFIG_ROOT: &str = "{config_root}";

/// Where a document's schema is, and whether the document is a fragment of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaLocation {
    pub source: SchemaSource,
    /// The modeline's fragment: a document that has one is a fragment, never a full document.
    pub fragment: Option<String>,
}

/// A schema's place: a file on disk, or a URL read with the copies kept of it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchemaSource {
    File(PathBuf),
    Url(String),
}

pub struct LoadedSchema {
    pub table: FunctionTable,
    validator: jsonschema::Validator,
    schema: JsonValue,
    /// The schema as a report names it: its file's name, or its URL.
    name: String,
}

/// A schema loaded once, by the first unit that reads it, or why it did not load.
type SchemaLoad = Arc<OnceLock<Result<Arc<LoadedSchema>, String>>>;

/// Schemas loaded once and shared by every unit that reads them.
#[derive(Default)]
pub struct SchemaCache {
    loaded: Mutex<BTreeMap<SchemaSource, SchemaLoad>>,
}

impl SchemaCache {
    pub fn load(
        &self,
        schema: &SchemaSource,
        source: &dyn FileSource,
        url_copies: Option<&SchemaUrlCopies>,
    ) -> Result<Arc<LoadedSchema>, String> {
        let cell = {
            let mut loaded = self
                .loaded
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(loaded.entry(schema.clone()).or_default())
        };
        cell.get_or_init(|| {
            let text = match schema {
                SchemaSource::File(path) => source
                    .read(path)
                    .map_err(|error| format!("reading schema {}: {error}", path.display()))?,
                SchemaSource::Url(url) => url_copies
                    .ok_or_else(|| {
                        format!("schema URL `{url}`: the caller names no folder for kept copies")
                    })?
                    .read(url)?,
            };
            LoadedSchema::parse(&schema.report_name(), &text).map(Arc::new)
        })
        .clone()
    }
}

impl SchemaSource {
    /// The schema as a report names it: its file's name, or its URL.
    fn report_name(&self) -> String {
        match self {
            SchemaSource::File(path) => path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ),
            SchemaSource::Url(url) => url.clone(),
        }
    }
}

impl std::fmt::Display for SchemaSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaSource::File(path) => write!(formatter, "{}", path.display()),
            SchemaSource::Url(url) => write!(formatter, "{url}"),
        }
    }
}

/// The schema of the document at `document_path`: its modeline first, then the
/// search paths. A URL is taken as written; a path, where a file is on disk.
pub fn schema_location(
    search_paths: &[String],
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
    if let Some((_, uri)) = &modeline {
        let (path, fragment) = match uri.split_once('#') {
            Some((path, fragment)) => (path, Some(fragment.to_owned())),
            None => (uri.as_str(), None),
        };
        if is_schema_url(path) {
            return Ok(SchemaLocation {
                source: SchemaSource::Url(path.to_owned()),
                fragment,
            });
        }
        let path = directory.join(path.split('?').next().unwrap_or(path));
        if source.exists(&path) {
            return Ok(SchemaLocation {
                source: SchemaSource::File(path),
                fragment,
            });
        }
    }
    for entry in search_paths {
        if is_schema_url(entry) {
            return Ok(SchemaLocation {
                source: SchemaSource::Url(entry.clone()),
                fragment: None,
            });
        }
        let path = match entry.strip_prefix(CONFIG_ROOT) {
            Some(rest) => config_root.join(rest.trim_start_matches('/')),
            None => directory.join(entry),
        };
        if source.exists(&path) {
            return Ok(SchemaLocation {
                source: SchemaSource::File(path),
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
    fn parse(name: &str, text: &str) -> Result<Self, String> {
        let schema: JsonValue =
            serde_json::from_str(text).map_err(|error| format!("schema {name}: {error}"))?;
        let table = FunctionTable::from_schema(&schema)
            .map_err(|error| format!("schema {name}: {error}"))?;
        let validator = jsonschema::validator_for(&schema)
            .map_err(|error| format!("schema {name}: {error}"))?;
        Ok(Self {
            table,
            validator,
            schema,
            name: name.to_owned(),
        })
    }

    /// Validates `document`. A value not known when the document compiles, such as
    /// a call or a reference to a declaration, is not checked.
    pub fn validate(&self, document: &Value, child_value_patterns: &[Regex]) -> Vec<Diagnostic> {
        let mut instance = Instance {
            child_value_patterns,
            positions: BTreeMap::new(),
            value_positions: BTreeMap::new(),
            unknown: BTreeSet::new(),
        };
        let json = instance.json(document, String::new(), &document.position);
        let mut diagnostics: Vec<Diagnostic> = self
            .validator
            .iter_errors(&json)
            .filter(|error| !instance.reads_unknown(error.instance_path().as_str(), error.kind()))
            .flat_map(|error| self.error_diagnostics(&error, &instance))
            .collect();
        diagnostics.dedup();
        diagnostics
    }

    /// A schema error as the report a reader fixes it from: placed at the value or
    /// the field that is wrong, labeled with what is wrong there, naming the rule
    /// it breaks, and the allowed name closest to a misspelt one.
    fn error_diagnostics(
        &self,
        error: &ValidationError<'_>,
        instance: &Instance<'_>,
    ) -> Vec<Diagnostic> {
        let pointer = error.instance_path().as_str();
        let rule = format!(
            "{}, at {}#{}",
            error.kind().keyword(),
            self.name,
            error.schema_path().as_str()
        );
        let holder = match last_step(pointer) {
            Some(name) => format!("`{name}`"),
            None => "the document".to_owned(),
        };
        let suggestion = |name: &str, candidates: Vec<String>| {
            closest(name, &candidates).map(|candidate| format!("did you mean `{candidate}`?"))
        };
        match error.kind() {
            ValidationErrorKind::AdditionalProperties { unexpected }
            | ValidationErrorKind::UnevaluatedProperties { unexpected } => {
                let allowed = allowed_fields(&self.schema, error.schema_path().as_str());
                unexpected
                    .iter()
                    .map(|name| {
                        let field = format!("{pointer}/{}", escape_pointer(name));
                        Diagnostic::at(
                            instance.position(&field),
                            format!("field `{name}` is not allowed"),
                        )
                        .with_label(format!("{holder} has no field `{name}`"))
                        .with_rule(rule.clone())
                        .with_help(suggestion(name, allowed.clone()))
                    })
                    .collect()
            }
            ValidationErrorKind::Enum { options } => {
                let allowed: Vec<String> = options
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|option| option.as_str().map(str::to_owned))
                    .collect();
                let count = options.as_array().map_or(0, Vec::len);
                let help = error
                    .instance()
                    .as_str()
                    .and_then(|text| suggestion(text, allowed));
                vec![
                    Diagnostic::at(
                        instance.value_position(pointer),
                        format!("`{}` is not an allowed value", shown(error.instance())),
                    )
                    .with_label(format!("not one of the {count} values `enum` allows"))
                    .with_rule(rule)
                    .with_help(help),
                ]
            }
            ValidationErrorKind::Type { kind } => {
                let expected: Vec<&str> = match kind {
                    TypeKind::Single(single) => vec![single.as_str()],
                    TypeKind::Multiple(several) => several.iter().map(JsonType::as_str).collect(),
                };
                vec![
                    Diagnostic::at(
                        instance.value_position(pointer),
                        format!("{holder} is not {}", with_article(&expected.join(" or "))),
                    )
                    .with_label(format!("is {}", with_article(json_type(error.instance()))))
                    .with_rule(rule),
                ]
            }
            ValidationErrorKind::Required { property } => {
                let name = property.as_str().unwrap_or_default();
                vec![
                    Diagnostic::at(
                        instance.position(pointer),
                        format!("field `{name}` is required"),
                    )
                    .with_label(format!("{holder} has no field `{name}`"))
                    .with_rule(rule),
                ]
            }
            _ => vec![
                Diagnostic::at(
                    instance.value_position(pointer),
                    format!("{}: {error}", display_pointer(pointer)),
                )
                .with_label(format!("breaks `{}`", error.kind().keyword()))
                .with_rule(rule),
            ],
        }
    }
}

/// The fields a schema node allows, by the `properties` of the node holding the
/// keyword at `keyword_pointer`, of the schemas it references and of those it
/// combines.
fn allowed_fields(schema: &JsonValue, keyword_pointer: &str) -> Vec<String> {
    let holder_pointer = keyword_pointer
        .rsplit_once('/')
        .map_or("", |(holder, _)| holder);
    let mut fields = Vec::new();
    let mut visited = BTreeSet::new();
    if let Some(holder) = schema.pointer(holder_pointer) {
        collect_fields(schema, holder, &mut fields, &mut visited);
    }
    fields
}

fn collect_fields(
    schema: &JsonValue,
    node: &JsonValue,
    fields: &mut Vec<String>,
    visited: &mut BTreeSet<String>,
) {
    if let Some(properties) = node.get("properties").and_then(JsonValue::as_object) {
        for name in properties.keys() {
            if !fields.contains(name) {
                fields.push(name.clone());
            }
        }
    }
    if let Some(reference) = node.get("$ref").and_then(JsonValue::as_str)
        && let Some(pointer) = reference.strip_prefix('#')
        && visited.insert(pointer.to_owned())
        && let Some(target) = schema.pointer(pointer)
    {
        collect_fields(schema, target, fields, visited);
    }
    for combinator in ["allOf", "anyOf", "oneOf"] {
        for branch in node
            .get(combinator)
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
        {
            collect_fields(schema, branch, fields, visited);
        }
    }
}

/// The candidate closest in spelling to `name`, where one is close enough to be a
/// likely fix and no other is as close.
fn closest(name: &str, candidates: &[String]) -> Option<String> {
    let mut ranked: Vec<(usize, &String)> = candidates
        .iter()
        .filter(|candidate| candidate.as_str() != name)
        .map(|candidate| (edit_distance(name, candidate), candidate))
        .collect();
    ranked.sort();
    let limit = (name.chars().count() / 3).max(1);
    match ranked.as_slice() {
        [(distance, best), rest @ ..]
            if *distance <= limit && rest.first().is_none_or(|(next, _)| next > distance) =>
        {
            Some((*best).clone())
        }
        _ => None,
    }
}

/// The fewest single-character insertions, deletions and substitutions turning
/// one text into the other.
fn edit_distance(first: &str, second: &str) -> usize {
    let second: Vec<char> = second.chars().collect();
    let mut previous: Vec<usize> = (0..=second.len()).collect();
    for (row, first_character) in first.chars().enumerate() {
        let mut current = vec![row + 1];
        for (column, second_character) in second.iter().enumerate() {
            let substitution = previous[column] + usize::from(first_character != *second_character);
            current.push(
                substitution
                    .min(previous[column + 1] + 1)
                    .min(current[column] + 1),
            );
        }
        previous = current;
    }
    previous[second.len()]
}

/// The last key of a JSON pointer, or `None` for the document's root.
fn last_step(pointer: &str) -> Option<String> {
    let step = pointer.rsplit_once('/')?.1;
    Some(step.replace("~1", "/").replace("~0", "~"))
}

fn shown(value: &JsonValue) -> String {
    match value {
        JsonValue::String(text) => format!("{text:?}"),
        other => other.to_string(),
    }
}

fn json_type(value: &JsonValue) -> &'static str {
    match value {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "boolean",
        JsonValue::Number(number) if number.is_i64() || number.is_u64() => "integer",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

fn with_article(noun: &str) -> String {
    let article = if noun.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    format!("{article} {noun}")
}

struct Instance<'p> {
    child_value_patterns: &'p [Regex],
    /// The position of the key holding each value, by JSON pointer.
    positions: BTreeMap<String, Position>,
    /// The position of each value itself, by JSON pointer.
    value_positions: BTreeMap<String, Position>,
    unknown: BTreeSet<String>,
}

impl Instance<'_> {
    fn json(&mut self, value: &Value, pointer: String, key_position: &Position) -> JsonValue {
        self.positions.insert(pointer.clone(), key_position.clone());
        self.value_positions
            .insert(pointer.clone(), value.position.clone());
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

    fn value_position(&self, pointer: &str) -> &Position {
        self.value_positions
            .get(pointer)
            .unwrap_or_else(|| self.position(pointer))
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
    use crate::definition::DslDefinition;
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
        LoadedSchema::parse("mock.schema.json", text).unwrap()
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
        assert_eq!(diagnostics[0].message, "`visibility` is not an integer");
        let notes = diagnostics[0].notes.as_deref().unwrap();
        assert_eq!(notes.label.as_deref(), Some("is a string"));
        assert_eq!(
            notes.rule.as_deref(),
            Some(
                "type, at mock.schema.json#/properties/queue/additionalProperties/properties/visibility/type"
            )
        );
    }

    #[test]
    fn a_misspelt_field_or_value_is_reported_at_itself_with_the_closest_allowed_one() {
        let text = r#"{
            "type": "object",
            "properties": {
                "queue": {
                    "type": "object",
                    "properties": {
                        "branch": {"type": "string"},
                        "tier": {"enum": ["gold", "silver", "bronze"]}
                    },
                    "additionalProperties": false
                }
            }
        }"#;
        let schema = LoadedSchema::parse("mock.schema.json", text).unwrap();
        let file: Arc<str> = Arc::from("mock.yml");
        let at = |line: usize| Position::new(&file, line, 3);
        let entry = |key: &str, line: usize, text: &str| MapEntry {
            key: key.to_owned(),
            key_position: at(line),
            value: Value::string(text, Position::new(&file, line, 9)),
        };
        let queue = Value::new(
            ValueKind::Map(vec![entry("brnch", 2, "main"), entry("tier", 3, "gld")]),
            at(1),
        );
        let document = Value::new(
            ValueKind::Map(vec![MapEntry {
                key: "queue".to_owned(),
                key_position: Position::new(&file, 1, 1),
                value: queue,
            }]),
            Position::new(&file, 1, 1),
        );
        let mut diagnostics = schema.validate(&document, &[]);
        diagnostics.sort_by_key(|diagnostic| diagnostic.position.as_ref().map(|at| at.line));
        let reports: Vec<(usize, usize, &str, Option<&str>)> = diagnostics
            .iter()
            .map(|diagnostic| {
                let position = diagnostic.position.as_ref().unwrap();
                let notes = diagnostic.notes.as_deref().unwrap();
                (
                    position.line,
                    position.column,
                    diagnostic.message.as_str(),
                    notes.help.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            reports,
            [
                (
                    2,
                    3,
                    "field `brnch` is not allowed",
                    Some("did you mean `branch`?")
                ),
                (
                    3,
                    9,
                    "`\"gld\"` is not an allowed value",
                    Some("did you mean `gold`?")
                ),
            ]
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
        let schema = LoadedSchema::parse("mock.schema.json", text).unwrap();
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
            &definition.schema_search_paths,
            Path::new("/root"),
            Path::new("/root/a/mock-dsl.yml"),
            text,
            &source,
        )
        .unwrap();
        assert_eq!(
            location,
            SchemaLocation {
                source: SchemaSource::File(PathBuf::from("/root/a/local.json")),
                fragment: Some("/properties/queue".into())
            }
        );
        let location = schema_location(
            &definition.schema_search_paths,
            Path::new("/root"),
            Path::new("/root/a/mock-dsl.yml"),
            "queue: {}\n",
            &source,
        )
        .unwrap();
        assert_eq!(
            location.source,
            SchemaSource::File(PathBuf::from("/root/search.json"))
        );
        let url = "https://mock.example/mock.schema.json?yaml_dsl_max_age_days=1";
        let location = schema_location(
            &[url.to_owned()],
            Path::new("/root"),
            Path::new("/root/a/mock-dsl.yml"),
            &format!("# yaml-language-server: $schema={url}#\nqueue: {{}}\n"),
            &source,
        )
        .unwrap();
        assert_eq!(location.source, SchemaSource::Url(url.to_owned()));
        assert_eq!(location.fragment.as_deref(), Some(""));
    }
}
