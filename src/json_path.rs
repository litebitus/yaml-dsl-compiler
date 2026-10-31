//! The JSONPath subset the DSL definition's paths are written in.

use crate::diagnostic::Diagnostic;
use crate::resolved::{Value, ValueKind};

/// One step of a concrete path into a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathStep {
    Key(String),
    Index(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Selector {
    Name(String),
    Wildcard,
    DescendantName(String),
    /// `..*`: every node below, never the node itself; only ever the last step.
    DescendantAll,
}

/// A parsed path and the keys its wildcard steps do not select or descend into.
#[derive(Clone, Debug)]
pub struct PathPattern {
    selectors: Vec<Selector>,
    skip_keys: Vec<String>,
}

impl PathPattern {
    pub fn parse(text: &str, skip_keys: &[String]) -> Result<Self, Diagnostic> {
        let invalid = || {
            Diagnostic::unplaced(format!(
                "`{text}` is not a path of the supported JSONPath subset"
            ))
        };
        let rest = text.strip_prefix('$').ok_or_else(invalid)?;
        let mut selectors = Vec::new();
        let mut index = 0;
        let bytes = rest.as_bytes();
        while index < bytes.len() {
            let descendant = rest[index..].starts_with("..");
            if descendant {
                index += 2;
            } else if bytes[index] == b'.' {
                index += 1;
            }
            let (selector, consumed) = if rest[index..].starts_with("['") {
                let close = rest[index + 2..].find("']").ok_or_else(invalid)?;
                (
                    Some(rest[index + 2..index + 2 + close].to_owned()),
                    close + 4,
                )
            } else if rest[index..].starts_with("[*]") {
                (None, 3)
            } else if rest[index..].starts_with('*') {
                (None, 1)
            } else {
                let end = rest[index..]
                    .find(['.', '['])
                    .map_or(rest.len(), |offset| index + offset);
                if end == index {
                    return Err(invalid());
                }
                (Some(rest[index..end].to_owned()), end - index)
            };
            index += consumed;
            selectors.push(match (descendant, selector) {
                (false, Some(name)) => Selector::Name(name),
                (false, None) => Selector::Wildcard,
                (true, Some(name)) => Selector::DescendantName(name),
                (true, None) if index == bytes.len() => Selector::DescendantAll,
                (true, None) => return Err(invalid()),
            });
        }
        Ok(Self {
            selectors,
            skip_keys: skip_keys.to_vec(),
        })
    }

    /// Every node of `document` the pattern selects, with its path, in the order
    /// the document writes them.
    pub fn select<'v>(&self, document: &'v Value) -> Vec<(Vec<PathStep>, &'v Value)> {
        let mut selected = Vec::new();
        let mut path = Vec::new();
        self.collect(document, &mut path, &mut selected);
        selected
    }

    fn collect<'v>(
        &self,
        value: &'v Value,
        path: &mut Vec<PathStep>,
        selected: &mut Vec<(Vec<PathStep>, &'v Value)>,
    ) {
        if self.selects(path) {
            selected.push((path.clone(), value));
        }
        match &value.kind {
            ValueKind::Map(entries) => {
                for entry in entries {
                    path.push(PathStep::Key(entry.key.clone()));
                    self.collect(&entry.value, path, selected);
                    path.pop();
                }
            }
            ValueKind::List(items) => {
                for (index, item) in items.iter().enumerate() {
                    path.push(PathStep::Index(index));
                    self.collect(item, path, selected);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    /// Whether the pattern selects the node at `path`.
    pub fn selects(&self, path: &[PathStep]) -> bool {
        self.matches_from(0, path)
    }

    /// Whether `path` is the node a step selects or anywhere below it.
    pub fn covers(&self, path: &[PathStep]) -> bool {
        (0..=path.len()).any(|length| self.selects(&path[..length]))
    }

    fn skipped(&self, step: &PathStep) -> bool {
        matches!(step, PathStep::Key(key) if self.skip_keys.contains(key))
    }

    fn matches_from(&self, selector_index: usize, path: &[PathStep]) -> bool {
        let Some(selector) = self.selectors.get(selector_index) else {
            return path.is_empty();
        };
        match selector {
            Selector::Name(name) => match path.first() {
                Some(PathStep::Key(key)) if key == name => {
                    self.matches_from(selector_index + 1, &path[1..])
                }
                _ => false,
            },
            Selector::Wildcard => match path.first() {
                Some(step) if !self.skipped(step) => {
                    self.matches_from(selector_index + 1, &path[1..])
                }
                _ => false,
            },
            Selector::DescendantName(name) => (0..path.len()).any(|depth| {
                path[..depth].iter().all(|step| !self.skipped(step))
                    && matches!(&path[depth], PathStep::Key(key) if key == name)
                    && self.matches_from(selector_index + 1, &path[depth + 1..])
            }),
            Selector::DescendantAll => {
                !path.is_empty() && path.iter().all(|step| !self.skipped(step))
            }
        }
    }
}

/// Several path entries; a node is selected when any of them selects it.
#[derive(Clone, Debug, Default)]
pub struct PathSet(Vec<PathPattern>);

impl PathSet {
    pub fn parse<'a>(
        entries: impl IntoIterator<Item = (&'a str, &'a [String])>,
    ) -> Result<Self, Diagnostic> {
        entries
            .into_iter()
            .map(|(path, skip_keys)| PathPattern::parse(path, skip_keys))
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    pub fn selects(&self, path: &[PathStep]) -> bool {
        self.0.iter().any(|pattern| pattern.selects(path))
    }

    pub fn covers(&self, path: &[PathStep]) -> bool {
        self.0.iter().any(|pattern| pattern.covers(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(steps: &[&str]) -> Vec<PathStep> {
        steps
            .iter()
            .map(|step| {
                step.parse::<usize>()
                    .map_or_else(|_| PathStep::Key((*step).to_owned()), PathStep::Index)
            })
            .collect()
    }

    fn pattern(text: &str, skip_keys: &[&str]) -> PathPattern {
        let skip_keys: Vec<String> = skip_keys.iter().map(|key| (*key).to_owned()).collect();
        PathPattern::parse(text, &skip_keys).unwrap()
    }

    #[test]
    fn a_name_and_a_wildcard_select_one_level_each() {
        let entries = pattern("$.*.*", &["locals"]);
        assert!(entries.selects(&path(&["queue", "orders"])));
        assert!(!entries.selects(&path(&["locals", "defaults"])));
        assert!(!entries.selects(&path(&["queue"])));
        assert!(pattern("$.locals.*", &[]).selects(&path(&["locals", "defaults"])));
    }

    #[test]
    fn a_descendant_step_selects_at_any_depth() {
        let fn_transform = pattern("$..['Fn::Transform']", &[]);
        assert!(fn_transform.selects(&path(&["Resources", "Bucket", "Fn::Transform"])));
        let below = pattern("$.cloud..*", &[]);
        assert!(below.selects(&path(&["cloud", "aws_regions", "0"])));
        assert!(!below.selects(&path(&["cloud"])));
    }

    #[test]
    fn list_items_are_selected_by_the_bracket_wildcard() {
        assert!(
            pattern("$.cases[*].steps[*].*", &[])
                .selects(&path(&["cases", "0", "steps", "2", "call"]))
        );
    }

    #[test]
    fn select_answers_every_selected_node_in_document_order() {
        use crate::position::Position;
        use crate::resolved::MapEntry;
        let position = Position::file_start(&std::sync::Arc::from("mock.yml"));
        let entry = |key: &str, value: Value| MapEntry {
            key: key.to_owned(),
            key_position: position.clone(),
            value,
        };
        let leaf = |text: &str| Value::string(text, position.clone());
        let map = |entries| Value::new(ValueKind::Map(entries), position.clone());
        let document = map(vec![
            entry(
                "queue",
                map(vec![entry("b", leaf("1")), entry("a", leaf("2"))]),
            ),
            entry("locals", map(vec![entry("c", leaf("3"))])),
        ]);
        let selected: Vec<Vec<PathStep>> = pattern("$.*.*", &["locals"])
            .select(&document)
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        assert_eq!(selected, [path(&["queue", "b"]), path(&["queue", "a"])]);
    }

    #[test]
    fn the_root_selects_the_document() {
        assert!(pattern("$", &[]).selects(&[]));
        assert!(pattern("$", &[]).covers(&path(&["any", "thing"])));
    }
}
