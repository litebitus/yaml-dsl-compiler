//! Resolves a unit: folds its layers, expands its macros, resolves its names, and
//! validates the result.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fancy_regex::Regex;
use yaml_dsl_spec::Diagnostic;
use yaml_dsl_spec::Position;
use yaml_dsl_spec::Severity;
use yaml_dsl_spec::ValidationRules;
use yaml_dsl_spec::definition::{DslDefinition, Scope};
use yaml_dsl_spec::function_table::{FunctionTable, MacroKind};
use yaml_dsl_spec::json_path::{PathSet, PathStep};
use yaml_dsl_spec::resolved::{
    Call, Declared, Loop, LoopBody, LoopEntry, MapEntry, Reference, Target, TemplatePart, Value,
    ValueKind,
};
use yaml_dsl_spec::schema::{SchemaCache, child_value};
use yaml_dsl_spec::yaml_reader::parse;
use yaml_dsl_spec::{Entry, Node, NodeKind};

use crate::declarations::{CompiledDeclarationRule, Declaration, Declarations, compile_rules};
use crate::discover::Unit;
use crate::fold::deep_merge;
use crate::macros::Expander;
use crate::reference_scan::{KeyReading, NameReading, Reading, Resolver, Scanner, Strictness};
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
            &|entry, path| {
                self.is_deferred_key(entry, path)
                    || expander.is_constant_body_key(&entry.key_position)
            },
        );
        let names = NameResolver {
            resolution: self,
            declarations: &declarations,
            unit,
            builtins,
        };
        let resolved = names.resolve(&expanded, &mut Vec::new(), &mut diagnostics);
        if !holds_error(&diagnostics) {
            diagnostics.extend(schema.validate(&resolved, &self.validation_rules));
        }
        let schema_warnings = document_schema
            .warnings
            .iter()
            .map(|warning| Diagnostic::warning_at(&document_start, warning.clone()));
        diagnostics.splice(0..0, schema_warnings);
        if holds_error(&diagnostics) {
            return Err(diagnostics);
        }
        let declarations = Declarations::collect(
            &resolved,
            &self.declaration_rules,
            &self.definition.scopes,
            None,
            &|entry, _| {
                entry.deferred_key.is_some() || expander.is_constant_body_key(&entry.key_position)
            },
        )
        .in_document_order()
        .into_iter()
        .map(|declared| Declared {
            value: with_child_values(
                &declared.value,
                &declared.key,
                &self.validation_rules.child_value_patterns,
                &schema.table,
            ),
            ..declared
        })
        .collect();
        Ok(ResolvedUnit {
            unit: unit.clone(),
            document: resolved,
            declarations,
            fragment: false,
            warnings: diagnostics,
        })
    }

    /// Reads a key's placeholder before the fold: a builtin the caller gives a
    /// value, or a local whose value is a constant scalar. Anything else stays as
    /// written, for the resolution after the fold.
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
            }) if value.scalar_text().is_some() && !expander.known_when_used(&value) => {
                Reading::Value(*value)
            }
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
        if !self.is_builtin(reading) {
            return None;
        }
        builtins?.value(unit, &reading.rule.scope_name, &reading.name)
    }

    /// Whether an entry's key is a deferred key, or text a rule reads a name other
    /// than a builtin in, which resolution reads as one.
    fn is_deferred_key(&self, entry: &MapEntry, path: &[PathStep]) -> bool {
        entry.deferred_key.is_some()
            || self
                .scanner
                .reads_name_matching(&entry.key, &entry.key_position, path, &|reading| {
                    !self.is_builtin(reading)
                })
    }

    /// Whether the reading names a builtin of its scope.
    fn is_builtin(&self, reading: &NameReading<'_>) -> bool {
        self.definition
            .scopes
            .get(&reading.rule.scope_name)
            .is_some_and(|scope| scope.builtin_names.contains(&reading.name))
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
            &|entry, path| self.is_deferred_key(entry, path),
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
                    deferred_key: None,
                    value: declaration.value.clone(),
                };
                Some((name.to_owned(), (parent.to_vec(), entry)))
            })
            .collect()
    }
}

fn holds_error(diagnostics: &[Diagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == Severity::Error)
}

fn fold_layers(common: Option<&Node>, overlay: &Node) -> Node {
    match common {
        Some(common) => deep_merge(common, overlay),
        None => overlay.clone(),
    }
}

/// `value` with the field its declaring key sets, placed first, unless the value
/// writes the field itself. A merge left for the target takes the field in its
/// own fields, the map its last argument merges over the contents.
fn with_child_values(value: &Value, key: &str, patterns: &[Regex], table: &FunctionTable) -> Value {
    let Some((field, field_value)) = child_value(key, patterns) else {
        return value.clone();
    };
    let field_entry = MapEntry {
        key: field,
        key_position: value.position.clone(),
        deferred_key: None,
        value: Value::string(field_value, value.position.clone()),
    };
    match &value.kind {
        ValueKind::Map(entries) => with_field_first(value, entries, field_entry),
        ValueKind::Call(call) if is_merge_macro(table, &call.function) => {
            let own_fields = match call.arguments.last() {
                Some(last) if !call.expand_final_argument => {
                    last.entries().map(|entries| (last, entries))
                }
                _ => None,
            };
            let arguments = match own_fields {
                Some((last, entries)) => {
                    let mut arguments = call.arguments.clone();
                    let last_index = arguments.len() - 1;
                    arguments[last_index] = with_field_first(last, entries, field_entry);
                    arguments
                }
                None => {
                    let own_fields =
                        Value::new(ValueKind::Map(vec![field_entry]), value.position.clone());
                    if call.expand_final_argument {
                        vec![value.clone(), own_fields]
                    } else {
                        call.arguments.iter().cloned().chain([own_fields]).collect()
                    }
                }
            };
            Value::new(
                ValueKind::Call(Call::new(call.function.clone(), arguments)),
                value.position.clone(),
            )
        }
        _ => value.clone(),
    }
}

/// `map`, whose entries are `entries`, with `field_entry` placed first, unless it
/// holds the field's key.
fn with_field_first(map: &Value, entries: &[MapEntry], field_entry: MapEntry) -> Value {
    if entries.iter().any(|entry| entry.key == field_entry.key) {
        return map.clone();
    }
    let entries = std::iter::once(field_entry)
        .chain(entries.iter().cloned())
        .collect();
    Value::new(ValueKind::Map(entries), map.position.clone())
}

fn is_merge_macro(table: &FunctionTable, function: &str) -> bool {
    table
        .entry(function)
        .and_then(|called| called.macro_entry.as_ref())
        .is_some_and(|macro_entry| matches!(macro_entry.kind, MacroKind::Merge))
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
                    let (key, deferred_key) = match &entry.deferred_key {
                        Some(deferred) => (
                            entry.key.clone(),
                            Some(self.resolve(deferred, path, diagnostics)),
                        ),
                        None => self.resolve_key(entry, path, diagnostics),
                    };
                    path.push(PathStep::Key(key.clone()));
                    let resolved = self.resolve(&entry.value, path, diagnostics);
                    path.pop();
                    resolved_entries.push(MapEntry {
                        key,
                        key_position: entry.key_position.clone(),
                        deferred_key,
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
                let mut resolved_parts: Vec<TemplatePart> = Vec::new();
                for part in parts {
                    let pieces = match part {
                        TemplatePart::Reference(reference) => {
                            let mut reference = reference.clone();
                            if let Target::Local(local) = &reference.target {
                                reference.target =
                                    Target::Local(Box::new(self.resolve(local, path, diagnostics)));
                            }
                            vec![TemplatePart::Reference(reference)]
                        }
                        TemplatePart::Text(text) => {
                            let mut resolve = |reading: &NameReading<'_>| self.read(reading, path);
                            self.resolution
                                .scanner
                                .scan_placeholders(
                                    text,
                                    &value.position,
                                    path,
                                    Strictness::Strict,
                                    &mut resolve,
                                )
                                .unwrap_or_else(|diagnostic| {
                                    diagnostics.push(diagnostic);
                                    vec![part.clone()]
                                })
                        }
                    };
                    for piece in pieces {
                        match (resolved_parts.last_mut(), piece) {
                            (Some(TemplatePart::Text(last)), TemplatePart::Text(text)) => {
                                last.push_str(&text);
                            }
                            (_, piece) => resolved_parts.push(piece),
                        }
                    }
                }
                Value::new(ValueKind::Template(resolved_parts), value.position.clone())
            }
            ValueKind::Loop(left) => {
                let collection = self.resolve(&left.collection, path, diagnostics);
                if let ValueKind::Null
                | ValueKind::Bool(_)
                | ValueKind::Number(_)
                | ValueKind::String(_)
                | ValueKind::Template(_) = collection.local_value().kind
                {
                    diagnostics.push(Diagnostic::at(
                        &collection.position,
                        "a loop's collection must be a list or a map",
                    ));
                }
                let body = match &left.body {
                    LoopBody::Item(item) => {
                        LoopBody::Item(Box::new(self.resolve(item, path, diagnostics)))
                    }
                    LoopBody::Entries(entries) => LoopBody::Entries(
                        entries
                            .iter()
                            .map(|entry| LoopEntry {
                                key: self.resolve(&entry.key, path, diagnostics),
                                value: self.resolve(&entry.value, path, diagnostics),
                            })
                            .collect(),
                    ),
                };
                let resolved = Loop {
                    variables: left.variables.clone(),
                    collection: Box::new(collection),
                    body,
                };
                Value::new(ValueKind::Loop(resolved), value.position.clone())
            }
            ValueKind::Null | ValueKind::Bool(_) | ValueKind::Number(_) => value.clone(),
        }
    }

    /// A key's text and, for a deferred key, its value; a key that does not resolve
    /// stays as written, as does a builtin the caller gives no value.
    fn resolve_key(
        &self,
        entry: &MapEntry,
        path: &[PathStep],
        diagnostics: &mut Vec<Diagnostic>,
    ) -> (String, Option<Value>) {
        let mut resolve = |reading: &NameReading<'_>| match self.read(reading, path) {
            Reading::Reference(Reference {
                target: Target::Builtin,
                ..
            }) => Reading::Value(Value::string(&reading.text, reading.position.clone())),
            other => other,
        };
        let scanned = self.resolution.scanner.scan_key(
            &entry.key,
            &entry.key_position,
            path,
            Strictness::Strict,
            &mut resolve,
        );
        match scanned {
            Ok(KeyReading::Resolved(key)) => (key, None),
            Ok(KeyReading::Deferred(value)) => (entry.key.clone(), Some(value)),
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                (entry.key.clone(), None)
            }
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
            None if self
                .declarations
                .declares_every_name(scope_name, scope_key, &reading.name)
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
  - path: "$.shared.*.*"
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
    fn a_local_holding_a_reference_is_read_inside_text_as_the_local() {
        let document = resolve(concat!(
            "locals:\n  basename: ref queue.base\n",
            "queue:\n  base:\n    x: 1\n  q:\n    host: \"${local.basename}.mock.${env}.com\"\n",
        ))
        .unwrap();
        let host = document
            .get("queue")
            .and_then(|queue| queue.get("q"))
            .and_then(|q| q.get("host"))
            .unwrap();
        let ValueKind::Template(parts) = &host.kind else {
            panic!("{host:?}");
        };
        let shown: Vec<String> = parts
            .iter()
            .map(|part| match part {
                TemplatePart::Text(text) => format!("text {text}"),
                TemplatePart::Reference(reference) => {
                    format!("{} {}", reference.scope, reference.name)
                }
            })
            .collect();
        assert_eq!(
            shown,
            ["local basename", "text .mock.", "GLOBAL env", "text .com"]
        );
    }

    #[test]
    fn a_local_holding_a_scalar_is_spliced_inside_text() {
        let document =
            resolve("locals:\n  suffix: a\nqueue:\n  q:\n    host: q-${local.suffix}.mock\n")
                .unwrap();
        assert_eq!(
            json(document.get("queue").unwrap()),
            serde_json::json!({"q": {"host": "q-a.mock"}})
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

    fn merge_arguments(value: &Value) -> Vec<Value> {
        match &value.kind {
            ValueKind::Call(call) if call.function == "merge" => call.arguments.clone(),
            other => panic!("not a merge call: {other:?}"),
        }
    }

    #[test]
    fn a_merge_left_for_the_target_takes_the_key_field_in_its_own_fields() {
        let resolved = resolve_layers(
            None,
            concat!(
                "locals:\n  policy fn.encode: x\n",
                "queue:\n  name mock-orders:\n    fn.merge: local.policy\n    x: 2\n",
            ),
        )
        .unwrap();
        let arguments = merge_arguments(&resolved.declarations[1].value);
        assert_eq!(arguments.len(), 2);
        assert_eq!(
            json(&arguments[1]),
            serde_json::json!({"name": "mock-orders", "x": 2})
        );
    }

    #[test]
    fn a_merge_left_for_the_target_with_no_own_fields_gains_the_key_field_as_one() {
        let resolved = resolve_layers(
            None,
            concat!(
                "locals:\n  policy fn.encode: x\n",
                "queue:\n  name mock-orders:\n    fn.merge: local.policy\n",
            ),
        )
        .unwrap();
        let arguments = merge_arguments(&resolved.declarations[1].value);
        assert_eq!(arguments.len(), 2);
        assert_eq!(
            json(&arguments[1]),
            serde_json::json!({"name": "mock-orders"})
        );
    }

    #[test]
    fn a_merge_left_for_the_target_keeps_the_key_field_its_own_fields_state() {
        let resolved = resolve_layers(
            None,
            concat!(
                "locals:\n  policy fn.encode: x\n",
                "queue:\n  name mock-orders:\n    fn.merge: local.policy\n",
                "    name: mock-orders\n    x: 2\n",
            ),
        )
        .unwrap();
        let arguments = merge_arguments(&resolved.declarations[1].value);
        assert_eq!(
            json(&arguments[1]),
            serde_json::json!({"name": "mock-orders", "x": 2})
        );
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

    fn loop_at(document: &Value, entry: &str, field: &str) -> Loop {
        let value = document
            .get("queue")
            .and_then(|queue| queue.get(entry))
            .and_then(|found| found.get(field))
            .unwrap();
        let ValueKind::Loop(left) = &value.kind else {
            panic!("{value:?}")
        };
        left.clone()
    }

    fn loop_variable(value: &Value) -> (&str, &str) {
        let ValueKind::Reference(Reference {
            name,
            path,
            target: Target::LoopVariable,
            ..
        }) = &value.kind
        else {
            panic!("{value:?}")
        };
        (name, path)
    }

    #[test]
    fn a_loop_over_a_declaration_is_left_to_the_target_its_variable_read_as_itself() {
        let document = resolve(concat!(
            "queue:\n  p:\n    a: 1\n",
            "  q:\n    labels fn.loop:\n      in: ref queue.p\n",
            "      body:\n        - ~it.label\n",
        ))
        .unwrap();
        let left = loop_at(&document, "q", "labels");
        assert_eq!(left.variables, ["it"]);
        assert!(matches!(
            &left.collection.kind,
            ValueKind::Reference(Reference {
                target: Target::Declaration { .. },
                ..
            })
        ));
        let LoopBody::Item(item) = &left.body else {
            panic!("{:?}", left.body)
        };
        assert_eq!(loop_variable(item), ("it", ".label"));
    }

    #[test]
    fn a_map_body_left_to_the_target_reads_its_variables_in_its_key_and_value() {
        let document = resolve(concat!(
            "queue:\n  p:\n    a: 1\n",
            "  q:\n    labels fn.loop:\n      for: [name, item]\n      in: ref queue.p\n",
            "      body:\n        q-${~name}: ~item\n        fixed: 1\n",
        ))
        .unwrap();
        let left = loop_at(&document, "q", "labels");
        assert_eq!(left.variables, ["name", "item"]);
        let LoopBody::Entries(entries) = &left.body else {
            panic!("{:?}", left.body)
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].key.as_str(), Some("fixed"));
        let LoopEntry { key, value } = &entries[0];
        let ValueKind::Template(parts) = &key.kind else {
            panic!("{key:?}")
        };
        assert_eq!(parts[0], TemplatePart::Text("q-".to_owned()));
        let TemplatePart::Reference(read) = &parts[1] else {
            panic!("{parts:?}")
        };
        assert_eq!(
            (read.name.as_str(), &read.target),
            ("name", &Target::LoopVariable)
        );
        assert_eq!(loop_variable(value), ("item", ""));
    }

    #[test]
    fn a_loop_over_a_call_is_left_to_the_target() {
        let document = resolve(concat!(
            "queue:\n  q:\n    labels fn.loop:\n      in:\n        fn.encode: [a]\n",
            "      body:\n        - ~it\n",
        ))
        .unwrap();
        let left = loop_at(&document, "q", "labels");
        assert!(matches!(&left.collection.kind, ValueKind::Call(_)));
    }

    #[test]
    fn a_call_as_a_map_body_key_left_to_the_target_is_an_error() {
        let errors = resolve(concat!(
            "queue:\n  p:\n    a: 1\n",
            "  q:\n    labels fn.loop:\n      in: ref queue.p\n",
            "      body:\n        tags fn.encode: [a]\n",
        ))
        .unwrap_err();
        let error = errors
            .iter()
            .find(|diagnostic| diagnostic.severity == Severity::Error)
            .unwrap();
        assert_eq!(
            error.message,
            "a loop whose collection is known only when the document is used has no call as a key of its map body"
        );
    }

    #[test]
    fn a_loop_over_text_is_an_error() {
        let errors = resolve(concat!(
            "queue:\n  q:\n    labels fn.loop:\n      in: mock-text\n",
            "      body:\n        - ~it\n",
        ))
        .unwrap_err();
        assert_eq!(
            errors[0].message,
            "a loop's collection must be a list or a map"
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

    fn map_entries<'d>(document: &'d Value, keys: &[&str]) -> &'d [MapEntry] {
        let map = keys
            .iter()
            .fold(document, |value, key| value.get(key).unwrap());
        map.entries().unwrap()
    }

    fn template_shown(value: &Value) -> Vec<String> {
        let ValueKind::Template(parts) = &value.kind else {
            panic!("{value:?}")
        };
        parts
            .iter()
            .map(|part| match part {
                TemplatePart::Text(text) => format!("text {text}"),
                TemplatePart::Reference(reference) => {
                    format!("{} {}", reference.scope, reference.name)
                }
            })
            .collect()
    }

    #[test]
    fn a_key_reading_a_declaration_whole_is_a_deferred_key() {
        let document =
            resolve("queue:\n  p:\n    a: 1\n  q:\n    tags:\n      ref queue.p: 1\n").unwrap();
        let entries = map_entries(&document, &["queue", "q", "tags"]);
        assert_eq!(entries[0].key, "ref queue.p");
        let deferred_key = entries[0].deferred_key.as_ref().unwrap();
        assert!(
            matches!(
                &deferred_key.kind,
                ValueKind::Reference(Reference {
                    target: Target::Declaration { .. },
                    ..
                })
            ),
            "{deferred_key:?}"
        );
    }

    #[test]
    fn a_deferred_key_no_declaration_answers_is_an_error() {
        let errors = resolve("queue:\n  q:\n    tags:\n      ref queue.missing: 1\n").unwrap_err();
        assert_eq!(errors[0].message, "Invalid reference: queue.missing");
        assert_eq!(
            (
                errors[0].position.as_ref().unwrap().line,
                errors[0].position.as_ref().unwrap().column
            ),
            (4, 7)
        );
    }

    #[test]
    fn a_key_holding_a_constant_placeholder_is_a_resolved_key() {
        let document =
            resolve("locals:\n  suffix: mock-a\nqueue:\n  q-${local.suffix}:\n    x: 1\n").unwrap();
        let entries = map_entries(&document, &["queue"]);
        assert_eq!(entries[0].key, "q-mock-a");
        assert_eq!(entries[0].deferred_key, None);
    }

    #[test]
    fn a_key_holding_a_placeholder_with_no_constant_is_a_template_deferred_key() {
        let document = resolve(concat!(
            "locals:\n  basename: ref queue.p\n",
            "queue:\n  p:\n    a: 1\n",
            "  q:\n    tags:\n      q-${env}: 1\n      q-${local.basename}: 2\n",
        ))
        .unwrap();
        let entries = map_entries(&document, &["queue", "q", "tags"]);
        assert_eq!(entries[0].key, "q-${env}");
        assert_eq!(entries[0].deferred_key, None);
        assert_eq!(entries[1].key, "q-${local.basename}");
        assert_eq!(
            template_shown(entries[1].deferred_key.as_ref().unwrap()),
            ["text q-", "local basename"]
        );
    }

    #[test]
    fn a_declaration_path_holding_a_deferred_key_declares_nothing() {
        let resolved = resolve_layers(
            None,
            concat!(
                "locals:\n  basename: ref queue.p\n",
                "queue:\n  p:\n    a: 1\n  ref queue.p:\n    x: 1\n",
                "  q-${local.basename}:\n    x: 2\n  q-${env}:\n    x: 3\n",
            ),
        )
        .unwrap();
        let names: Vec<(&str, &str)> = resolved
            .declarations
            .iter()
            .map(|declared| (declared.scope.as_str(), declared.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [("local", "basename"), ("ENTRY", "p"), ("ENTRY", "q-${env}")]
        );
    }

    /// The caller's value of every builtin: `mock-dev` for `env`.
    struct MockBuiltins;

    impl BuiltinValues for MockBuiltins {
        fn value(&self, _unit: &Unit, _scope: &str, name: &str) -> Option<ValueKind> {
            (name == "env").then(|| ValueKind::String("mock-dev".to_owned()))
        }
    }

    #[test]
    fn a_key_reading_a_builtin_the_caller_gives_is_a_resolved_key_spliced() {
        let source = MemorySource::new()
            .with_file("/root/dev/mock-dsl.yml", "queue:\n  q-${env}:\n    x: 1\n");
        let resolution = Resolution::new(definition(), PathBuf::from("/root")).unwrap();
        let unit = Unit {
            id: "dev/mock-dsl.yml".into(),
            file: PathBuf::from("/root/dev/mock-dsl.yml"),
            directory: PathBuf::from("/root/dev"),
            overlay: None,
            common_layer: None,
            holds_no_documents: false,
        };
        let options = ResolveOptions {
            builtins: Some(&MockBuiltins),
            schema_provider: Some(&MockSchema),
        };
        let resolved = resolution.resolve(&unit, &source, &options).unwrap();
        let entries = map_entries(&resolved.document, &["queue"]);
        assert_eq!(entries[0].key, "q-mock-dev");
        assert_eq!(entries[0].deferred_key, None);
        assert_eq!(resolved.declarations[0].name, "q-mock-dev");
    }

    const LOOP_KEY_DOCUMENT: &str = concat!(
        "source:\n  p:\n    a: 1\n",
        "queue:\n  fn.loop:\n    for: [name, item]\n    in: ref source.p\n",
        "    body:\n      q_${~name}:\n        x: ~item\n",
    );

    fn reference_target(document: &Value, keys: &[&str]) -> Target {
        let value = keys
            .iter()
            .fold(document, |value, key| value.get(key).unwrap());
        let ValueKind::Reference(reference) = &value.kind else {
            panic!("{value:?}")
        };
        reference.target.clone()
    }

    #[test]
    fn a_loop_key_reading_a_loop_variable_declares_every_name_it_can_spell() {
        let document = resolve(&format!(
            "{LOOP_KEY_DOCUMENT}topic:\n  t:\n    x: ref queue.q_mock\n"
        ))
        .unwrap();
        assert_eq!(
            reference_target(&document, &["topic", "t", "x"]),
            Target::Undeclared
        );
        let errors = resolve(&format!(
            "{LOOP_KEY_DOCUMENT}topic:\n  t:\n    x: ref queue.r_mock\n"
        ))
        .unwrap_err();
        assert_eq!(errors[0].message, "Invalid reference: queue.r_mock");
    }

    #[test]
    fn a_name_declared_explicitly_wins_over_a_loop_key() {
        let document = resolve(&format!(
            "{LOOP_KEY_DOCUMENT}{}",
            concat!(
                "shared:\n  queue:\n    q_mock:\n      x: 1\n",
                "topic:\n  t:\n    x: ref queue.q_mock\n    y: ref queue.q_other\n",
            )
        ))
        .unwrap();
        assert!(matches!(
            reference_target(&document, &["topic", "t", "x"]),
            Target::Declaration { .. }
        ));
        assert_eq!(
            reference_target(&document, &["topic", "t", "y"]),
            Target::Undeclared
        );
    }

    #[test]
    fn a_key_reading_a_loop_variable_in_a_body_left_to_the_target_is_a_deferred_key() {
        let document = resolve(concat!(
            "queue:\n  p:\n    a: 1\n",
            "  q:\n    labels fn.loop:\n      for: [name, item]\n      in: ref queue.p\n",
            "      body:\n        - tags:\n            q-${~name}: 1\n            ~item: 2\n",
        ))
        .unwrap();
        let left = loop_at(&document, "q", "labels");
        let LoopBody::Item(item) = &left.body else {
            panic!("{:?}", left.body)
        };
        let entries = map_entries(item, &["tags"]);
        assert_eq!(
            template_shown(entries[0].deferred_key.as_ref().unwrap()),
            ["text q-", "LOOP name"]
        );
        assert_eq!(
            loop_variable(entries[1].deferred_key.as_ref().unwrap()),
            ("item", "")
        );
    }

    #[test]
    fn a_call_named_by_a_deferred_key_is_an_error() {
        let errors = resolve(concat!(
            "queue:\n  p:\n    a: 1\n",
            "  q:\n    labels fn.loop:\n      in: ref queue.p\n",
            "      body:\n        - tags:\n            ${~it} fn.encode: x\n",
        ))
        .unwrap_err();
        assert_eq!(
            errors[0].message,
            "the call `${~it} fn.encode` is named by a deferred key; a call's name must be a resolved key"
        );
    }

    /// The test schema, found with a warning.
    struct WarningSchema;

    impl SchemaProvider for WarningSchema {
        fn schema_of(&self, unit: &Unit, text: &str) -> Result<DocumentSchema, Diagnostic> {
            let found = MockSchema.schema_of(unit, text)?;
            Ok(DocumentSchema {
                warnings: vec!["mock schema warning".to_owned()],
                ..found
            })
        }
    }

    #[test]
    fn what_finding_the_schema_warns_of_is_a_warning() {
        let source = MemorySource::new().with_file("/root/dev/mock-dsl.yml", "queue: {}\n");
        let resolution = Resolution::new(definition(), PathBuf::from("/root")).unwrap();
        let unit = Unit {
            id: "dev/mock-dsl.yml".into(),
            file: PathBuf::from("/root/dev/mock-dsl.yml"),
            directory: PathBuf::from("/root/dev"),
            overlay: None,
            common_layer: None,
            holds_no_documents: false,
        };
        let options = ResolveOptions {
            builtins: None,
            schema_provider: Some(&WarningSchema),
        };
        let resolved = resolution.resolve(&unit, &source, &options).unwrap();
        let shown: Vec<(Severity, &str)> = resolved
            .warnings
            .iter()
            .map(|warning| (warning.severity, warning.message.as_str()))
            .collect();
        assert_eq!(shown, [(Severity::Warning, "mock schema warning")]);
    }

    const CONSTANT_BODY_KEY_WARNING: &str = concat!(
        "the loop body's key `mock_queue` reads no loop variable, ",
        "so every pass writes the same key",
    );

    #[test]
    fn a_map_body_key_reading_no_loop_variable_is_a_warning_and_declares_nothing() {
        let resolved = resolve_layers(
            None,
            concat!(
                "locals:\n  names: [mock_a]\n",
                "queue:\n  fn.loop:\n    in: local.names\n",
                "    body:\n      mock_queue:\n        x: 1\n      q_${~it}:\n        x: 2\n",
            ),
        )
        .unwrap();
        let shown: Vec<(Severity, &str, usize, usize)> = resolved
            .warnings
            .iter()
            .map(|warning| {
                let position = warning.position.as_ref().unwrap();
                (
                    warning.severity,
                    warning.message.as_str(),
                    position.line,
                    position.column,
                )
            })
            .collect();
        assert_eq!(
            shown,
            [(Severity::Warning, CONSTANT_BODY_KEY_WARNING, 7, 7)]
        );
        let names: Vec<(&str, &str)> = resolved
            .declarations
            .iter()
            .map(|declared| (declared.scope.as_str(), declared.name.as_str()))
            .collect();
        assert_eq!(names, [("local", "names"), ("ENTRY", "q_mock_a")]);
    }

    #[test]
    fn a_constant_map_body_key_is_reported_once_however_many_passes_write_it() {
        let errors = resolve(concat!(
            "locals:\n  names: [mock_a, mock_b]\n",
            "queue:\n  fn.loop:\n    in: local.names\n",
            "    body:\n      mock_queue:\n        x: 1\n",
        ))
        .unwrap_err();
        let warnings: Vec<&str> = errors
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Warning)
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();
        assert_eq!(warnings, [CONSTANT_BODY_KEY_WARNING]);
    }

    #[test]
    fn a_constant_key_of_a_map_body_left_to_the_target_is_a_warning_and_declares_nothing() {
        let resolved = resolve_layers(
            None,
            concat!(
                "source:\n  p:\n    a: 1\n",
                "queue:\n  fn.loop:\n    in: ref source.p\n",
                "    body:\n      mock_queue:\n        x: 1\n",
            ),
        )
        .unwrap();
        let shown: Vec<&str> = resolved
            .warnings
            .iter()
            .map(|warning| warning.message.as_str())
            .collect();
        assert_eq!(shown, [CONSTANT_BODY_KEY_WARNING]);
        let names: Vec<(&str, &str)> = resolved
            .declarations
            .iter()
            .map(|declared| (declared.scope.as_str(), declared.name.as_str()))
            .collect();
        assert_eq!(names, [("ENTRY", "p")]);
    }
}
