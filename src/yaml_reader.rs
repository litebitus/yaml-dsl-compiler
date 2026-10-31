//! Reads YAML into nodes that keep where each value was written.

use std::collections::HashMap;
use std::sync::Arc;

use saphyr_parser::{Event, Parser, ScalarStyle, Span, Tag};

use crate::definition::TagKey;
use crate::diagnostic::Diagnostic;
use crate::node::{Entry, Node, NodeKind};
use crate::position::Position;

/// Parses the first document of `text`. A node carrying one of `tag_keys` reads as
/// a map holding that tag's key, the node its value.
pub fn parse(file: &Arc<str>, text: &str, tag_keys: &[TagKey]) -> Result<Node, Diagnostic> {
    let mut builder = Builder {
        file,
        tag_keys,
        anchors: HashMap::new(),
        stack: Vec::new(),
        root: None,
    };
    for event in Parser::new_from_str(text) {
        let (event, span) = event.map_err(|error| {
            let marker = error.marker();
            Diagnostic::at(
                &Position::new(file, marker.line(), marker.col() + 1),
                error.info().to_owned(),
            )
        })?;
        if builder.accept(event, span)? {
            break;
        }
    }
    Ok(builder
        .root
        .unwrap_or_else(|| Node::new(NodeKind::Null, Position::file_start(file))))
}

enum Frame {
    List {
        items: Vec<Node>,
        position: Position,
        anchor: usize,
        tag: Option<String>,
    },
    Map {
        entries: Vec<Entry>,
        pending_key: Option<(String, Position)>,
        position: Position,
        anchor: usize,
        tag: Option<String>,
    },
}

struct Builder<'a> {
    file: &'a Arc<str>,
    tag_keys: &'a [TagKey],
    anchors: HashMap<usize, Node>,
    stack: Vec<Frame>,
    root: Option<Node>,
}

impl Builder<'_> {
    fn position(&self, span: &Span) -> Position {
        Position::new(self.file, span.start.line(), span.start.col() + 1)
    }

    /// Takes one event; true once the first document is complete.
    fn accept(&mut self, event: Event<'_>, span: Span) -> Result<bool, Diagnostic> {
        match event {
            Event::DocumentEnd => return Ok(self.root.is_some()),
            Event::Scalar(text, style, anchor, tag) => {
                let position = self.position(&span);
                let tag_text = tag.as_deref().map(tag_text);
                let node = scalar_node(&text, style, tag.as_deref(), position);
                self.complete(node, anchor, tag_text)?;
            }
            Event::Alias(anchor) => {
                let position = self.position(&span);
                let node = self.anchors.get(&anchor).cloned().ok_or_else(|| {
                    Diagnostic::at(&position, "alias of an anchor not defined before it")
                })?;
                self.complete(node, 0, None)?;
            }
            Event::SequenceStart(anchor, tag) => {
                let position = self.position(&span);
                self.stack.push(Frame::List {
                    items: Vec::new(),
                    position,
                    anchor,
                    tag: tag.as_deref().map(tag_text),
                });
            }
            Event::MappingStart(anchor, tag) => {
                let position = self.position(&span);
                self.stack.push(Frame::Map {
                    entries: Vec::new(),
                    pending_key: None,
                    position,
                    anchor,
                    tag: tag.as_deref().map(tag_text),
                });
            }
            Event::SequenceEnd => {
                if let Some(Frame::List {
                    items,
                    position,
                    anchor,
                    tag,
                }) = self.stack.pop()
                {
                    self.complete(Node::new(NodeKind::List(items), position), anchor, tag)?;
                }
            }
            Event::MappingEnd => {
                if let Some(Frame::Map {
                    entries,
                    position,
                    anchor,
                    tag,
                    ..
                }) = self.stack.pop()
                {
                    self.complete(Node::new(NodeKind::Map(entries), position), anchor, tag)?;
                }
            }
            Event::Nothing | Event::StreamStart | Event::StreamEnd | Event::DocumentStart(_) => {}
        }
        Ok(false)
    }

    fn complete(
        &mut self,
        node: Node,
        anchor: usize,
        tag: Option<String>,
    ) -> Result<(), Diagnostic> {
        let node = match tag.and_then(|tag| self.tag_keys.iter().find(|tag_key| tag_key.tag == tag))
        {
            Some(tag_key) => {
                let position = node.position.clone();
                let entry = Entry {
                    key: tag_key.key.clone(),
                    key_position: position.clone(),
                    value: node,
                };
                Node::new(NodeKind::Map(vec![entry]), position)
            }
            None => node,
        };
        if anchor != 0 {
            self.anchors.insert(anchor, node.clone());
        }
        match self.stack.last_mut() {
            None => self.root = Some(node),
            Some(Frame::List { items, .. }) => items.push(node),
            Some(Frame::Map {
                entries,
                pending_key,
                ..
            }) => match pending_key.take() {
                None => {
                    let key = node
                        .scalar_text()
                        .filter(|_| !matches!(node.kind, NodeKind::Null))
                        .ok_or_else(|| {
                            Diagnostic::at(&node.position, "a mapping key must be a scalar")
                        })?;
                    if entries.iter().any(|entry| entry.key == key) {
                        return Err(Diagnostic::at(
                            &node.position,
                            format!("duplicate key `{key}`"),
                        ));
                    }
                    *pending_key = Some((key, node.position));
                }
                Some((key, key_position)) => entries.push(Entry {
                    key,
                    key_position,
                    value: node,
                }),
            },
        }
        Ok(())
    }
}

fn tag_text(tag: &Tag) -> String {
    format!("{}{}", tag.handle, tag.suffix)
}

fn scalar_node(text: &str, style: ScalarStyle, tag: Option<&Tag>, position: Position) -> Node {
    let explicit_string = tag.is_some_and(|tag| tag.handle == "!!" && tag.suffix == "str")
        || tag.is_some_and(|tag| tag.handle == "tag:yaml.org,2002:" && tag.suffix == "str");
    let kind = if style != ScalarStyle::Plain || explicit_string {
        NodeKind::String(text.to_owned())
    } else {
        plain_scalar_kind(text)
    };
    Node::new(kind, position)
}

/// The YAML 1.2 core schema's reading of a plain scalar.
fn plain_scalar_kind(text: &str) -> NodeKind {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return NodeKind::Null,
        "true" | "True" | "TRUE" => return NodeKind::Bool(true),
        "false" | "False" | "FALSE" => return NodeKind::Bool(false),
        _ => {}
    }
    if is_core_number(text) {
        NodeKind::Number(text.to_owned())
    } else {
        NodeKind::String(text.to_owned())
    }
}

fn is_core_number(text: &str) -> bool {
    let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
    if unsigned.is_empty() {
        return false;
    }
    if unsigned.bytes().all(|byte| byte.is_ascii_digit()) {
        return true;
    }
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (unsigned, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some((whole, fraction)) => {
            (!whole.is_empty() || !fraction.is_empty())
                && whole.bytes().all(|byte| byte.is_ascii_digit())
                && fraction.bytes().all(|byte| byte.is_ascii_digit())
        }
        None => !mantissa.is_empty() && mantissa.bytes().all(|byte| byte.is_ascii_digit()),
    };
    let exponent_ok = exponent.is_none_or(|exponent| {
        let digits = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    mantissa_ok && exponent_ok && (mantissa.contains('.') || exponent.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> Arc<str> {
        Arc::from("mock.yml")
    }

    #[test]
    fn keeps_key_order_and_positions() {
        let node = parse(&file(), "zeta: 1\nalpha:\n  nested: true\n", &[]).unwrap();
        let entries = node.entries().unwrap();
        assert_eq!(entries[0].key, "zeta");
        assert_eq!(entries[1].key, "alpha");
        assert_eq!(entries[1].key_position.line, 2);
        assert_eq!(
            entries[1].value.get("nested").unwrap().kind,
            NodeKind::Bool(true)
        );
        assert_eq!(entries[1].value.get("nested").unwrap().position.column, 11);
    }

    #[test]
    fn reads_plain_scalars_by_the_core_schema() {
        let node = parse(&file(), "a: 30\nb: 1.5\nc: \"30\"\nd: ~\ne: mock\n", &[]).unwrap();
        assert_eq!(node.get("a").unwrap().kind, NodeKind::Number("30".into()));
        assert_eq!(node.get("b").unwrap().kind, NodeKind::Number("1.5".into()));
        assert_eq!(node.get("c").unwrap().kind, NodeKind::String("30".into()));
        assert_eq!(node.get("d").unwrap().kind, NodeKind::Null);
        assert_eq!(node.get("e").unwrap().kind, NodeKind::String("mock".into()));
    }

    #[test]
    fn a_listed_tag_reads_as_a_map_holding_its_key() {
        let tags = [TagKey {
            tag: "!Ref".into(),
            key: "Ref".into(),
        }];
        let node = parse(&file(), "bucket: !Ref MockBucket\n", &tags).unwrap();
        let reference = node.get("bucket").unwrap();
        assert_eq!(
            reference.get("Ref").unwrap().kind,
            NodeKind::String("MockBucket".into())
        );
    }

    #[test]
    fn an_alias_reads_as_its_anchor() {
        let node = parse(&file(), "a: &shared {x: 1}\nb: *shared\n", &[]).unwrap();
        assert_eq!(
            node.get("b").unwrap().get("x").unwrap().kind,
            NodeKind::Number("1".into())
        );
    }

    #[test]
    fn a_duplicate_key_is_refused() {
        let error = parse(&file(), "a: 1\na: 2\n", &[]).unwrap_err();
        assert_eq!(error.message, "duplicate key `a`");
        assert_eq!(error.position.unwrap().line, 2);
    }
}
