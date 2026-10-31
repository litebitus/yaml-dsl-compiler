//! The resolved document: what the compiler hands a translator.

use crate::json_path::PathStep;
use crate::position::Position;

/// A name the resolved document declares, by one of the definition's declaration
/// rules.
#[derive(Clone, Debug, PartialEq)]
pub struct Declared {
    pub scope: String,
    /// For a scope named by its parent key, that key, such as the type a resource
    /// is declared under.
    pub scope_key: Option<String>,
    /// The name as the rule spells it.
    pub name: String,
    /// The key that declares the name, as written.
    pub key: String,
    pub path: Vec<PathStep>,
    pub key_position: Position,
    /// The declared value, with the fields its key sets by the validator's
    /// `child_values_from_parent_key`.
    pub value: Value,
}

/// A value of the folded, expanded document, with where it was written.
#[derive(Clone, Debug, PartialEq)]
pub struct Value {
    pub kind: ValueKind,
    pub position: Position,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ValueKind {
    Null,
    Bool(bool),
    /// A number as written.
    Number(String),
    String(String),
    List(Vec<Value>),
    Map(Vec<MapEntry>),
    Reference(Reference),
    /// A call left for the target to evaluate.
    Call(Call),
    /// Text with references spliced in, for a placeholder whose value is not known
    /// when the document compiles.
    Template(Vec<TemplatePart>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MapEntry {
    pub key: String,
    pub key_position: Position,
    pub value: Value,
}

/// A name read by one of the definition's reference rules.
#[derive(Clone, Debug, PartialEq)]
pub struct Reference {
    /// The scope the rule names.
    pub scope: String,
    /// For a scope named by its parent key, that key, such as the type a resource
    /// is declared under.
    pub scope_key: Option<String>,
    pub name: String,
    /// The text after the name, such as a field path into the declaration.
    pub path: String,
    /// The reference as written.
    pub text: String,
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A local, with the resolved value at the reference's path.
    Local(Box<Value>),
    /// A local whose path reaches into a value known only when the document is
    /// used, such as a field of a call's result.
    LocalUnknown,
    /// A builtin of a logical scope whose value the caller does not supply.
    Builtin,
    /// A declaration in the document.
    Declaration { position: Position },
    /// A name of a scope a key declares every name of.
    Undeclared,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub function: String,
    pub arguments: Vec<Value>,
    /// Whether the last argument is a list whose items are the call's arguments,
    /// for a splat call whose list is not known when the document compiles.
    pub expand_final_argument: bool,
}

impl Call {
    pub fn new(function: impl Into<String>, arguments: Vec<Value>) -> Self {
        Self {
            function: function.into(),
            arguments,
            expand_final_argument: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TemplatePart {
    Text(String),
    Reference(Reference),
}

impl Value {
    pub fn new(kind: ValueKind, position: Position) -> Self {
        Self { kind, position }
    }

    pub fn string(text: impl Into<String>, position: Position) -> Self {
        Self::new(ValueKind::String(text.into()), position)
    }

    pub fn entries(&self) -> Option<&[MapEntry]> {
        match &self.kind {
            ValueKind::Map(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn items(&self) -> Option<&[Value]> {
        match &self.kind {
            ValueKind::List(items) => Some(items),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries()?
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| &entry.value)
    }

    pub fn as_str(&self) -> Option<&str> {
        match &self.kind {
            ValueKind::String(text) => Some(text),
            _ => None,
        }
    }

    /// A scalar's text, for splicing into a longer string.
    pub fn scalar_text(&self) -> Option<String> {
        match &self.kind {
            ValueKind::Null => Some(String::new()),
            ValueKind::Bool(value) => Some(value.to_string()),
            ValueKind::Number(text) | ValueKind::String(text) => Some(text.clone()),
            ValueKind::Reference(Reference {
                target: Target::Local(value),
                ..
            }) => value.scalar_text(),
            _ => None,
        }
    }

    /// The value with every local reference replaced by the local's value.
    pub fn local_value(&self) -> &Value {
        match &self.kind {
            ValueKind::Reference(Reference {
                target: Target::Local(value),
                ..
            }) => value.local_value(),
            _ => self,
        }
    }
}
