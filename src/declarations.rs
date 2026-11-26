//! The names a document declares, by the definition's declaration rules.

use std::collections::BTreeMap;

use yaml_dsl_spec::Diagnostic;
use yaml_dsl_spec::Position;
use yaml_dsl_spec::definition::{DeclarationRule, KeyToken, NameSource, NameSpelling, Scope};
use yaml_dsl_spec::json_path::{PathPattern, PathStep};
use yaml_dsl_spec::resolved::{
    Declared, LoopBody, MapEntry, Reference, Target, TemplatePart, Value, ValueKind,
};

/// Whether an entry of the map at the path given holds a key a rule reads no name
/// from as written: a deferred key, or a key a loop's map body writes that reads no
/// loop variable.
pub type UnnamedKeyTest<'t> = dyn Fn(&MapEntry, &[PathStep]) -> bool + 't;

/// Stands in a key's spelling for a value not known when the document compiles.
const UNKNOWN_PART: char = '\u{0}';

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
    every_name: BTreeMap<String, Vec<EveryNameKey>>,
    ordered: Vec<Declared>,
}

/// A key that declares every name of a scope, or every name it can spell.
#[derive(Clone, Debug)]
struct EveryNameKey {
    /// The parent key, for a scope named by its parent key; `""` otherwise.
    scope_key: String,
    /// For a key reading a loop variable, its text around each value not known.
    spelling_segments: Option<Vec<String>>,
}

/// The key a value is under.
#[derive(Clone, Copy)]
enum HoldingKey<'v> {
    MapKey(&'v MapEntry),
    /// A key of a loop's map body, read as a value.
    LoopBodyKey(&'v Value),
}

/// What a declaration rule reads a name from in a key.
enum KeyNaming {
    /// A resolved key, or no key.
    AsWritten,
    /// A deferred key reading a loop variable, each value not known as `UNKNOWN_PART`.
    Spelling(String),
    /// Any other deferred key, or a key a loop's map body writes that reads no loop
    /// variable.
    NoName,
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
    /// A rule reads no name from a key `is_unnamed_key` answers for, except a
    /// deferred key reading a loop variable, which declares every name it can spell.
    pub fn collect(
        document: &Value,
        rules: &[CompiledDeclarationRule],
        scopes: &BTreeMap<String, Scope>,
        only_scope: Option<&str>,
        is_unnamed_key: &UnnamedKeyTest<'_>,
    ) -> Self {
        let mut declarations = Self::default();
        let rules: Vec<&CompiledDeclarationRule> = rules
            .iter()
            .filter(|compiled| only_scope.is_none_or(|scope| compiled.rule.scope_name == scope))
            .collect();
        let mut path = Vec::new();
        declarations.visit(document, &mut path, None, &rules, scopes, is_unnamed_key);
        declarations
    }

    fn visit(
        &mut self,
        value: &Value,
        path: &mut Vec<PathStep>,
        holding_key: Option<HoldingKey<'_>>,
        rules: &[&CompiledDeclarationRule],
        scopes: &BTreeMap<String, Scope>,
        is_unnamed_key: &UnnamedKeyTest<'_>,
    ) {
        let key_position = holding_key.map(|holding_key| match holding_key {
            HoldingKey::MapKey(entry) => &entry.key_position,
            HoldingKey::LoopBodyKey(key) => &key.position,
        });
        for compiled in rules {
            if compiled.pattern.selects(path) {
                let key_naming = match holding_key {
                    None => KeyNaming::AsWritten,
                    Some(HoldingKey::MapKey(entry))
                        if !is_unnamed_key(entry, &path[..path.len() - 1]) =>
                    {
                        KeyNaming::AsWritten
                    }
                    Some(HoldingKey::MapKey(entry)) => {
                        deferred_key_naming(entry.deferred_key.as_ref())
                    }
                    Some(HoldingKey::LoopBodyKey(key)) => match key.kind {
                        ValueKind::Reference(_) | ValueKind::Template(_) => {
                            deferred_key_naming(Some(key))
                        }
                        _ => KeyNaming::NoName,
                    },
                };
                self.declare(compiled, value, path, key_position, &key_naming, scopes);
            }
        }
        match &value.kind {
            ValueKind::Map(entries) => {
                for entry in entries {
                    path.push(PathStep::Key(entry.key.clone()));
                    let holding_key = Some(HoldingKey::MapKey(entry));
                    self.visit(
                        &entry.value,
                        path,
                        holding_key,
                        rules,
                        scopes,
                        is_unnamed_key,
                    );
                    path.pop();
                }
            }
            ValueKind::List(items) => {
                for (index, item) in items.iter().enumerate() {
                    path.push(PathStep::Index(index));
                    self.visit(item, path, None, rules, scopes, is_unnamed_key);
                    path.pop();
                }
            }
            ValueKind::Loop(left) => match &left.body {
                LoopBody::Entries(entries) => {
                    for entry in entries {
                        path.push(PathStep::Key(key_as_written(&entry.key)));
                        let holding_key = Some(HoldingKey::LoopBodyKey(&entry.key));
                        self.visit(
                            &entry.value,
                            path,
                            holding_key,
                            rules,
                            scopes,
                            is_unnamed_key,
                        );
                        path.pop();
                    }
                }
                LoopBody::Item(item) => {
                    path.push(PathStep::Index(0));
                    self.visit(item, path, None, rules, scopes, is_unnamed_key);
                    path.pop();
                }
            },
            _ => {}
        }
    }

    fn declare(
        &mut self,
        compiled: &CompiledDeclarationRule,
        value: &Value,
        path: &[PathStep],
        key_position: Option<&Position>,
        key_naming: &KeyNaming,
        scopes: &BTreeMap<String, Scope>,
    ) {
        let rule = &compiled.rule;
        let key = match path.last() {
            Some(PathStep::Key(key)) => Some(key.as_str()),
            _ => None,
        };
        let key_read_for_name = key.filter(|_| matches!(key_naming, KeyNaming::AsWritten));
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
        let spelling_segments = match (key_naming, rule.name_source) {
            _ if rule.declares_every_name => None,
            (KeyNaming::Spelling(spelling), NameSource::Key) => {
                let spelled = spelled_name(key_name(spelling, rule.key_token), rule.name_spelling);
                Some(spelled.split(UNKNOWN_PART).map(str::to_owned).collect())
            }
            _ => None,
        };
        if rule.declares_every_name || spelling_segments.is_some() {
            self.every_name
                .entry(rule.scope_name.clone())
                .or_default()
                .push(EveryNameKey {
                    scope_key: parent_key,
                    spelling_segments,
                });
            return;
        }
        let names: Vec<String> = match rule.name_source {
            NameSource::Key => key_read_for_name
                .map(|key| key_name(key, rule.key_token))
                .into_iter()
                .collect(),
            NameSource::Value => value.scalar_text().into_iter().collect(),
            NameSource::MetaArgument => key_read_for_name
                .zip(rule.meta_argument_name.as_deref())
                .map(|(key, argument)| meta_argument_values(key, argument))
                .unwrap_or_default(),
        };
        let position = key_position
            .cloned()
            .unwrap_or_else(|| value.position.clone());
        for name in names {
            let name = spelled_name(name, rule.name_spelling);
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

    /// Whether a key declaring every name of `scope`, or every name it can spell,
    /// declares `name`.
    pub fn declares_every_name(&self, scope: &str, scope_key: Option<&str>, name: &str) -> bool {
        self.every_name.get(scope).is_some_and(|keys| {
            keys.iter().any(|key| {
                (key.scope_key.is_empty() || Some(key.scope_key.as_str()) == scope_key)
                    && key
                        .spelling_segments
                        .as_ref()
                        .is_none_or(|segments| spells(segments, name))
            })
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

/// What a rule reads from a deferred key: a key reading a loop variable spells
/// every name its values can make.
fn deferred_key_naming(deferred_key: Option<&Value>) -> KeyNaming {
    let reads_loop_variable = |reference: &Reference| reference.target == Target::LoopVariable;
    match deferred_key.map(|key| &key.kind) {
        Some(ValueKind::Reference(reference)) if reads_loop_variable(reference) => {
            KeyNaming::Spelling(UNKNOWN_PART.to_string())
        }
        Some(ValueKind::Template(parts))
            if parts.iter().any(|part| {
                matches!(part, TemplatePart::Reference(reference) if reads_loop_variable(reference))
            }) =>
        {
            KeyNaming::Spelling(
                parts
                    .iter()
                    .map(|part| match part {
                        TemplatePart::Text(text) => text.clone(),
                        TemplatePart::Reference(_) => UNKNOWN_PART.to_string(),
                    })
                    .collect(),
            )
        }
        _ => KeyNaming::NoName,
    }
}

/// A key of a loop's map body as written.
fn key_as_written(key: &Value) -> String {
    match &key.kind {
        ValueKind::Reference(reference) => reference.text.clone(),
        ValueKind::Template(parts) => parts
            .iter()
            .map(|part| match part {
                TemplatePart::Text(text) => text.as_str(),
                TemplatePart::Reference(reference) => reference.text.as_str(),
            })
            .collect(),
        _ => key.scalar_text().unwrap_or_default(),
    }
}

/// Whether `name` is `segments` in order with any text between each.
fn spells(segments: &[String], name: &str) -> bool {
    let Some((first, rest)) = segments.split_first() else {
        return false;
    };
    let Some(mut remaining) = name.strip_prefix(first.as_str()) else {
        return false;
    };
    let Some((last, middle)) = rest.split_last() else {
        return remaining.is_empty();
    };
    for segment in middle {
        let Some(found_at) = remaining.find(segment.as_str()) else {
            return false;
        };
        remaining = &remaining[found_at + segment.len()..];
    }
    remaining.ends_with(last.as_str())
}

fn spelled_name(name: String, spelling: NameSpelling) -> String {
    match spelling {
        NameSpelling::AsWritten => name,
        NameSpelling::DashesAsUnderscores => name.replace('-', "_"),
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
    fn a_spelling_matches_its_segments_in_order_with_any_text_between() {
        let segments = ["Topic".to_owned(), "-".to_owned(), String::new()];
        assert!(spells(&segments, "Topic-"));
        assert!(spells(&segments, "TopicA-mock"));
        assert!(!spells(&segments, "TopicA"));
        assert!(!spells(&segments, "MockTopic-a"));
    }

    #[test]
    fn a_meta_argument_names_each_listed_id() {
        assert_eq!(meta_argument_values("http(id=config)", "id"), ["config"]);
        assert!(meta_argument_values("http", "id").is_empty());
    }
}
