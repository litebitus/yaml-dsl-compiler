use serde_json::{Map, Number, Value as JsonValue};

use crate::position::Position;

/// A YAML value as written, with where each part was written.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub kind: NodeKind,
    pub position: Position,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    Null,
    Bool(bool),
    /// A number as written, so integers of any size and floats keep their text.
    Number(String),
    String(String),
    List(Vec<Node>),
    Map(Vec<Entry>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: String,
    pub key_position: Position,
    pub value: Node,
}

impl Node {
    pub fn new(kind: NodeKind, position: Position) -> Self {
        Self { kind, position }
    }

    pub fn entries(&self) -> Option<&[Entry]> {
        match &self.kind {
            NodeKind::Map(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn items(&self) -> Option<&[Node]> {
        match &self.kind {
            NodeKind::List(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match &self.kind {
            NodeKind::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Node> {
        self.entries()?
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| &entry.value)
    }

    pub fn is_map(&self) -> bool {
        matches!(self.kind, NodeKind::Map(_))
    }

    /// The scalar as text, for splicing into a longer string.
    pub fn scalar_text(&self) -> Option<String> {
        match &self.kind {
            NodeKind::Null => Some(String::new()),
            NodeKind::Bool(value) => Some(value.to_string()),
            NodeKind::Number(text) | NodeKind::String(text) => Some(text.clone()),
            NodeKind::List(_) | NodeKind::Map(_) => None,
        }
    }

    pub fn to_json(&self) -> JsonValue {
        match &self.kind {
            NodeKind::Null => JsonValue::Null,
            NodeKind::Bool(value) => JsonValue::Bool(*value),
            NodeKind::Number(text) => number_json(text),
            NodeKind::String(text) => JsonValue::String(text.clone()),
            NodeKind::List(items) => JsonValue::Array(items.iter().map(Node::to_json).collect()),
            NodeKind::Map(entries) => {
                let mut map = Map::new();
                for entry in entries {
                    map.insert(entry.key.clone(), entry.value.to_json());
                }
                JsonValue::Object(map)
            }
        }
    }
}

pub(crate) fn number_json(text: &str) -> JsonValue {
    if let Ok(integer) = text.parse::<i64>() {
        return JsonValue::Number(integer.into());
    }
    if let Ok(integer) = text.parse::<u64>() {
        return JsonValue::Number(integer.into());
    }
    text.parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .map(JsonValue::Number)
        .unwrap_or_else(|| JsonValue::String(text.to_owned()))
}
