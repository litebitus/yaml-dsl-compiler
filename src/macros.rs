//! Expands macros and binds locals and loop variables: the document as it will be
//! used, with the names of every other scope left for resolution.

use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::definition::{DslDefinition, MarkerFunction};
use crate::diagnostic::Diagnostic;
use crate::function_table::{
    CallKey, FunctionEntry, FunctionTable, MacroEntry, MacroKind, MergeBehavior, parse_call_key,
};
use crate::json_path::{PathSet, PathStep};
use crate::position::Position;
use crate::reference_scan::{NameReading, Reading, Scanner, Strictness};
use crate::resolved::{Call, MapEntry, Reference, Target, Value, ValueKind};

type Bindings = Vec<(String, Value)>;

pub struct Expander<'a> {
    scanner: &'a Scanner,
    table: &'a FunctionTable,
    marker: Option<&'a MarkerFunction>,
    calls_not_allowed: PathSet,
    local_scope: Option<String>,
    loop_scope: Option<String>,
    path_separator: Option<String>,
    /// Each local's entry as written, with the path of the map holding it.
    raw_locals: BTreeMap<String, (Vec<PathStep>, MapEntry)>,
    local_values: RefCell<BTreeMap<String, Value>>,
    resolving: RefCell<Vec<String>>,
}

/// The maps a merge macro merges, in order; when they are a list not known when the
/// document compiles, that list stands alone and the target expands it.
struct MergeContents {
    contents: Vec<Value>,
    expand_final_argument: bool,
}

/// What a call written in a key expands to.
enum CallYield<'t> {
    /// A value under the call's name, or standing for its map when it has no name.
    Value(Value),
    /// A loop's entries: under the call's name, or added to its map when it has none.
    Entries(Vec<MapEntry>),
    /// A merge, called by a whole key, merged under its map's own entries once they
    /// are read.
    Merge(&'t MacroEntry, MergeContents),
    Nothing,
}

/// What a loop yields: entries for a map body, items for a list body.
enum LoopYield {
    Entries(Vec<MapEntry>),
    Items(Vec<Value>),
}

impl<'a> Expander<'a> {
    pub fn new(
        definition: &'a DslDefinition,
        scanner: &'a Scanner,
        table: &'a FunctionTable,
        raw_locals: BTreeMap<String, (Vec<PathStep>, MapEntry)>,
    ) -> Result<Self, Diagnostic> {
        let function = definition.function.as_ref();
        let calls_not_allowed = match function {
            Some(function) => PathSet::parse(
                function
                    .calls_not_allowed_at
                    .iter()
                    .map(|entry| (entry.path.as_str(), entry.skip_keys.as_slice())),
            )?,
            None => PathSet::default(),
        };
        Ok(Self {
            scanner,
            table,
            marker: function.and_then(|function| function.marker_function.as_ref()),
            calls_not_allowed,
            local_scope: definition
                .locals
                .as_ref()
                .map(|locals| locals.scope_name.clone()),
            loop_scope: definition
                .loops
                .as_ref()
                .map(|loops| loops.scope_name.clone()),
            path_separator: definition
                .locals
                .as_ref()
                .and_then(|locals| locals.path_separator.clone()),
            raw_locals,
            local_values: RefCell::new(BTreeMap::new()),
            resolving: RefCell::new(Vec::new()),
        })
    }

    pub fn expand_document(&self, document: &Value, diagnostics: &mut Vec<Diagnostic>) -> Value {
        self.expand(document, &mut Vec::new(), &Vec::new(), diagnostics)
    }

    fn local_value(&self, name: &str) -> Result<Value, Diagnostic> {
        if let Some(value) = self.local_values.borrow().get(name) {
            return Ok(value.clone());
        }
        let (parent_path, entry) = self
            .raw_locals
            .get(name)
            .ok_or_else(|| Diagnostic::unplaced(format!("no local `{name}`")))?;
        if self
            .resolving
            .borrow()
            .iter()
            .any(|resolving| resolving == name)
        {
            return Err(Diagnostic::at(
                &entry.key_position,
                format!("local `{name}` reaches itself through its references"),
            ));
        }
        self.resolving.borrow_mut().push(name.to_owned());
        let mut diagnostics = Vec::new();
        let holder = Value::new(
            ValueKind::Map(vec![entry.clone()]),
            entry.key_position.clone(),
        );
        let expanded = self.expand(
            &holder,
            &mut parent_path.clone(),
            &Vec::new(),
            &mut diagnostics,
        );
        self.resolving.borrow_mut().pop();
        if let Some(diagnostic) = diagnostics.into_iter().next() {
            return Err(diagnostic);
        }
        let value = match expanded.entries() {
            Some([only]) => only.value.clone(),
            _ => {
                return Err(Diagnostic::at(
                    &entry.key_position,
                    format!("local `{name}` does not expand to one value"),
                ));
            }
        };
        self.local_values
            .borrow_mut()
            .insert(name.to_owned(), value.clone());
        Ok(value)
    }

    fn expand(
        &self,
        value: &Value,
        path: &mut Vec<PathStep>,
        bindings: &Bindings,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Value {
        match &value.kind {
            ValueKind::String(text) => {
                let mut resolve = |reading: &NameReading<'_>| self.read(reading, bindings);
                match self.scanner.scan(
                    text,
                    &value.position,
                    path,
                    Strictness::Lenient,
                    &mut resolve,
                ) {
                    Ok(scanned) => scanned,
                    Err(diagnostic) => {
                        diagnostics.push(diagnostic);
                        value.clone()
                    }
                }
            }
            ValueKind::List(items) => {
                let items = items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        path.push(PathStep::Index(index));
                        let expanded = self.expand(item, path, bindings, diagnostics);
                        path.pop();
                        expanded
                    })
                    .collect();
                Value::new(ValueKind::List(items), value.position.clone())
            }
            ValueKind::Map(entries) => {
                self.expand_map(entries, &value.position, path, bindings, diagnostics)
            }
            _ => value.clone(),
        }
    }

    /// A local's reading outside any loop, such as in a key read before the fold.
    pub fn read_local(&self, reading: &NameReading<'_>) -> Reading {
        if self.local_scope.as_deref() != Some(reading.rule.scope_name.as_str()) {
            return Reading::Keep;
        }
        self.read(reading, &Vec::new())
    }

    /// The resolver of this phase: loop variables and locals; every other scope is
    /// left for resolution.
    fn read(&self, reading: &NameReading<'_>, bindings: &Bindings) -> Reading {
        let scope = reading.rule.scope_name.as_str();
        if self.loop_scope.as_deref() == Some(scope) {
            let Some((_, bound)) = bindings
                .iter()
                .rev()
                .find(|(name, _)| *name == reading.name)
            else {
                return Reading::Keep;
            };
            return match step_into(
                bound,
                &reading.path,
                self.path_separator.as_deref().unwrap_or("."),
            ) {
                Ok(Some(value)) => Reading::Value(value),
                Ok(None) => Reading::Error(format!(
                    "`{}` reaches into a value known only when the document is used",
                    reading.text
                )),
                Err(message) => Reading::Error(format!("`{}`: {message}", reading.text)),
            };
        }
        if self.local_scope.as_deref() == Some(scope) {
            if !self.raw_locals.contains_key(&reading.name) {
                return Reading::Keep;
            }
            let value = match self.local_value(&reading.name) {
                Ok(value) => value,
                Err(diagnostic) => return Reading::Error(diagnostic.message),
            };
            let stepped = match self.path_separator.as_deref() {
                Some(separator) => match step_into(&value, &reading.path, separator) {
                    Ok(stepped) => stepped,
                    Err(message) => {
                        return Reading::Error(format!("`{}`: {message}", reading.text));
                    }
                },
                None => Some(value.clone()),
            };
            let reference = |target: Target| Reference {
                scope: scope.to_owned(),
                scope_key: None,
                name: reading.name.clone(),
                path: reading.path.clone(),
                text: reading.text.clone(),
                target,
            };
            return match stepped {
                Some(stepped) if !reading.whole => Reading::Value(stepped),
                Some(stepped) => Reading::Reference(reference(Target::Local(Box::new(stepped)))),
                None => Reading::Reference(reference(Target::LocalUnknown)),
            };
        }
        Reading::Keep
    }

    fn expand_map(
        &self,
        entries: &[MapEntry],
        position: &Position,
        path: &mut Vec<PathStep>,
        bindings: &Bindings,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Value {
        let mut own: Vec<MapEntry> = Vec::new();
        let mut nameless_merges: Vec<(String, &MacroEntry, MergeContents, Position)> = Vec::new();
        let mut standing_value: Option<Value> = None;
        for entry in entries {
            let mut resolve = |reading: &NameReading<'_>| self.read(reading, bindings);
            let key = match self.scanner.scan_text(
                &entry.key,
                &entry.key_position,
                path,
                Strictness::Lenient,
                &mut resolve,
            ) {
                Ok(key) => key,
                Err(diagnostic) => {
                    diagnostics.push(diagnostic);
                    entry.key.clone()
                }
            };
            path.push(PathStep::Key(key.clone()));
            let call = self.marker.and_then(|marker| parse_call_key(&key, marker));
            match call {
                None => {
                    let value = self.expand(&entry.value, path, bindings, diagnostics);
                    push_entry(
                        &mut own,
                        key,
                        entry.key_position.clone(),
                        value,
                        diagnostics,
                    );
                }
                Some(call) => {
                    let key_position = entry.key_position.clone();
                    if self.calls_not_allowed.selects(path) {
                        let message = format!("a call is not allowed at `{key}`");
                        diagnostics.push(Diagnostic::at(&key_position, message));
                    }
                    let nameless_refused = self
                        .marker
                        .is_some_and(|marker| !marker.calls_without_name_allowed);
                    if call.name.is_none() && nameless_refused {
                        let message = "this DSL has no calls without a name";
                        diagnostics.push(Diagnostic::at(&key_position, message));
                    }
                    let value_position = entry.value.position.clone();
                    match (
                        self.expand_call(&call, entry, path, bindings, diagnostics),
                        call.name,
                    ) {
                        (CallYield::Value(value), Some(name)) => {
                            push_entry(&mut own, name, key_position, value, diagnostics);
                        }
                        (CallYield::Value(value), None) => standing_value = Some(value),
                        (CallYield::Entries(yielded), Some(name)) => {
                            let map = Value::new(ValueKind::Map(yielded), value_position);
                            push_entry(&mut own, name, key_position, map, diagnostics);
                        }
                        (CallYield::Entries(yielded), None) => {
                            for yielded_entry in yielded {
                                let MapEntry {
                                    key,
                                    key_position,
                                    value,
                                } = yielded_entry;
                                push_entry(&mut own, key, key_position, value, diagnostics);
                            }
                        }
                        (CallYield::Merge(macro_entry, contents), _) => {
                            let function = call.function.clone();
                            nameless_merges.push((function, macro_entry, contents, key_position));
                        }
                        (CallYield::Nothing, _) => {}
                    }
                }
            }
            path.pop();
        }
        if let Some(value) = standing_value {
            if !own.is_empty() || !nameless_merges.is_empty() || entries.len() > 1 {
                diagnostics.push(Diagnostic::at(
                    position,
                    "a call with no name is the only key of its map",
                ));
            }
            return value;
        }
        let mut merged_own = own;
        for (function, macro_entry, contents, key_position) in nameless_merges {
            let base = merge_values(
                &function,
                macro_entry,
                contents,
                merged_own.clone(),
                &key_position,
                diagnostics,
            );
            match base.kind {
                ValueKind::Map(entries) => merged_own = entries,
                _ => return base,
            }
        }
        Value::new(ValueKind::Map(merged_own), position.clone())
    }

    fn expand_call(
        &self,
        call: &CallKey,
        entry: &MapEntry,
        path: &mut Vec<PathStep>,
        bindings: &Bindings,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> CallYield<'a> {
        let table: &'a FunctionTable = self.table;
        let macro_call = table
            .entry(&call.function)
            .and_then(|called| Some((called, called.macro_entry.as_ref()?)));
        let Some((called, macro_entry)) = macro_call else {
            let value = self.expand(&entry.value, path, bindings, diagnostics);
            let target = target_call(&call.function, value, call.splat);
            return CallYield::Value(Value::new(
                ValueKind::Call(target),
                entry.value.position.clone(),
            ));
        };
        match macro_entry.kind {
            MacroKind::Merge if call.name.is_some() => {
                let message = format!(
                    "merge macro `{}` is called by a whole key, with no name",
                    call.function
                );
                diagnostics.push(Diagnostic::at(&entry.key_position, message));
                CallYield::Nothing
            }
            MacroKind::Merge => CallYield::Merge(
                macro_entry,
                self.merge_contents(
                    called,
                    macro_entry,
                    call,
                    entry,
                    path,
                    bindings,
                    diagnostics,
                ),
            ),
            MacroKind::Loop => {
                match self.expand_loop(called, macro_entry, entry, path, bindings, diagnostics) {
                    Some(LoopYield::Entries(entries)) => CallYield::Entries(entries),
                    Some(LoopYield::Items(items)) => CallYield::Value(Value::new(
                        ValueKind::List(items),
                        entry.value.position.clone(),
                    )),
                    None => CallYield::Nothing,
                }
            }
            MacroKind::FunctionCall => CallYield::Value(self.function_call_macro(
                called,
                macro_entry,
                entry,
                path,
                bindings,
                diagnostics,
            )),
            MacroKind::Transform => {
                let message = format!(
                    "transform macro `{}` cannot be expanded when the document compiles",
                    call.function
                );
                diagnostics.push(Diagnostic::at(&entry.key_position, message));
                CallYield::Nothing
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn merge_contents(
        &self,
        called: &FunctionEntry,
        macro_entry: &MacroEntry,
        call: &CallKey,
        entry: &MapEntry,
        path: &mut Vec<PathStep>,
        bindings: &Bindings,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> MergeContents {
        let value = self.expand(&entry.value, path, bindings, diagnostics);
        let content_name = macro_entry.role("content").unwrap_or("");
        let repeated = called
            .arguments
            .iter()
            .any(|argument| argument.name == content_name && argument.repeated);
        let bound = bind_arguments(&value, called)
            .into_iter()
            .find(|(name, _)| name == content_name)
            .map(|(_, value)| value);
        let (contents, expand_final_argument) = match bound {
            Some(content) if call.splat => splat_arguments(content, true),
            Some(Value {
                kind: ValueKind::List(items),
                ..
            }) if repeated => (items, false),
            Some(content) => (vec![content], false),
            None => (Vec::new(), false),
        };
        MergeContents {
            contents,
            expand_final_argument,
        }
    }

    fn expand_loop(
        &self,
        called: &FunctionEntry,
        macro_entry: &MacroEntry,
        entry: &MapEntry,
        path: &mut Vec<PathStep>,
        bindings: &Bindings,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Option<LoopYield> {
        let arguments = bind_arguments(&entry.value, called);
        let argument = |role: &str| {
            macro_entry.role(role).and_then(|name| {
                arguments
                    .iter()
                    .find(|(argument, _)| argument == name)
                    .map(|(_, value)| value.clone())
            })
        };
        let Some(collection) = argument("collection") else {
            diagnostics.push(Diagnostic::at(
                &entry.key_position,
                "a loop has no collection",
            ));
            return None;
        };
        let Some(body) = argument("body") else {
            diagnostics.push(Diagnostic::at(&entry.key_position, "a loop has no body"));
            return None;
        };
        let variables_value = argument("loop_variables").or_else(|| {
            let name = macro_entry.role("loop_variables")?;
            let default = called
                .arguments
                .iter()
                .find(|argument| argument.name == name)?
                .default
                .as_ref()?;
            Some(json_value(default, &entry.key_position))
        });
        let variables: Vec<String> =
            match variables_value.map(|value| self.expand(&value, path, bindings, diagnostics)) {
                Some(Value {
                    kind: ValueKind::String(name),
                    ..
                }) => vec![name],
                Some(Value {
                    kind: ValueKind::List(items),
                    ..
                }) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                _ => Vec::new(),
            };
        if variables.is_empty() || variables.len() > 2 {
            diagnostics.push(Diagnostic::at(
                &entry.key_position,
                "a loop takes one or two loop variables",
            ));
            return None;
        }
        let collection = self.expand(&collection, path, bindings, diagnostics);
        let items: Vec<(Value, Value)> = match &collection.local_value().kind {
            ValueKind::List(items) => items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    (
                        Value::new(ValueKind::Number(index.to_string()), item.position.clone()),
                        item.clone(),
                    )
                })
                .collect(),
            ValueKind::Map(entries) => {
                let mut sorted = entries.clone();
                sorted.sort_by(|first, second| first.key.cmp(&second.key));
                sorted
                    .into_iter()
                    .map(|entry| (Value::string(entry.key, entry.key_position), entry.value))
                    .collect()
            }
            _ => {
                diagnostics.push(Diagnostic::at(
                    &collection.position,
                    "a loop's collection must be a list or a map known when the document compiles",
                ));
                return None;
            }
        };
        let mut yielded_entries = Vec::new();
        let mut yielded_items = Vec::new();
        let list_body = match &body.kind {
            ValueKind::List(items) if items.len() == 1 => Some(items[0].clone()),
            ValueKind::List(_) => {
                diagnostics.push(Diagnostic::at(
                    &body.position,
                    "a loop's list body holds one element",
                ));
                return None;
            }
            _ => None,
        };
        for (key, item) in items {
            let mut item_bindings = bindings.clone();
            match variables.as_slice() {
                [one] => item_bindings.push((one.clone(), item)),
                [first, second] => {
                    item_bindings.push((first.clone(), key));
                    item_bindings.push((second.clone(), item));
                }
                _ => unreachable!("one or two loop variables"),
            }
            match &list_body {
                Some(element) => {
                    yielded_items.push(self.expand(element, path, &item_bindings, diagnostics))
                }
                None => match self.expand(&body, path, &item_bindings, diagnostics).kind {
                    ValueKind::Map(entries) => yielded_entries.extend(entries),
                    _ => {
                        diagnostics.push(Diagnostic::at(
                            &body.position,
                            "a loop's body is a map or a list of one element",
                        ));
                        return None;
                    }
                },
            }
        }
        Some(if list_body.is_some() {
            LoopYield::Items(yielded_items)
        } else {
            LoopYield::Entries(yielded_entries)
        })
    }

    fn function_call_macro(
        &self,
        called: &FunctionEntry,
        macro_entry: &MacroEntry,
        entry: &MapEntry,
        path: &mut Vec<PathStep>,
        bindings: &Bindings,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Value {
        let value = self.expand(&entry.value, path, bindings, diagnostics);
        let arguments = bind_arguments(&value, called);
        let argument = |role: &str| {
            macro_entry.role(role).and_then(|name| {
                arguments
                    .iter()
                    .find(|(argument, _)| argument == name)
                    .map(|(_, value)| value.clone())
            })
        };
        let Some(callee) = argument("callee").and_then(|callee| callee.as_str().map(str::to_owned))
        else {
            diagnostics.push(Diagnostic::at(
                &entry.key_position,
                "a function call names no callee",
            ));
            return value;
        };
        if self
            .table
            .entry(&callee)
            .is_some_and(|callee_entry| callee_entry.macro_entry.is_some())
        {
            diagnostics.push(Diagnostic::at(
                &entry.key_position,
                format!("calling macro `{callee}` through a function call is not supported yet"),
            ));
            return value;
        }
        let callee_arguments: Vec<Value> = match argument("callee_arguments") {
            Some(Value {
                kind: ValueKind::Map(entries),
                ..
            }) => match self.table.entry(&callee) {
                Some(callee_entry) => callee_entry
                    .arguments
                    .iter()
                    .filter_map(|parameter| {
                        entries
                            .iter()
                            .find(|entry| entry.key == parameter.name)
                            .map(|entry| entry.value.clone())
                    })
                    .collect(),
                None => entries.into_iter().map(|entry| entry.value).collect(),
            },
            Some(other) => vec![other],
            None => Vec::new(),
        };
        Value::new(
            ValueKind::Call(Call::new(callee, callee_arguments)),
            entry.value.position.clone(),
        )
    }
}

fn push_entry(
    own: &mut Vec<MapEntry>,
    key: String,
    key_position: Position,
    value: Value,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if own.iter().any(|entry| entry.key == key) {
        diagnostics.push(Diagnostic::at(
            &key_position,
            format!("duplicate key `{key}`"),
        ));
        return;
    }
    own.push(MapEntry {
        key,
        key_position,
        value,
    });
}

/// A call left for the target: a splat call takes a list's items as its arguments,
/// any other call takes the value whole.
fn target_call(function: &str, value: Value, splat: bool) -> Call {
    let (arguments, expand_final_argument) = splat_arguments(value, splat);
    Call {
        function: function.to_owned(),
        arguments,
        expand_final_argument,
    }
}

/// A splat's list items, or the value whole, with whether the target expands it.
fn splat_arguments(value: Value, splat: bool) -> (Vec<Value>, bool) {
    if !splat {
        return (vec![value], false);
    }
    match &value.local_value().kind {
        ValueKind::List(items) => (items.clone(), false),
        _ => (vec![value], true),
    }
}

/// Binds a call's value to the entry's arguments: by name when it is a map holding
/// argument names, by position when it is a list and the entry takes several, and
/// otherwise as the first argument.
fn bind_arguments(value: &Value, called: &FunctionEntry) -> Vec<(String, Value)> {
    let names: Vec<&str> = called
        .arguments
        .iter()
        .map(|argument| argument.name.as_str())
        .collect();
    match &value.kind {
        ValueKind::Map(entries)
            if entries
                .iter()
                .any(|entry| names.contains(&entry.key.as_str())) =>
        {
            entries
                .iter()
                .filter(|entry| names.contains(&entry.key.as_str()))
                .map(|entry| (entry.key.clone(), entry.value.clone()))
                .collect()
        }
        ValueKind::List(items) if names.len() > 1 => names
            .iter()
            .zip(items)
            .map(|(name, item)| ((*name).to_owned(), item.clone()))
            .collect(),
        _ => names
            .first()
            .map(|name| vec![((*name).to_owned(), value.clone())])
            .unwrap_or_default(),
    }
}

/// Merges a merge macro's contents in order, then `own` over them. A content not
/// known when the document compiles leaves the merge to the target, as a call.
fn merge_values(
    function: &str,
    macro_entry: &MacroEntry,
    merge_contents: MergeContents,
    own: Vec<MapEntry>,
    position: &Position,
    diagnostics: &mut Vec<Diagnostic>,
) -> Value {
    let MergeContents {
        contents,
        expand_final_argument,
    } = merge_contents;
    let own = (!own.is_empty()).then_some(own);
    let known = !expand_final_argument
        && contents
            .iter()
            .all(|content| content.local_value().entries().is_some());
    if !known {
        let merged = Call {
            function: function.to_owned(),
            arguments: contents,
            expand_final_argument,
        };
        let Some(own) = own else {
            return Value::new(ValueKind::Call(merged), position.clone());
        };
        let own = Value::new(ValueKind::Map(own), position.clone());
        let arguments = if expand_final_argument {
            vec![Value::new(ValueKind::Call(merged), position.clone()), own]
        } else {
            merged.arguments.into_iter().chain([own]).collect()
        };
        return Value::new(
            ValueKind::Call(Call::new(function, arguments)),
            position.clone(),
        );
    }
    let mut merged: Vec<MapEntry> = Vec::new();
    let layers = contents
        .iter()
        .map(|content| {
            content
                .local_value()
                .entries()
                .expect("checked known")
                .to_vec()
        })
        .chain(own);
    for layer in layers {
        merged = merge_pair(merged, layer, macro_entry.merge_behavior, diagnostics);
    }
    Value::new(ValueKind::Map(merged), position.clone())
}

fn merge_pair(
    base: Vec<MapEntry>,
    over: Vec<MapEntry>,
    behavior: MergeBehavior,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<MapEntry> {
    let mut merged = base;
    for entry in over {
        match merged.iter().position(|existing| existing.key == entry.key) {
            None => merged.push(entry),
            Some(index) => match behavior {
                MergeBehavior::Shallow => merged[index] = entry,
                MergeBehavior::Join => diagnostics.push(Diagnostic::at(
                    &entry.key_position,
                    format!("`{}` is set twice in a join", entry.key),
                )),
                MergeBehavior::Deep => {
                    let combined = match (
                        merged[index].value.local_value().entries(),
                        entry.value.local_value().entries(),
                    ) {
                        (Some(first), Some(second)) => {
                            let entries =
                                merge_pair(first.to_vec(), second.to_vec(), behavior, diagnostics);
                            MapEntry {
                                value: Value::new(
                                    ValueKind::Map(entries),
                                    entry.value.position.clone(),
                                ),
                                ..entry
                            }
                        }
                        _ => entry,
                    };
                    merged[index] = combined;
                }
            },
        }
    }
    merged
}

/// Steps into `value` along `path`, the text after a name: map keys, or list
/// indexes counted from 0, each after `separator`.
/// The value at `path`, or `None` where the path goes into a value known only
/// when the document is used, such as a call's result.
fn step_into(value: &Value, path: &str, separator: &str) -> Result<Option<Value>, String> {
    if path.is_empty() {
        return Ok(Some(value.clone()));
    }
    let rest = path
        .strip_prefix(separator)
        .ok_or_else(|| format!("`{path}` is not a path into the value"))?;
    let mut current = value.local_value().clone();
    for step in rest.split(separator) {
        let next = match &current.kind {
            ValueKind::Map(entries) => entries
                .iter()
                .find(|entry| entry.key == step)
                .map(|entry| entry.value.clone()),
            ValueKind::List(items) => step
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get(index).cloned()),
            ValueKind::Call(_) | ValueKind::Reference(_) | ValueKind::Template(_) => {
                return Ok(None);
            }
            _ => None,
        };
        current = next
            .ok_or_else(|| format!("holds no `{step}`"))?
            .local_value()
            .clone();
    }
    Ok(Some(current))
}

/// A table default, as a value placed at the call.
fn json_value(json: &serde_json::Value, position: &Position) -> Value {
    let kind = match json {
        serde_json::Value::Null => ValueKind::Null,
        serde_json::Value::Bool(value) => ValueKind::Bool(*value),
        serde_json::Value::Number(number) => ValueKind::Number(number.to_string()),
        serde_json::Value::String(text) => ValueKind::String(text.clone()),
        serde_json::Value::Array(items) => ValueKind::List(
            items
                .iter()
                .map(|item| json_value(item, position))
                .collect(),
        ),
        serde_json::Value::Object(map) => ValueKind::Map(
            map.iter()
                .map(|(key, value)| MapEntry {
                    key: key.clone(),
                    key_position: position.clone(),
                    value: json_value(value, position),
                })
                .collect(),
        ),
    };
    Value::new(kind, position.clone())
}
