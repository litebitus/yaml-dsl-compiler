//! JSON, two-space indented in tree order.

use serde_json::{Map, Value as JsonValue};

use crate::node::number_json;
use crate::output::{BodyItem, Expression, OutputFile};

pub fn write(file: &OutputFile) -> Result<String, String> {
    if file.comment.is_some() {
        return Err("JSON has no comments".to_owned());
    }
    let mut root = Map::new();
    write_body(&file.body, &mut root)?;
    let mut text = serde_json::to_string_pretty(&JsonValue::Object(root))
        .map_err(|error| error.to_string())?;
    text.push('\n');
    Ok(text)
}

fn write_body(body: &[BodyItem], object: &mut Map<String, JsonValue>) -> Result<(), String> {
    for item in body {
        match item {
            BodyItem::Attribute(attribute) => {
                if attribute.comment.is_some() {
                    return Err(format!("`{}`: JSON has no comments", attribute.name));
                }
                if object
                    .insert(attribute.name.clone(), json(&attribute.value)?)
                    .is_some()
                {
                    return Err(format!("`{}` is written twice", attribute.name));
                }
            }
            BodyItem::Block(block) => {
                if block.comment.is_some() {
                    return Err(format!("`{}`: JSON has no comments", block.block_type));
                }
                let mut body = Map::new();
                write_body(&block.body, &mut body)?;
                let mut holder = &mut *object;
                let mut keys = std::iter::once(&block.block_type)
                    .chain(&block.labels)
                    .peekable();
                while let Some(key) = keys.next() {
                    if keys.peek().is_none() {
                        insert_block(holder, key, body)?;
                        break;
                    }
                    let next = holder
                        .entry(key.clone())
                        .or_insert_with(|| JsonValue::Object(Map::new()));
                    holder = next
                        .as_object_mut()
                        .ok_or_else(|| format!("`{key}` is both a block and an attribute"))?;
                }
            }
        }
    }
    Ok(())
}

/// A block's body under its last key; a second block under the same keys makes
/// the key a list of bodies.
fn insert_block(
    holder: &mut Map<String, JsonValue>,
    key: &str,
    body: Map<String, JsonValue>,
) -> Result<(), String> {
    match holder.get_mut(key) {
        None => {
            holder.insert(key.to_owned(), JsonValue::Object(body));
        }
        Some(JsonValue::Array(bodies)) => bodies.push(JsonValue::Object(body)),
        Some(existing @ JsonValue::Object(_)) => {
            let first = existing.take();
            *existing = JsonValue::Array(vec![first, JsonValue::Object(body)]);
        }
        Some(_) => return Err(format!("`{key}` is both a block and an attribute")),
    }
    Ok(())
}

fn json(expression: &Expression) -> Result<JsonValue, String> {
    Ok(match expression {
        Expression::Null => JsonValue::Null,
        Expression::Bool(flag) => JsonValue::Bool(*flag),
        Expression::Number(text) => match number_json(text) {
            JsonValue::Number(number) => JsonValue::Number(number),
            _ => return Err(format!("`{text}` is not a number JSON writes")),
        },
        Expression::String(text) => JsonValue::String(text.clone()),
        Expression::List(items) => {
            JsonValue::Array(items.iter().map(json).collect::<Result<_, _>>()?)
        }
        Expression::Map(entries) => {
            let mut object = Map::new();
            for (key, value) in entries {
                object.insert(key.clone(), json(value)?);
            }
            JsonValue::Object(object)
        }
        Expression::Traversal(_) | Expression::Call(_) | Expression::Template(_) => {
            return Err("JSON has no references, calls or templates".to_owned());
        }
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::output::{Attribute, Block};
    use crate::writer::TargetLanguage;

    #[test]
    fn blocks_nest_by_type_then_labels() {
        let block = |label: &str| {
            BodyItem::Block(Block::new(
                "queue",
                vec![label.to_owned()],
                vec![BodyItem::Attribute(Attribute::new(
                    "visibility",
                    Expression::Number("30".into()),
                ))],
            ))
        };
        let file = OutputFile {
            path: PathBuf::from("mock.json"),
            language: TargetLanguage::Json,
            comment: None,
            body: vec![block("mock-orders"), block("mock-payments")],
        };
        let expected = r#"{
  "queue": {
    "mock-orders": {
      "visibility": 30
    },
    "mock-payments": {
      "visibility": 30
    }
  }
}
"#;
        assert_eq!(write(&file).unwrap(), expected);
    }
}
