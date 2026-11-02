//! Resolves a unit: folds its layers, expands its macros, resolves its names, and
//! validates the result.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fancy_regex::Regex;
use yaml_dsl_spec::Diagnostic;
use yaml_dsl_spec::Position;
use yaml_dsl_spec::ValidationRules;
use yaml_dsl_spec::definition::{DslDefinition, Scope};
use yaml_dsl_spec::json_path::{PathSet, PathStep};
use yaml_dsl_spec::resolved::{
    Declared, MapEntry, Reference, Target, TemplatePart, Value, ValueKind,
};
use yaml_dsl_spec::schema::{SchemaCache, child_value};
use yaml_dsl_spec::yaml_reader::parse;
use yaml_dsl_spec::{Entry, Node, NodeKind};

use crate::declarations::{CompiledDeclarationRule, Declaration, Declarations, compile_rules};
use crate::discover::Unit;
use crate::fold::deep_merge;
use crate::macros::Expander;
use crate::reference_scan::{NameReading, Reading, Resolver, Scanner, Strictness};
use crate::schema_provider::SchemaProvider;
use crate::source::FileSource;

/// The document an overlay holding no documents reads as: an empty map.
const EMPTY_DOCUMENT: &str = "{}\n";

/// The values of a logical scope's builtin names, which the caller knows and the
/// document does not, such as the environment an overlay deploys to.
pub trait BuiltinValues: Send + Sync {
    /// The value of `scope`'s builtin `name` in `unit`; `None` leaves it a reference
    /// for the target to read.
    fn value(&self, unit: &Unit, scope: &str, name: &str) -> Option<ValueKind>;
}

/// What one resolution takes beside its unit.
#[derive(Clone, Copy, Default)]
pub struct ResolveOptions<'o> {
    /// The values of the unit's builtin names.
    pub builtins: Option<&'o dyn BuiltinValues>,
    /// The caller's code that finds the unit's schema.
    pub schema_provider: Option<&'o dyn SchemaProvider>,
}

/// A unit's document, folded, expanded and resolved.
#[derive(Clone, Debug)]
pub struct ResolvedUnit {
    pub unit: Unit,
    pub document: Value,
    /// Every name the document declares, in the order it writes them.
    pub declarations: Vec<Declared>,
    /// Whether the unit's document is a fragment, which is never compiled alone.
    pub fragment: bool,
    /// What resolving warns of without failing, such as what finding its schema
    /// warned of.
    pub warnings: Vec<Diagnostic>,
}

/// What resolving needs that does not change from unit to unit.
pub struct Resolution {
    definition: DslDefinition,
    config_root: PathBuf,
    scanner: Scanner,
    declaration_rules: Vec<CompiledDeclarationRule>,
    regions: BTreeMap<String, PathSet>,
    validation_rules: ValidationRules,
    schemas: SchemaCache,
    /// Common layers, parsed once however many units fold over them.
    common_layers: Mutex<BTreeMap<PathBuf, Result<Arc<Node>, Diagnostic>>>,
}

impl Resolution {
    pub fn new(definition: DslDefinition, config_root: PathBuf) -> Result<Self, Diagnostic> {
        let scanner = Scanner::new(&definition)?;
        let declaration_rules = compile_rules(&definition.declarations)?;
        let mut regions = BTreeMap::new();
        for (name, scope) in &definition.scopes {
            let paths = scope
                .regions
                .iter()
                .map(|region| (region.as_str(), &[][..]));
            regions.insert(name.clone(), PathSet::parse(paths)?);
        }
        let validation_rules = ValidationRules::from_validator(definition.validator.as_ref())
            .map_err(|error| Diagnostic::unplaced(format!("pattern {error}")))?;
        Ok(Self {
            definition,
            config_root,
            scanner,
            declaration_rules,
            regions,
            validation_rules,
            schemas: SchemaCache::default(),
            common_layers: Mutex::default(),
        })
    }

    pub fn definition(&self) -> &DslDefinition {
        &self.definition
    }

    pub fn config_root(&self) -> &Path {
        &self.config_root
    }

    pub fn resolve(
        &self,
        unit: &Unit,
        source: &dyn FileSource,
        options: &ResolveOptions<'_>,
    ) -> Result<ResolvedUnit, Vec<Diagnostic>> {
        let builtins = options.builtins;
        let text = if unit.holds_no_documents {
            EMPTY_DOCUMENT.to_owned()
        } else {
            read(source, &unit.file).map_err(|diagnostic| vec![diagnostic])?
        };
        let file: Arc<str> = Arc::from(unit.file.to_string_lossy().as_ref());
        let document_start = Position::file_start(&file);
        let provider = options.schema_provider.ok_or_else(|| {
            vec![Diagnostic::at(
                &document_start,
                "the caller gives no schema provider to find this document's schema",
            )]
        })?;
        let document_schema = provider
            .schema_of(unit, &text)
            .map_err(|diagnostic| vec![diagnostic])?;
        let overlay = self
            .parse(&unit.file, &text)
            .map_err(|diagnostic| vec![diagnostic])?;
        let common = match &unit.common_layer {
            Some(common_layer) => Some(
                self.common_layer(common_layer, source)
                    .map_err(|diagnostic| vec![diagnostic])?,
            ),
            None => None,
        };
        let first_fold = Value::from_node(&fold_layers(common.as_ref(), &overlay));
        if document_schema.fragment.is_some() {
            return Ok(ResolvedUnit {
                unit: unit.clone(),
                document: first_fold,
                declarations: Vec::new(),
                fragment: true,
                warnings: Vec::new(),
            });
        }
        let schema = self
            .schemas
            .load(&document_schema.name, &document_schema.text)
            .map_err(|message| vec![Diagnostic::at(&document_start, message)])?;

        let key_expander = Expander::new(
            &self.definition,
            &self.scanner,
            &schema.table,
            self.raw_locals(&first_fold),
        )
        .map_err(|diagnostic| vec![diagnostic])?;
        let mut resolve_key =
            |reading: &NameReading<'_>| self.key_reading(reading, &key_expander, unit, builtins);
        let overlay = self.resolve_keys(&overlay, &mut Vec::new(), &mut resolve_key);
        let common =
            common.map(|common| self.resolve_keys(&common, &mut Vec::new(), &mut resolve_key));
        let document = Value::from_node(&fold_layers(common.as_ref(), &overlay));

        let mut diagnostics = Vec::new();
        let raw_locals = self.raw_locals(&document);
        let expander = Expander::new(&self.definition, &self.scanner, &schema.table, raw_locals)
            .map_err(|diagnostic| vec![diagnostic])?;
        let expanded = expander.expand_document(&document, &mut diagnostics);
        let declarations = Declarations::collect(
            &expanded,
            &self.declaration_rules,
            &self.definition.scopes,
            None,
        );
        let names = NameResolver {
            resolution: self,
            declarations: &declarations,
            unit,
            builtins,
        };
        let resolved = names.resolve(&expanded, &mut Vec::new(), &mut diagnostics);
        if diagnostics.is_empty() {
            diagnostics.extend(schema.validate(&resolved, &self.validation_rules));
        }
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
        let declarations = Declarations::collect(
            &resolved,
            &self.declaration_rules,
            &self.definition.scopes,
            None,
        )
        .in_document_order()
        .into_iter()
        .map(|declared| Declared {
            value: with_child_values(
                &declared.value,
                &declared.key,
                &self.validation_rules.child_value_patterns,
            ),
            ..declared
        })
        .collect();
        let warnings = document_schema
            .warnings
            .iter()
            .map(|warning| Diagnostic::at(&document_start, warning.clone()))
            .collect();
        Ok(ResolvedUnit {
            unit: unit.clone(),
            document: resolved,
            declarations,
            fragment: false,
            warnings,
        })
    }

    /// Reads a key's placeholder before the fold: a builtin the caller gives a
    /// value, or a local whose value is a scalar. Anything else stays as written,
    /// for the resolution after the fold.
    fn key_reading(
        &self,
        reading: &NameReading<'_>,
        expander: &Expander<'_>,
        unit: &Unit,
        builtins: Option<&dyn BuiltinValues>,
    ) -> Reading {
        if let Some(kind) = self.builtin_value(reading, unit, builtins) {
            return Reading::Value(Value::new(kind, reading.position.clone()));
        }
        match expander.read_local(reading) {
            Reading::Value(value) => Reading::Value(value),
            Reading::Reference(Reference {
                target: Target::Local(value),
                ..
            }) if value.scalar_text().is_some() => Reading::Value(*value),
            _ => Reading::Keep,
        }
    }

    /// The value the caller gives a builtin of a logical scope, where the reading
    /// names one.
    fn builtin_value(
        &self,
        reading: &NameReading<'_>,
        unit: &Unit,
        builtins: Option<&dyn BuiltinValues>,
    ) -> Option<ValueKind> {
        let scope_name = reading.rule.scope_name.as_str();
        let scope = self.definition.scopes.get(scope_name)?;
        if !scope.builtin_names.contains(&reading.name) {
            return None;
        }
        builtins?.value(unit, scope_name, &reading.name)
    }

    fn resolve_keys(
        &self,
        node: &Node,
        path: &mut Vec<PathStep>,
        resolve: &mut Resolver<'_>,
    ) -> Node {
        let kind = match &node.kind {
            NodeKind::Map(entries) => NodeKind::Map(
                entries
                    .iter()
                    .map(|entry| {
                        let key = self
                            .scanner
                            .scan_text(
                                &entry.key,
                                &entry.key_position,
                                path,
                                Strictness::Lenient,
                                resolve,
                            )
                            .unwrap_or_else(|_| entry.key.clone());
                        path.push(PathStep::Key(key.clone()));
                        let value = self.resolve_keys(&entry.value, path, resolve);
                        path.pop();
                        Entry {
                            key,
                            key_position: entry.key_position.clone(),
                            value,
                        }
                    })
                    .collect(),
            ),
            NodeKind::List(items) => NodeKind::List(
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        path.push(PathStep::Index(index));
                        let item = self.resolve_keys(item, path, resolve);
                        path.pop();
                        item
                    })
                    .collect(),
            ),
            other => other.clone(),
        };
        Node::new(kind, node.position.clone())
    }

    fn common_layer(&self, path: &Path, source: &dyn FileSource) -> Result<Node, Diagnostic> {
        let mut parsed = self
            .common_layers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let layer = parsed.entry(path.to_path_buf()).or_insert_with(|| {
            read(source, path)
                .and_then(|text| self.parse(path, &text))
                .map(Arc::new)
        });
        layer
            .as_ref()
            .map(|node| (**node).clone())
            .map_err(Clone::clone)
    }

    fn parse(&self, path: &Path, text: &str) -> Result<Node, Diagnostic> {
        let file: Arc<str> = Arc::from(path.to_string_lossy().as_ref());
        parse(&file, text, &self.definition.tag_keys)
    }

    fn raw_locals(&self, document: &Value) -> BTreeMap<String, (Vec<PathStep>, MapEntry)> {
        let Some(locals) = &self.definition.locals else {
            return BTreeMap::new();
        };
        let declared = Declarations::collect(
            document,
            &self.declaration_rules,
            &self.definition.scopes,
            Some(&locals.scope_name),
        );
        declared
            .of_scope(&locals.scope_name)
            .filter_map(|(name, declaration)| {
                let (Some(PathStep::Key(key)), parent) = (
                    declaration.path.last(),
                    &declaration.path[..declaration.path.len() - 1],
                ) else {
                    return None;
                };
                let entry = MapEntry {
                    key: key.clone(),
                    key_position: declaration.position.clone(),
                    value: declaration.value.clone(),
                };
                Some((name.to_owned(), (parent.to_vec(), entry)))
            })
            .collect()
    }
}

fn fold_layers(common: Option<&Node>, overlay: &Node) -> Node {
    match common {
        Some(common) => deep_merge(common, overlay),
        None => overlay.clone(),
    }
}

/// `value` with the field its declaring key sets, placed first, unless the value
/// writes the field itself.
fn with_child_values(value: &Value, key: &str, patterns: &[Regex]) -> Value {
    let (Some(entries), Some((field, field_value))) = (value.entries(), child_value(key, patterns))
    else {
        return value.clone();
    };
    if entries.iter().any(|entry| entry.key == field) {
        return value.clone();
    }
    let set = MapEntry {
        key: field,
        key_position: value.position.clone(),
        value: Value::string(field_value, value.position.clone()),
    };
    let entries = std::iter::once(set)
        .chain(entries.iter().cloned())
        .collect();
    Value::new(ValueKind::Map(entries), value.position.clone())
}

fn read(source: &dyn FileSource, path: &Path) -> Result<String, Diagnostic> {
    source
        .read(path)
        .map_err(|error| Diagnostic::unplaced(format!("reading {}: {error}", path.display())))
}

/// Resolves the names left after expansion: builtins and declarations. A name no
/// rule resolves is an error.
struct NameResolver<'r> {
    resolution: &'r Resolution,
    declarations: &'r Declarations,
    unit: &'r Unit,
    builtins: Option<&'r dyn BuiltinValues>,
}

impl NameResolver<'_> {
    fn resolve(
        &self,
        value: &Value,
        path: &mut Vec<PathStep>,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Value {
        match &value.kind {
            ValueKind::String(text) => {
                let mut resolve = |reading: &NameReading<'_>| self.read(reading, path);
                let scanned = self.resolution.scanner.scan(
                    text,
                    &value.position,
                    path,
                    Strictness::Strict,
                    &mut resolve,
                );
                scanned.unwrap_or_else(|diagnostic| {
                    diagnostics.push(diagnostic);
                    value.clone()
                })
            }
            ValueKind::List(items) => {
                let items = items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        path.push(PathStep::Index(index));
                        let resolved = self.resolve(item, path, diagnostics);
                        path.pop();
                        resolved
                    })
                    .collect();
                Value::new(ValueKind::List(items), value.position.clone())
            }
            ValueKind::Map(entries) => {
                let mut resolved_entries = Vec::with_capacity(entries.len());
                for entry in entries {
                    let mut resolve = |reading: &NameReading<'_>| self.read(reading, path);
                    let key = self
                        .resolution
                        .scanner
                        .scan_text(
                            &entry.key,
                            &entry.key_position,
                            path,
                            Strictness::Strict,
                            &mut resolve,
                        )
                        .unwrap_or_else(|diagnostic| {
                            diagnostics.push(diagnostic);
                            entry.key.clone()
                        });
                    path.push(PathStep::Key(key.clone()));
                    let resolved = self.resolve(&entry.value, path, diagnostics);
                    path.pop();
                    resolved_entries.push(MapEntry {
                        key,
                        key_position: entry.key_position.clone(),
                        value: resolved,
                    });
                }
                Value::new(ValueKind::Map(resolved_entries), value.position.clone())
            }
            ValueKind::Reference(reference) => {
                let mut reference = reference.clone();
                if let Target::Local(local) = &reference.target {
                    reference.target =
                        Target::Local(Box::new(self.resolve(local, path, diagnostics)));
                }
                Value::new(ValueKind::Reference(reference), value.position.clone())
            }
            ValueKind::Call(call) => {
                let mut call = call.clone();
                call.arguments = call
                    .arguments
                    .iter()
                    .map(|argument| self.resolve(argument, path, diagnostics))
                    .collect();
                Value::new(ValueKind::Call(call), value.position.clone())
            }
            ValueKind::Template(parts) => {
                let parts = parts
                    .iter()
                    .map(|part| match part {
                        TemplatePart::Reference(reference) => {
                            let mut reference = reference.clone();
                            if let Target::Local(local) = &reference.target {
                                reference.target =
                                    Target::Local(Box::new(self.resolve(local, path, diagnostics)));
                            }
                            TemplatePart::Reference(reference)
                        }
                        text => text.clone(),
                    })
                    .collect();
                Value::new(ValueKind::Template(parts), value.position.clone())
            }
            ValueKind::Null | ValueKind::Bool(_) | ValueKind::Number(_) => value.clone(),
        }
    }

    fn read(&self, reading: &NameReading<'_>, path: &[PathStep]) -> Reading {
        let scope_name = reading.rule.scope_name.as_str();
        let definition = self.resolution.definition();
        let bound_by_expansion = definition
            .locals
            .as_ref()
            .is_some_and(|locals| locals.scope_name == scope_name)
            || definition
                .loops
                .as_ref()
                .is_some_and(|loops| loops.scope_name == scope_name);
        if bound_by_expansion {
            return Reading::Keep;
        }
        let scope = definition.scopes.get(scope_name);
        let builtin_names = scope.map_or(&[][..], |scope| scope.builtin_names.as_slice());
        if !builtin_names.is_empty() {
            if !builtin_names.contains(&reading.name) || !self.visible(scope_name, path) {
                return Reading::Keep;
            }
            if let Some(kind) = self
                .builtins
                .and_then(|builtins| builtins.value(self.unit, scope_name, &reading.name))
            {
                return Reading::Value(Value::new(kind, reading.position.clone()));
            }
            return Reading::Reference(self.reference(reading, Target::Builtin));
        }
        let scope_key = reading.scope_key.as_deref();
        match self.declarations.find(scope_name, scope_key, &reading.name) {
            Some(found) if self.declaration_visible(scope_name, scope, found, path) => {
                Reading::Reference(self.reference(
                    reading,
                    Target::Declaration {
                        position: found[0].position.clone(),
                    },
                ))
            }
            Some(_) => Reading::Keep,
            None if self.declarations.declares_every_name(scope_name, scope_key)
                && self.visible(scope_name, path) =>
            {
                Reading::Reference(self.reference(reading, Target::Undeclared))
            }
            None => Reading::Keep,
        }
    }

    fn reference(&self, reading: &NameReading<'_>, target: Target) -> Reference {
        Reference {
            scope: reading.rule.scope_name.clone(),
            scope_key: reading.scope_key.clone(),
            name: reading.name.clone(),
            path: reading.path.clone(),
            text: reading.text.clone(),
            target,
        }
    }

    fn visible(&self, scope_name: &str, path: &[PathStep]) -> bool {
        self.resolution
            .regions
            .get(scope_name)
            .is_none_or(|regions| regions.covers(path))
    }

    fn declaration_visible(
        &self,
        scope_name: &str,
        scope: Option<&Scope>,
        found: &[Declaration],
        path: &[PathStep],
    ) -> bool {
        if self.visible(scope_name, path) {
            return true;
        }
        scope.is_some_and(|scope| scope.later_items_of_declaring_list)
            && found
                .iter()
                .any(|declaration| in_later_item(&declaration.path, path))
    }
}

/// Whether `site` is inside a later item of a list that holds `declaration`.
fn in_later_item(declaration: &[PathStep], site: &[PathStep]) -> bool {
    declaration.iter().enumerate().any(|(depth, step)| {
        let PathStep::Index(declared_index) = step else {
            return false;
        };
        site.len() > depth
            && site[..depth] == declaration[..depth]
            && matches!(site[depth], PathStep::Index(site_index) if site_index > *declared_index)
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use yaml_dsl_spec::resolved::Call;

    use super::*;
    use crate::schema_provider::DocumentSchema;
    use crate::source::MemorySource;

    const SCHEMA: &str = r#"{
        "x-yaml-dsl-functions": {
            "encode": {"arguments": [{"name": "value"}]},
            "loop": {
                "arguments": [{"name": "in"}, {"name": "body"}, {"name": "for", "default": ["it"]}],
                "macro": {
                    "kind": "loop", "loop_variables": "for", "collection": "in", "body": "body"
                }
            },
            "merge": {
                "arguments": [{"name": "contents", "repeated": true}],
                "macro": {"kind": "merge", "content": "contents"}
            }
        }
    }"#;

    const DEFINITION: &str = r#"
id: mock
file_includes: ["**/mock-dsl.yml"]
placeholder:
  pattern: "\\$\\{(?<body>[^}\\n]*)\\}"
  escape_prefix: "$"
  unscanned_paths: []
validator:
  child_values_from_parent_key:
    - pattern: "^(?<field>name) (?<value>[a-z-]+)$"
function:
  marker_function:
    call_marker: fn.
    splat_operator: "*"
    calls_without_name_allowed: true
locals: { scope_name: local, path_separator: "." }
loops: { scope_name: LOOP }
scopes:
  ENTRY: { regions: ["$"], named_by_parent_key: true }
  GLOBAL: { regions: ["$"], builtin_names: [env] }
  local: { regions: ["$"] }
  LOOP: { regions: ["$"] }
declarations:
  - path: "$.locals.*"
    name_source: key
    key_token: first_word
    name_spelling: as_written
    scope_name: local
  - path: "$.*.*"
    skip_keys: [locals]
    name_source: key
    key_token: last_word
    name_spelling: as_written
    scope_name: ENTRY
references:
  - pattern: "^ref (?<type>[a-z]+)\\.(?<name>[a-z_]+)"
    positions: [whole_scalar]
    text_after_name_allowed: true
    scope_name: ENTRY
    scope_group: type
    name_group: name
  - pattern: "^local\\.(?<name>[a-z_]+)"
    positions: [whole_scalar, whole_placeholder, placeholder_in_text]
    text_after_name_allowed: true
    scope_name: local
    name_group: name
  - pattern: "^~(?<name>[a-z_]+)"
    positions: [whole_scalar, whole_placeholder, placeholder_in_text]
    text_after_name_allowed: true
    scope_name: LOOP
    name_group: name
  - pattern: "^(?<name>[a-z_]+)$"
    positions: [whole_placeholder, placeholder_in_text]
    text_after_name_allowed: false
    scope_name: GLOBAL
    name_group: name
"#;

    fn definition() -> DslDefinition {
        let block = parse(&Arc::from("mock-config.yml"), DEFINITION, &[]).unwrap();
        DslDefinition::from_json(&block.to_json()).unwrap()
    }

    fn resolve(document: &str) -> Result<Value, Vec<Diagnostic>> {
        resolve_layers(None, document).map(|resolved| resolved.document)
    }

    fn resolve_layers(
        common_layer: Option<&str>,
        document: &str,
    ) -> Result<ResolvedUnit, Vec<Diagnostic>> {
        let mut source = MemorySource::new().with_file("/root/dev/mock-dsl.yml", document);
        if let Some(common_layer) = common_layer {
            source = source.with_file("/root/mock-dsl.yml", common_layer);
        }
        let resolution = Resolution::new(definition(), PathBuf::from("/root")).unwrap();
        let unit = Unit {
            id: "dev/mock-dsl.yml".into(),
            file: PathBuf::from("/root/dev/mock-dsl.yml"),
            directory: PathBuf::from("/root/dev"),
            overlay: common_layer.map(|_| "dev".to_owned()),
            common_layer: common_layer.map(|_| PathBuf::from("/root/mock-dsl.yml")),
            holds_no_documents: false,
        };
        let options = ResolveOptions {
            builtins: None,
            schema_provider: Some(&MockSchema),
        };
        resolution.resolve(&unit, &source, &options)
    }

    /// The test schema, for every unit.
    struct MockSchema;

    impl SchemaProvider for MockSchema {
        fn schema_of(&self, _unit: &Unit, _text: &str) -> Result<DocumentSchema, Diagnostic> {
            Ok(DocumentSchema {
                name: "mock.schema.json".to_owned(),
                text: SCHEMA.to_owned(),
                fragment: None,
                warnings: Vec::new(),
            })
        }
    }

    #[test]
    fn an_escaped_placeholder_is_text_without_its_prefix() {
        let document = resolve("queue:\n  q-$${a}:\n    label: cost $${missing}\n").unwrap();
        assert_eq!(
            json(document.get("queue").unwrap()),
            serde_json::json!({"q-${a}": {"label": "cost ${missing}"}})
        );
    }

    #[test]
    fn a_key_placeholder_resolves_before_the_layers_fold() {
        let resolved = resolve_layers(
            Some("queue:\n  q-a:\n    x: 1\n"),
            "locals:\n  suffix: a\nqueue:\n  q-${local.suffix}:\n    y: 2\n",
        )
        .unwrap();
        assert_eq!(
            json(resolved.document.get("queue").unwrap()),
            serde_json::json!({"q-a": {"x": 1, "y": 2}})
        );
    }

    #[test]
    fn an_overlay_holding_no_documents_folds_to_its_common_layer_alone() {
        let source =
            MemorySource::new().with_file("/root/mock-dsl.yml", "queue:\n  q-a:\n    x: 1\n");
        let resolution = Resolution::new(definition(), PathBuf::from("/root")).unwrap();
        let unit = Unit {
            id: "uat/mock-dsl.yml".into(),
            file: PathBuf::from("/root/uat/mock-dsl.yml"),
            directory: PathBuf::from("/root/uat"),
            overlay: Some("uat".to_owned()),
            common_layer: Some(PathBuf::from("/root/mock-dsl.yml")),
            holds_no_documents: true,
        };
        let options = ResolveOptions {
            builtins: None,
            schema_provider: Some(&MockSchema),
        };
        let resolved = resolution.resolve(&unit, &source, &options).unwrap();
        assert_eq!(
            json(resolved.document.get("queue").unwrap()),
            serde_json::json!({"q-a": {"x": 1}})
        );
    }

    #[test]
    fn a_local_path_into_a_call_is_known_only_when_used() {
        let document = resolve(concat!(
            "locals:\n  policy fn.encode:\n    a: 1\n",
            "queue:\n  q:\n    x: local.policy.statement\n",
        ))
        .unwrap();
        let x = document
            .get("queue")
            .and_then(|queue| queue.get("q"))
            .and_then(|q| q.get("x"))
            .unwrap();
        assert!(
            matches!(
                &x.kind,
                ValueKind::Reference(Reference {
                    target: Target::LocalUnknown,
                    ..
                })
            ),
            "{x:?}"
        );
    }

    #[test]
    fn declarations_come_in_document_order_with_their_key_fields() {
        let resolved = resolve_layers(
            None,
            "queue:\n  zeta:\n    x: 1\n  name mock-orders:\n    x: 2\nlocals:\n  a: 1\n",
        )
        .unwrap();
        let names: Vec<(&str, &str)> = resolved
            .declarations
            .iter()
            .map(|declared| (declared.scope.as_str(), declared.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [("ENTRY", "zeta"), ("ENTRY", "mock-orders"), ("local", "a")]
        );
        assert_eq!(
            json(&resolved.declarations[1].value),
            serde_json::json!({"name": "mock-orders", "x": 2})
        );
        assert_eq!(resolved.declarations[1].scope_key.as_deref(), Some("queue"));
    }

    fn json(value: &Value) -> serde_json::Value {
        match &value.local_value().kind {
            ValueKind::Map(entries) => serde_json::Value::Object(
                entries
                    .iter()
                    .map(|entry| (entry.key.clone(), json(&entry.value)))
                    .collect(),
            ),
            ValueKind::List(items) => serde_json::Value::Array(items.iter().map(json).collect()),
            ValueKind::String(text) => serde_json::Value::String(text.clone()),
            ValueKind::Number(text) => yaml_dsl_spec::number_json(text),
            ValueKind::Bool(flag) => serde_json::Value::Bool(*flag),
            other => serde_json::Value::String(format!("{other:?}")),
        }
    }

    #[test]
    fn a_loop_yields_one_entry_per_item_with_its_variable_bound() {
        let document = resolve(concat!(
            "locals:\n  names: [a, b]\n",
            "queue:\n  fn.loop:\n    in: local.names\n",
            "    body:\n      q-${~it}:\n        label: ${~it} queue\n",
        ))
        .unwrap();
        assert_eq!(
            json(document.get("queue").unwrap()),
            serde_json::json!({"q-a": {"label": "a queue"}, "q-b": {"label": "b queue"}})
        );
    }

    #[test]
    fn a_merge_of_known_maps_is_expanded_and_own_keys_win() {
        let document = resolve(concat!(
            "locals:\n  base:\n    a: 1\n    b: 1\n",
            "queue:\n  q:\n    fn.merge: local.base\n    b: 2\n",
        ))
        .unwrap();
        assert_eq!(
            json(document.get("queue").unwrap()),
            serde_json::json!({"q": {"a": 1, "b": 2}})
        );
    }

    #[test]
    fn a_merge_reading_a_declaration_is_left_to_the_target_as_a_call() {
        let document = resolve(concat!(
            "queue:\n  p:\n    a: 1\n",
            "  q:\n    tags:\n      fn.merge: ref queue.p\n      b: 2\n",
        ))
        .unwrap();
        let tags = document
            .get("queue")
            .and_then(|queue| queue.get("q"))
            .and_then(|q| q.get("tags"))
            .unwrap();
        let ValueKind::Call(Call {
            function,
            arguments,
            expand_final_argument,
        }) = &tags.kind
        else {
            panic!("{tags:?}")
        };
        assert_eq!(
            (function.as_str(), arguments.len(), *expand_final_argument),
            ("merge", 2, false)
        );
        assert!(matches!(
            &arguments[0].kind,
            ValueKind::Reference(Reference {
                target: Target::Declaration { .. },
                ..
            })
        ));
    }

    #[test]
    fn a_merge_called_with_a_name_is_an_error() {
        let errors = resolve("queue:\n  q:\n    tags fn.merge: {a: 1}\n").unwrap_err();
        assert_eq!(
            errors[0].message,
            "merge macro `merge` is called by a whole key, with no name"
        );
    }

    #[test]
    fn a_function_that_is_not_a_macro_stays_a_call() {
        let document = resolve("queue:\n  q:\n    policy fn.encode:\n      a: ${env}\n").unwrap();
        let policy = document
            .get("queue")
            .and_then(|queue| queue.get("q"))
            .and_then(|q| q.get("policy"))
            .unwrap();
        let ValueKind::Call(call) = &policy.kind else {
            panic!("{policy:?}")
        };
        assert_eq!(call.function, "encode");
        let ValueKind::Map(entries) = &call.arguments[0].kind else {
            panic!()
        };
        assert!(matches!(
            &entries[0].value.kind,
            ValueKind::Reference(Reference {
                target: Target::Builtin,
                ..
            })
        ));
    }

    #[test]
    fn a_local_reaching_itself_is_an_error() {
        let errors = resolve("locals:\n  a: local.b\n  b: local.a\nqueue:\n  q:\n    x: local.a\n")
            .unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("reaches itself")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_reference_no_declaration_answers_names_the_name() {
        let errors = resolve("queue:\n  q:\n    x: ref queue.missing\n").unwrap_err();
        assert_eq!(errors[0].message, "Invalid reference: queue.missing");
        assert_eq!(
            (
                errors[0].position.as_ref().unwrap().line,
                errors[0].position.as_ref().unwrap().column
            ),
            (3, 8)
        );
    }
}
