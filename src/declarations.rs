//! The names a document declares, by the definition's declaration rules.

use std::collections::BTreeMap;

use crate::definition::{DeclarationRule, KeyToken, NameSource, NameSpelling, Scope};
use crate::diagnostic::Diagnostic;
use crate::json_path::{PathPattern, PathStep};
use crate::position::Position;
use crate::resolved::{Declared, Value, ValueKind};

#[derive(Clone, Debug)]
pub struct Declaration {
    pub position: Position,
    pub path: Vec<PathStep>,
    pub value: Value,
}

/// Declarations by scope, then by the key a scope named by its parent key is under
/// (`""` for any other scope), then by name.
#[derive(Clone, Debug, Default)]
pub struct Declarations {
    names: BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<Declaration>>>>,
    /// Scopes, and parent keys, a key declares every name of.
    every_name: BTreeMap<String, Vec<String>>,
    ordered: Vec<Declared>,
}

pub struct CompiledDeclarationRule {
    rule: DeclarationRule,
    pattern: PathPattern,
}

pub fn compile_rules(
    rules: &[DeclarationRule],
) -> Result<Vec<CompiledDeclarationRule>, Diagnostic> {
    rules
        .iter()
        .map(|rule| {
            Ok(CompiledDeclarationRule {
                rule: rule.clone(),
                pattern: PathPattern::parse(&rule.path, &rule.skip_keys)?,
            })
        })
        .collect()
}

impl Declarations {
    /// Collects the declarations of `rules` in `document`; `only_scope` limits them to one scope.
    pub fn collect(
        document: &Value,
        rules: &[CompiledDeclarationRule],
        scopes: &BTreeMap<String, Scope>,
        only_scope: Option<&str>,
    ) -> Self {
        let mut declarations = Self::default();
        let rules: Vec<&CompiledDeclarationRule> = rules
            .iter()
            .filter(|compiled| only_scope.is_none_or(|scope| compiled.rule.scope_name == scope))
            .collect();
        let mut path = Vec::new();
        declarations.visit(document, &mut path, None, &rules, scopes);
        declarations
    }

    fn visit(
        &mut self,
        value: &Value,
        path: &mut Vec<PathStep>,
        key_position: Option<&Position>,
        rules: &[&CompiledDeclarationRule],
        scopes: &BTreeMap<String, Scope>,
    ) {
        for compiled in rules {
            if compiled.pattern.selects(path) {
                self.declare(compiled, value, path, key_position, scopes);
            }
        }
        match &value.kind {
            ValueKind::Map(entries) => {
                for entry in entries {
                    path.push(PathStep::Key(entry.key.clone()));
                    self.visit(&entry.value, path, Some(&entry.key_position), rules, scopes);
                    path.pop();
                }
            }
            ValueKind::List(items) => {
                for (index, item) in items.iter().enumerate() {
                    path.push(PathStep::Index(index));
                    self.visit(item, path, None, rules, scopes);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    fn declare(
        &mut self,
        compiled: &CompiledDeclarationRule,
        value: &Value,
        path: &[PathStep],
        key_position: Option<&Position>,
        scopes: &BTreeMap<String, Scope>,
    ) {
        let rule = &compiled.rule;
        let key = match path.last() {
            Some(PathStep::Key(key)) => Some(key.as_str()),
            _ => None,
        };
        if key.is_some_and(|key| {
            rule.exclude_candidates
                .iter()
                .any(|candidate| candidate == key)
        }) {
            return;
        }
        let named_by_parent_key = scopes
            .get(&rule.scope_name)
            .is_some_and(|scope| scope.named_by_parent_key);
        let parent_key = if named_by_parent_key {
            path.iter()
                .rev()
                .skip(1)
                .find_map(|step| match step {
                    PathStep::Key(key) => Some(key.clone()),
                    PathStep::Index(_) => None,
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        if rule.declares_every_name {
            self.every_name
                .entry(rule.scope_name.clone())
                .or_default()
                .push(parent_key);
            return;
        }
        let names: Vec<String> = match rule.name_source {
            NameSource::Key => key
                .map(|key| key_name(key, rule.key_token))
                .into_iter()
                .collect(),
            NameSource::Value => value.scalar_text().into_iter().collect(),
            NameSource::MetaArgument => key
                .zip(rule.meta_argument_name.as_deref())
                .map(|(key, argument)| meta_argument_values(key, argument))
                .unwrap_or_default(),
        };
        let position = key_position
            .cloned()
            .unwrap_or_else(|| value.position.clone());
        for name in names {
            let name = match rule.name_spelling {
                NameSpelling::AsWritten => name,
                NameSpelling::DashesAsUnderscores => name.replace('-', "_"),
            };
            self.ordered.push(Declared {
                scope: rule.scope_name.clone(),
                scope_key: (!parent_key.is_empty()).then(|| parent_key.clone()),
                name: name.clone(),
                key: key.unwrap_or_default().to_owned(),
                path: path.to_vec(),
                key_position: position.clone(),
                value: value.clone(),
            });
            self.names
                .entry(rule.scope_name.clone())
                .or_default()
                .entry(parent_key.clone())
                .or_default()
                .entry(name)
                .or_default()
                .push(Declaration {
                    position: position.clone(),
                    path: path.to_vec(),
                    value: value.clone(),
                });
        }
    }

    /// Every declaration, in the order the document writes them.
    pub fn in_document_order(self) -> Vec<Declared> {
        self.ordered
    }

    pub fn find(&self, scope: &str, scope_key: Option<&str>, name: &str) -> Option<&[Declaration]> {
        self.names
            .get(scope)?
            .get(scope_key.unwrap_or(""))?
            .get(name)
            .map(Vec::as_slice)
    }

    pub fn declares_every_name(&self, scope: &str, scope_key: Option<&str>) -> bool {
        self.every_name.get(scope).is_some_and(|keys| {
            keys.iter()
                .any(|key| key.is_empty() || Some(key.as_str()) == scope_key)
        })
    }

    /// Every declaration of `scope`, by name.
    pub fn of_scope(&self, scope: &str) -> impl Iterator<Item = (&str, &Declaration)> {
        self.names
            .get(scope)
            .into_iter()
            .flat_map(|by_key| by_key.values())
            .flat_map(|by_name| by_name.iter())
            .filter_map(|(name, declarations)| {
                declarations
                    .first()
                    .map(|declaration| (name.as_str(), declaration))
            })
    }
}

fn key_name(key: &str, token: Option<KeyToken>) -> String {
    let mut words = key.split_whitespace();
    match token {
        Some(KeyToken::FirstWord) => words.next().unwrap_or("").to_owned(),
        Some(KeyToken::LastWord) => words.next_back().unwrap_or("").to_owned(),
        Some(KeyToken::AllWords) | None => key.trim().to_owned(),
    }
}

/// The values of `argument` in a key such as `http(id=config)`.
fn meta_argument_values(key: &str, argument: &str) -> Vec<String> {
    let Some(open) = key.find('(') else {
        return Vec::new();
    };
    let Some(close) = key.rfind(')') else {
        return Vec::new();
    };
    key[open + 1..close]
        .split(',')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(name, _)| name.trim() == argument)
        .map(|(_, value)| value.trim().to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_token_picks_the_word_that_names() {
        assert_eq!(
            key_name("name mock-orders", Some(KeyToken::LastWord)),
            "mock-orders"
        );
        assert_eq!(
            key_name("defaults fn.merge", Some(KeyToken::FirstWord)),
            "defaults"
        );
        assert_eq!(key_name("orders", Some(KeyToken::AllWords)), "orders");
    }

    #[test]
    fn a_meta_argument_names_each_listed_id() {
        assert_eq!(meta_argument_values("http(id=config)", "id"), ["config"]);
        assert!(meta_argument_values("http", "id").is_empty());
    }
}
