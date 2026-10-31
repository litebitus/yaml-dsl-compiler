//! YAML, in block style with two-space indentation.

use crate::output::{BodyItem, Expression, OutputFile};
use crate::writer::{MAX_LINE_WIDTH, key_name};

const INDENT_WIDTH: usize = 2;
const MAX_FLOW_ITEMS: usize = 2;

pub fn write(file: &OutputFile) -> Result<String, String> {
    let mut lines = Vec::new();
    if let Some(comment) = &file.comment {
        lines.extend(comment_lines(comment, 0));
    }
    let mut entries = Vec::with_capacity(file.body.len());
    for item in &file.body {
        match item {
            BodyItem::Attribute(attribute) => entries.push((
                attribute.name.as_str(),
                &attribute.value,
                attribute.comment.as_deref(),
            )),
            BodyItem::Block(block) => {
                return Err(format!("`{}`: YAML has no blocks", block.block_type));
            }
        }
    }
    lines.extend(map_lines(&entries, 0)?);
    let mut text = lines.join("\n");
    text.push('\n');
    Ok(text)
}

fn map_lines(
    entries: &[(&str, &Expression, Option<&str>)],
    indent: usize,
) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    for (key, value, comment) in entries {
        if let Some(comment) = comment {
            lines.extend(comment_lines(comment, indent));
        }
        let prefix = format!("{}{}:", spaces(indent), scalar_text(key, false));
        lines.extend(value_lines(prefix, value, indent)?);
    }
    Ok(lines)
}

/// A value after `prefix`, a key or a list item's dash: on the same line when it
/// is a scalar or a flow list, else on the lines below.
fn value_lines(prefix: String, value: &Expression, indent: usize) -> Result<Vec<String>, String> {
    let inner = indent + INDENT_WIDTH;
    match value {
        Expression::Map(entries) if !entries.is_empty() => {
            let entries: Vec<(&str, &Expression, Option<&str>)> = entries
                .iter()
                .map(|(key, value)| Ok((key_name(key, "YAML")?, value, None)))
                .collect::<Result<_, String>>()?;
            let mut lines = vec![prefix];
            lines.extend(map_lines(&entries, inner)?);
            Ok(lines)
        }
        Expression::List(items) if !items.is_empty() => {
            if let Some(flow) = flow_list(items)?
                && prefix.len() + 1 + flow.len() <= MAX_LINE_WIDTH
            {
                return Ok(vec![format!("{prefix} {flow}")]);
            }
            let mut lines = vec![prefix];
            for item in items {
                lines.extend(list_item_lines(item, inner)?);
            }
            Ok(lines)
        }
        Expression::String(text) if text.contains('\n') => {
            let indicator = match text.trim_end_matches('\n').len() {
                trimmed if trimmed == text.len() => "|-",
                trimmed if trimmed + 1 == text.len() => "|",
                _ => "|+",
            };
            let mut lines = vec![format!("{prefix} {indicator}")];
            let body = text.strip_suffix('\n').unwrap_or(text);
            lines.extend(body.split('\n').map(|line| {
                if line.is_empty() {
                    String::new()
                } else {
                    format!("{}{line}", spaces(inner))
                }
            }));
            Ok(lines)
        }
        _ => Ok(vec![format!("{prefix} {}", scalar(value, false)?)]),
    }
}

fn list_item_lines(item: &Expression, indent: usize) -> Result<Vec<String>, String> {
    let dash = format!("{}-", spaces(indent));
    match item {
        Expression::Map(entries) if !entries.is_empty() => {
            let entries: Vec<(&str, &Expression, Option<&str>)> = entries
                .iter()
                .map(|(key, value)| Ok((key_name(key, "YAML")?, value, None)))
                .collect::<Result<_, String>>()?;
            let mut lines = map_lines(&entries, indent + INDENT_WIDTH)?;
            lines[0].replace_range(..dash.len(), &dash);
            Ok(lines)
        }
        _ => value_lines(dash, item, indent),
    }
}

/// A list of one or two scalars as a flow sequence.
fn flow_list(items: &[Expression]) -> Result<Option<String>, String> {
    if items.len() > MAX_FLOW_ITEMS {
        return Ok(None);
    }
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Expression::List(_) | Expression::Map(_) => return Ok(None),
            Expression::String(text) if text.contains('\n') => return Ok(None),
            _ => parts.push(scalar(item, true)?),
        }
    }
    Ok(Some(format!("[{}]", parts.join(", "))))
}

fn scalar(value: &Expression, in_flow: bool) -> Result<String, String> {
    match value {
        Expression::Null => Ok("null".to_owned()),
        Expression::Bool(flag) => Ok(flag.to_string()),
        Expression::Number(text) if is_core_number(text) => Ok(text.clone()),
        Expression::Number(text) => Err(format!("`{text}` is not a number YAML writes")),
        Expression::String(text) => Ok(scalar_text(text, in_flow)),
        Expression::List(_) => Ok("[]".to_owned()),
        Expression::Map(_) => Ok("{}".to_owned()),
        _ => Err("YAML has no expressions such as references, calls or templates".to_owned()),
    }
}

/// A string as a plain scalar where it reads back as the same string, else
/// double-quoted.
fn scalar_text(text: &str, in_flow: bool) -> String {
    if is_plain(text, in_flow) {
        text.to_owned()
    } else {
        double_quoted(text)
    }
}

fn is_plain(text: &str, in_flow: bool) -> bool {
    let Some(first) = text.chars().next() else {
        return false;
    };
    if text.starts_with(' ') || text.ends_with(' ') || text.ends_with(':') {
        return false;
    }
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) {
        return false;
    }
    if text.contains(": ") || text.contains(" #") || text.chars().any(char::is_control) {
        return false;
    }
    if in_flow && text.contains([',', '[', ']', '{', '}']) {
        return false;
    }
    !is_reserved_word(text) && !is_core_number(text)
}

/// Words a YAML reader of either the 1.1 or the 1.2 core schema reads as other than a string.
fn is_reserved_word(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "~" | "null"
            | "true"
            | "false"
            | "yes"
            | "no"
            | "on"
            | "off"
            | "y"
            | "n"
            | ".inf"
            | "-.inf"
            | "+.inf"
            | ".nan"
    )
}

fn is_core_number(text: &str) -> bool {
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if let Some(hex) = unsigned.strip_prefix("0x") {
        return !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    if let Some(octal) = unsigned.strip_prefix("0o") {
        return !octal.is_empty() && octal.bytes().all(|byte| (b'0'..=b'7').contains(&byte));
    }
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(index) => (&unsigned[..index], Some(&unsigned[index + 1..])),
        None => (unsigned, None),
    };
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    let mantissa_valid = match mantissa.split_once('.') {
        Some((whole, fraction)) => {
            digits(whole) && digits(fraction) && !(whole.is_empty() && fraction.is_empty())
        }
        None => !mantissa.is_empty() && digits(mantissa),
    };
    mantissa_valid
        && exponent.is_none_or(|exponent| {
            let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
            !exponent.is_empty() && digits(exponent)
        })
}

fn double_quoted(text: &str) -> String {
    let mut quoted = String::from("\"");
    for character in text.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            control if control.is_control() => {
                quoted.push_str(&format!("\\u{:04x}", control as u32))
            }
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

fn comment_lines(comment: &str, indent: usize) -> Vec<String> {
    comment
        .lines()
        .map(|line| {
            if line.is_empty() {
                format!("{}#", spaces(indent))
            } else {
                format!("{}# {line}", spaces(indent))
            }
        })
        .collect()
}

fn spaces(count: usize) -> String {
    " ".repeat(count)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::output::{Attribute, Block};
    use crate::writer::TargetLanguage;

    fn file(body: Vec<BodyItem>) -> OutputFile {
        OutputFile {
            path: PathBuf::from("mock.yml"),
            language: TargetLanguage::Yaml,
            comment: None,
            body,
        }
    }

    fn string(text: &str) -> Expression {
        Expression::String(text.to_owned())
    }

    #[test]
    fn writes_block_style_with_short_lists_in_flow() {
        let queue = Expression::Map(vec![
            ("name".into(), string("mock-orders")),
            ("visibility".into(), Expression::Number("30".into())),
            (
                "tiers".into(),
                Expression::List(vec![string("gold"), string("silver")]),
            ),
            (
                "consumers".into(),
                Expression::List(vec![
                    string("mock-billing"),
                    string("mock-shipping"),
                    string("yes"),
                ]),
            ),
            (
                "targets".into(),
                Expression::List(vec![Expression::Map(vec![
                    ("arn".into(), string("mock-arn")),
                    ("id".into(), string("mock-id")),
                ])]),
            ),
        ]);
        let written = write(&file(vec![BodyItem::Attribute(Attribute::new(
            "queue", queue,
        ))]))
        .unwrap();
        let expected = r#"queue:
  name: mock-orders
  visibility: 30
  tiers: [gold, silver]
  consumers:
    - mock-billing
    - mock-shipping
    - "yes"
  targets:
    - arn: mock-arn
      id: mock-id
"#;
        assert_eq!(written, expected);
    }

    #[test]
    fn a_block_is_refused() {
        assert!(
            write(&file(vec![BodyItem::Block(Block::new(
                "queue",
                Vec::new(),
                Vec::new()
            ))]))
            .is_err()
        );
    }

    #[test]
    fn strings_that_read_back_as_other_values_are_quoted() {
        assert_eq!(scalar_text("30", false), "\"30\"");
        assert_eq!(scalar_text("null", false), "\"null\"");
        assert_eq!(scalar_text("a: b", false), "\"a: b\"");
        assert_eq!(scalar_text("mock-orders", false), "mock-orders");
    }
}
