//! Reads placeholders and references in scalars by the definition's rules.

use fancy_regex::Regex;

use crate::definition::{DslDefinition, ReferencePosition, ReferenceRule};
use crate::diagnostic::Diagnostic;
use crate::json_path::{PathSet, PathStep};
use crate::position::Position;
use crate::resolved::{Reference, TemplatePart, Value, ValueKind};

struct CompiledRule {
    rule: ReferenceRule,
    regex: Regex,
}

/// A name a rule read out of some text.
pub struct NameReading<'a> {
    pub rule: &'a ReferenceRule,
    pub scope_key: Option<String>,
    pub name: String,
    pub path: String,
    pub text: String,
    /// Where the scalar holding the reading was written.
    pub position: &'a Position,
    /// Whether the reading stands for the whole scalar rather than part of its text.
    pub whole: bool,
}

/// What a resolver makes of a reading.
pub enum Reading {
    /// The reading's value, spliced in or standing for the scalar.
    Value(Value),
    Reference(Reference),
    /// Not resolved by this rule: the next rule is tried.
    Keep,
    Error(String),
}

/// How a scan treats text a rule reads but no rule resolves.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    /// It stays as written, for a later phase.
    Lenient,
    /// It is an error.
    Strict,
}

pub type Resolver<'r> = dyn FnMut(&NameReading<'_>) -> Reading + 'r;

pub struct Scanner {
    placeholder: Option<Regex>,
    rules: Vec<CompiledRule>,
    unscanned: PathSet,
}

enum Part {
    Text(String),
    Value(Value),
    Reference(Reference),
}

impl Scanner {
    pub fn new(definition: &DslDefinition) -> Result<Self, Diagnostic> {
        let placeholder = match &definition.placeholder {
            Some(placeholder) => Some(compile(&placeholder.pattern)?),
            None => None,
        };
        let rules = definition
            .references
            .iter()
            .map(|rule| {
                Ok(CompiledRule {
                    rule: rule.clone(),
                    regex: compile(&rule.pattern)?,
                })
            })
            .collect::<Result<Vec<_>, Diagnostic>>()?;
        let unscanned = match &definition.placeholder {
            Some(placeholder) => PathSet::parse(
                placeholder
                    .unscanned_paths
                    .iter()
                    .map(|entry| (entry.path.as_str(), entry.skip_keys.as_slice())),
            )?,
            None => PathSet::default(),
        };
        Ok(Self {
            placeholder,
            rules,
            unscanned,
        })
    }

    /// Reads `text`, written at `position` and document `path`, handing each name a
    /// rule reads to `resolve`.
    pub fn scan(
        &self,
        text: &str,
        position: &Position,
        path: &[PathStep],
        strictness: Strictness,
        resolve: &mut Resolver<'_>,
    ) -> Result<Value, Diagnostic> {
        if self.unscanned.covers(path) {
            return Ok(Value::string(text, position.clone()));
        }
        let mut parts = self.placeholder_parts(text, position, strictness, resolve)?;
        if let [Part::Text(whole)] = parts.as_slice()
            && let Some(part) = self.read_name(
                whole,
                whole,
                ReferencePosition::WholeScalar,
                position,
                strictness,
                resolve,
            )?
        {
            parts = vec![part];
        }
        Ok(assemble(parts, position))
    }

    /// Reads `text` where only a constant may result, such as a key.
    pub fn scan_text(
        &self,
        text: &str,
        position: &Position,
        path: &[PathStep],
        strictness: Strictness,
        resolve: &mut Resolver<'_>,
    ) -> Result<String, Diagnostic> {
        if self.unscanned.covers(path) {
            return Ok(text.to_owned());
        }
        let parts = self.placeholder_parts(text, position, strictness, resolve)?;
        let mut joined = String::new();
        for part in parts {
            match part {
                Part::Text(piece) => joined.push_str(&piece),
                Part::Value(value) => joined.push_str(&value.scalar_text().ok_or_else(|| {
                    Diagnostic::at(
                        position,
                        format!("a placeholder in `{text}` holds a value that is not a scalar"),
                    )
                })?),
                Part::Reference(reference) => {
                    return Err(Diagnostic::at(
                        position,
                        format!(
                            "`{}` in `{text}` has no value when the document compiles",
                            reference.text
                        ),
                    ));
                }
            }
        }
        Ok(joined)
    }

    fn placeholder_parts(
        &self,
        text: &str,
        position: &Position,
        strictness: Strictness,
        resolve: &mut Resolver<'_>,
    ) -> Result<Vec<Part>, Diagnostic> {
        let Some(placeholder) = &self.placeholder else {
            return Ok(vec![Part::Text(text.to_owned())]);
        };
        let mut parts = Vec::new();
        let mut cursor = 0;
        while cursor <= text.len() {
            let Some(captures) = placeholder
                .captures_from_pos(text, cursor)
                .map_err(|error| regex_error(position, &error))?
            else {
                break;
            };
            let whole_match = captures.get(0).expect("a match has a whole group");
            let body = captures.name("body").ok_or_else(|| {
                Diagnostic::unplaced("the placeholder pattern has no `body` group")
            })?;
            if whole_match.end() == whole_match.start() {
                break;
            }
            push_text(&mut parts, &text[cursor..whole_match.start()]);
            let body_text = if placeholder
                .is_match(body.as_str())
                .map_err(|error| regex_error(position, &error))?
            {
                self.scan_text(body.as_str(), position, &[], strictness, resolve)?
            } else {
                body.as_str().to_owned()
            };
            let whole = whole_match.start() == 0 && whole_match.end() == text.len();
            let wanted = if whole {
                ReferencePosition::WholePlaceholder
            } else {
                ReferencePosition::PlaceholderInText
            };
            let written = format!(
                "{}{}{}",
                &text[whole_match.start()..body.start()],
                body_text,
                &text[body.end()..whole_match.end()]
            );
            match self.read_name(&body_text, &written, wanted, position, strictness, resolve)? {
                Some(Part::Value(value)) if !whole => {
                    let spliced = value.scalar_text().ok_or_else(|| {
                        Diagnostic::at(
                            position,
                            format!("`{written}` inside text holds a value that is not a scalar"),
                        )
                    })?;
                    push_text(&mut parts, &spliced);
                }
                Some(Part::Text(text)) => push_text(&mut parts, &text),
                Some(part) => parts.push(part),
                None if strictness == Strictness::Strict => {
                    return Err(Diagnostic::at(
                        position,
                        format!("no reference rule reads the placeholder `{written}`"),
                    ));
                }
                None => push_text(&mut parts, &written),
            }
            cursor = whole_match.end();
        }
        if cursor < text.len() {
            push_text(&mut parts, &text[cursor..]);
        }
        Ok(parts)
    }

    /// Tries each rule standing at `wanted` in order; the first that resolves is the
    /// reading. Under `Strict`, text a rule reads that no rule resolves is an error.
    fn read_name(
        &self,
        body: &str,
        written: &str,
        wanted: ReferencePosition,
        position: &Position,
        strictness: Strictness,
        resolve: &mut Resolver<'_>,
    ) -> Result<Option<Part>, Diagnostic> {
        let mut first_read: Option<String> = None;
        for compiled in self
            .rules
            .iter()
            .filter(|compiled| compiled.rule.positions.contains(&wanted))
        {
            let Some(captures) = compiled
                .regex
                .captures(body)
                .map_err(|error| regex_error(position, &error))?
            else {
                continue;
            };
            let whole_match = captures.get(0).expect("a match has a whole group");
            if whole_match.start() != 0 {
                continue;
            }
            let Some(name) = captures.name(&compiled.rule.name_group) else {
                continue;
            };
            let path = &body[whole_match.end()..];
            if !compiled.rule.text_after_name_allowed && !path.is_empty() {
                continue;
            }
            let scope_key = compiled
                .rule
                .scope_group
                .as_ref()
                .and_then(|group| captures.name(group))
                .map(|group| group.as_str().to_owned());
            let shown_scope = scope_key
                .clone()
                .unwrap_or_else(|| compiled.rule.scope_name.clone());
            first_read.get_or_insert_with(|| format!("{shown_scope}.{}", name.as_str()));
            let reading = NameReading {
                rule: &compiled.rule,
                scope_key,
                name: name.as_str().to_owned(),
                path: path.to_owned(),
                text: written.to_owned(),
                position,
                whole: wanted != ReferencePosition::PlaceholderInText,
            };
            match resolve(&reading) {
                Reading::Value(value) => return Ok(Some(Part::Value(value))),
                Reading::Reference(reference) => return Ok(Some(Part::Reference(reference))),
                Reading::Keep => {}
                Reading::Error(message) => return Err(Diagnostic::at(position, message)),
            }
        }
        match first_read {
            Some(read) if strictness == Strictness::Strict => Err(Diagnostic::at(
                position,
                format!("Invalid reference: {read}"),
            )),
            Some(_) => Ok(Some(Part::Text(written.to_owned()))),
            None => Ok(None),
        }
    }
}

fn push_text(parts: &mut Vec<Part>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Part::Text(last)) = parts.last_mut() {
        last.push_str(text);
    } else {
        parts.push(Part::Text(text.to_owned()));
    }
}

fn assemble(parts: Vec<Part>, position: &Position) -> Value {
    if parts.len() <= 1 {
        return match parts.into_iter().next() {
            None => Value::string("", position.clone()),
            Some(Part::Text(text)) => Value::string(text, position.clone()),
            Some(Part::Value(value)) => value,
            Some(Part::Reference(reference)) => {
                Value::new(ValueKind::Reference(reference), position.clone())
            }
        };
    }
    let template = parts
        .into_iter()
        .map(|part| match part {
            Part::Text(text) => TemplatePart::Text(text),
            Part::Reference(reference) => TemplatePart::Reference(reference),
            Part::Value(value) => TemplatePart::Text(value.scalar_text().unwrap_or_default()),
        })
        .collect();
    Value::new(ValueKind::Template(template), position.clone())
}

fn compile(pattern: &str) -> Result<Regex, Diagnostic> {
    Regex::new(pattern)
        .map_err(|error| Diagnostic::unplaced(format!("pattern `{pattern}`: {error}")))
}

fn regex_error(position: &Position, error: &fancy_regex::Error) -> Diagnostic {
    Diagnostic::at(position, format!("matching a pattern: {error}"))
}
