//! Resolves a unit: folds its layers, expands its macros, resolves its names, and
//! validates the result.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use fancy_regex::Regex;

use crate::declarations::{CompiledDeclarationRule, Declaration, Declarations, compile_rules};
use crate::definition::{DslDefinition, Scope};
use crate::diagnostic::Diagnostic;
use crate::discover::Unit;
use crate::fold::deep_merge;
use crate::json_path::{PathSet, PathStep};
use crate::macros::Expander;
use crate::node::{Node, NodeKind};
use crate::reference_scan::{NameReading, Reading, Scanner, Strictness};
use crate::resolved::{MapEntry, Reference, Target, TemplatePart, Value, ValueKind};
use crate::schema::{SchemaCache, schema_location};
use crate::source::FileSource;
use crate::yaml_reader::parse;

/// The values of a logical scope's builtin names, which the caller knows and the
/// document does not, such as the environment an overlay deploys to.
pub trait BuiltinValues: Send + Sync {
    /// The value of `scope`'s builtin `name` in `unit`; `None` leaves it a reference
    /// for the target to read.
    fn value(&self, unit: &Unit, scope: &str, name: &str) -> Option<ValueKind>;
}

/// A unit's document, folded, expanded and resolved.
#[derive(Clone, Debug)]
pub struct ResolvedUnit {
    pub unit: Unit,
    pub document: Value,
    /// Whether the unit's document is a fragment, which is never compiled alone.
    pub fragment: bool,
}

/// What resolving needs that does not change from unit to unit.
pub struct Resolution {
    definition: DslDefinition,
    config_root: std::path::PathBuf,
    scanner: Scanner,
    declaration_rules: Vec<CompiledDeclarationRule>,
    regions: BTreeMap<String, PathSet>,
    child_value_patterns: Vec<Regex>,
    schemas: SchemaCache,
}

impl Resolution {
    pub fn new(
        definition: DslDefinition,
        config_root: std::path::PathBuf,
    ) -> Result<Self, Diagnostic> {
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
        let child_value_patterns = definition
            .validator
            .iter()
            .flat_map(|validator| &validator.child_values_from_parent_key)
            .map(|rule| {
                Regex::new(&rule.pattern).map_err(|error| {
                    Diagnostic::unplaced(format!("pattern `{}`: {error}", rule.pattern))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            definition,
            config_root,
            scanner,
            declaration_rules,
            regions,
            child_value_patterns,
            schemas: SchemaCache::default(),
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
        builtins: Option<&dyn BuiltinValues>,
    ) -> Result<ResolvedUnit, Vec<Diagnostic>> {
        let text = read(source, &unit.file).map_err(|diagnostic| vec![diagnostic])?;
        let location = schema_location(
            &self.definition,
            &self.config_root,
            &unit.file,
            &text,
            source,
        )
        .map_err(|diagnostic| vec![diagnostic])?;
        let overlay = self
            .parse(&unit.file, &text)
            .map_err(|diagnostic| vec![diagnostic])?;
        let folded = match &unit.common_layer {
            Some(common_layer) => {
                let common_text =
                    read(source, common_layer).map_err(|diagnostic| vec![diagnostic])?;
                let common = self
                    .parse(common_layer, &common_text)
                    .map_err(|diagnostic| vec![diagnostic])?;
                deep_merge(&common, &overlay)
            }
            None => overlay,
        };
        let document = value_of(&folded);
        if location.fragment.is_some() {
            return Ok(ResolvedUnit {
                unit: unit.clone(),
                document,
                fragment: true,
            });
        }
        let schema = self
            .schemas
            .load(&location.path, source)
            .map_err(|message| vec![Diagnostic::at(&document.position, message)])?;

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
            diagnostics.extend(schema.validate(&resolved, &self.child_value_patterns));
        }
        if diagnostics.is_empty() {
            Ok(ResolvedUnit {
                unit: unit.clone(),
                document: resolved,
                fragment: false,
            })
        } else {
            Err(diagnostics)
        }
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

fn read(source: &dyn FileSource, path: &Path) -> Result<String, Diagnostic> {
    source
        .read(path)
        .map_err(|error| Diagnostic::unplaced(format!("reading {}: {error}", path.display())))
}

fn value_of(node: &Node) -> Value {
    let kind = match &node.kind {
        NodeKind::Null => ValueKind::Null,
        NodeKind::Bool(flag) => ValueKind::Bool(*flag),
        NodeKind::Number(text) => ValueKind::Number(text.clone()),
        NodeKind::String(text) => ValueKind::String(text.clone()),
        NodeKind::List(items) => ValueKind::List(items.iter().map(value_of).collect()),
        NodeKind::Map(entries) => ValueKind::Map(
            entries
                .iter()
                .map(|entry| MapEntry {
                    key: entry.key.clone(),
                    key_position: entry.key_position.clone(),
                    value: value_of(&entry.value),
                })
                .collect(),
        ),
    };
    Value::new(kind, node.position.clone())
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

    use super::*;
    use crate::resolved::Call;
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
schema_search_paths: ["{config_root}/mock.schema.json"]
placeholder:
  pattern: "\\$\\{(?<body>[^}\\n]*)\\}"
  unscanned_paths: []
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
        let source = MemorySource::new()
            .with_file("/root/mock.schema.json", SCHEMA)
            .with_file("/root/mock-dsl.yml", document);
        let resolution = Resolution::new(definition(), PathBuf::from("/root")).unwrap();
        let unit = Unit {
            id: "mock-dsl.yml".into(),
            file: PathBuf::from("/root/mock-dsl.yml"),
            directory: PathBuf::from("/root"),
            overlay: None,
            common_layer: None,
        };
        resolution
            .resolve(&unit, &source, None)
            .map(|resolved| resolved.document)
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
            ValueKind::Number(text) => crate::node::number_json(text),
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
