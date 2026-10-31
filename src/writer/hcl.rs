//! HCL, in the style `terraform fmt` leaves unchanged.

use crate::output::{
    Attribute, Block, BodyItem, Expression, FunctionCall, OutputFile, TemplatePart, Traversal,
    TraversalStep,
};
use crate::writer::MAX_LINE_WIDTH;

const INDENT_WIDTH: usize = 2;
const MAX_INLINE_ITEMS: usize = 2;
const HEREDOC_OPENER: &str = "<<-";
const KEYWORDS: [&str; 6] = ["true", "false", "null", "for", "if", "in"];

pub fn write(file: &OutputFile) -> Result<String, String> {
    let mut lines = Vec::new();
    if let Some(comment) = &file.comment {
        lines.extend(comment_lines(comment, 0));
        if !file.body.is_empty() {
            lines.push(String::new());
        }
    }
    let items: Vec<Item<'_>> = file
        .body
        .iter()
        .map(Item::of_body)
        .collect::<Result<_, _>>()?;
    lines.extend(item_lines(&items, 0)?);
    let mut text = lines.join("\n");
    text.push('\n');
    Ok(text)
}

/// A line of a body or of a map: an attribute, a map item or a block.
enum Item<'a> {
    Assignment {
        key: String,
        value: &'a Expression,
        comment: Option<&'a str>,
    },
    Block(&'a Block),
}

impl<'a> Item<'a> {
    fn of_body(item: &'a BodyItem) -> Result<Self, String> {
        match item {
            BodyItem::Attribute(Attribute {
                name,
                value,
                comment,
            }) => {
                if !is_identifier(name) {
                    return Err(format!("`{name}` is not an attribute name"));
                }
                Ok(Item::Assignment {
                    key: name.clone(),
                    value,
                    comment: comment.as_deref(),
                })
            }
            BodyItem::Block(block) => Ok(Item::Block(block)),
        }
    }
}

struct RenderedItem<'a> {
    key: Option<&'a str>,
    comment: Option<&'a str>,
    lines: Vec<String>,
    /// A block or a multi-line map, which has a blank line on each side.
    stands_apart: bool,
}

impl RenderedItem<'_> {
    /// Whether the item's `=` aligns with its neighbors': an assignment written on
    /// one line, or opening a heredoc, which HCL reads as one line.
    fn aligns(&self) -> bool {
        self.key.is_some() && (self.lines.len() == 1 || self.lines[0].starts_with(HEREDOC_OPENER))
    }
}

fn item_lines(items: &[Item<'_>], indent: usize) -> Result<Vec<String>, String> {
    let key_width_bound = items
        .iter()
        .filter_map(|item| match item {
            Item::Assignment { key, .. } => Some(key.len()),
            Item::Block(_) => None,
        })
        .max()
        .unwrap_or(0);
    let mut rendered = Vec::with_capacity(items.len());
    for item in items {
        rendered.push(match item {
            Item::Assignment {
                key,
                value,
                comment,
            } => {
                let lines =
                    expression_lines(value, indent, indent + key_width_bound + " = ".len(), true)?;
                let stands_apart = lines.len() > 1 && matches!(value, Expression::Map(_));
                RenderedItem {
                    key: Some(key.as_str()),
                    comment: *comment,
                    lines,
                    stands_apart,
                }
            }
            Item::Block(block) => RenderedItem {
                key: None,
                comment: block.comment.as_deref(),
                lines: block_lines(block, indent)?,
                stands_apart: true,
            },
        });
    }
    let mut lines = Vec::new();
    let mut index = 0;
    while index < rendered.len() {
        let run_end = alignment_run_end(&rendered, index);
        let key_width = rendered[index..run_end]
            .iter()
            .filter_map(|item| item.key)
            .map(str::len)
            .max()
            .unwrap_or(0);
        for position in index..run_end {
            let item = &rendered[position];
            if position > 0 && (item.stands_apart || rendered[position - 1].stands_apart) {
                lines.push(String::new());
            }
            if let Some(comment) = item.comment {
                lines.extend(comment_lines(comment, indent));
            }
            match item.key {
                Some(key) => {
                    let padding = if item.aligns() {
                        key_width - key.len()
                    } else {
                        0
                    };
                    lines.push(format!(
                        "{}{key}{} = {}",
                        spaces(indent),
                        spaces(padding),
                        item.lines[0]
                    ));
                    lines.extend(item.lines[1..].iter().cloned());
                }
                None => lines.extend(item.lines.iter().cloned()),
            }
        }
        index = run_end;
    }
    Ok(lines)
}

/// The end of the run of aligning assignments starting at `start`; any other item
/// is a run of its own.
fn alignment_run_end(rendered: &[RenderedItem<'_>], start: usize) -> usize {
    if !rendered[start].aligns() {
        return start + 1;
    }
    let mut end = start + 1;
    while end < rendered.len() && rendered[end].aligns() && rendered[end].comment.is_none() {
        end += 1;
    }
    end
}

fn block_lines(block: &Block, indent: usize) -> Result<Vec<String>, String> {
    if !is_identifier(&block.block_type) {
        return Err(format!("`{}` is not a block type", block.block_type));
    }
    let mut header = format!("{}{}", spaces(indent), block.block_type);
    for label in &block.labels {
        header.push(' ');
        header.push_str(&quoted(label));
    }
    if block.body.is_empty() {
        return Ok(vec![format!("{header} {{}}")]);
    }
    let items: Vec<Item<'_>> = block
        .body
        .iter()
        .map(Item::of_body)
        .collect::<Result<_, _>>()?;
    let mut lines = vec![format!("{header} {{")];
    lines.extend(item_lines(&items, indent + INDENT_WIDTH)?);
    lines.push(format!("{}}}", spaces(indent)));
    Ok(lines)
}

/// An expression whose first line follows text already written up to column
/// `used`; later lines are whole lines, the last closing at `indent`.
fn expression_lines(
    expression: &Expression,
    indent: usize,
    used: usize,
    heredoc_allowed: bool,
) -> Result<Vec<String>, String> {
    if heredoc_allowed && let Some(lines) = heredoc_lines(expression, indent)? {
        return Ok(lines);
    }
    if let Some(text) = inline(expression)?
        && (used + text.len() <= MAX_LINE_WIDTH || !breakable(expression))
    {
        return Ok(vec![text]);
    }
    let inner = indent + INDENT_WIDTH;
    match expression {
        Expression::List(items) => {
            let mut lines = vec!["[".to_owned()];
            for item in items {
                let mut item_lines = expression_lines(item, inner, inner, false)?;
                item_lines[0].insert_str(0, &spaces(inner));
                if let Some(last) = item_lines.last_mut() {
                    last.push(',');
                }
                lines.extend(item_lines);
            }
            lines.push(format!("{}]", spaces(indent)));
            Ok(lines)
        }
        Expression::Map(entries) => {
            let items: Vec<Item<'_>> = entries
                .iter()
                .map(|(key, value)| Item::Assignment {
                    key: map_key(key),
                    value,
                    comment: None,
                })
                .collect();
            let mut lines = vec!["{".to_owned()];
            lines.extend(item_lines(&items, inner)?);
            lines.push(format!("{}}}", spaces(indent)));
            Ok(lines)
        }
        Expression::Call(call) => {
            let mut lines = vec![format!("{}(", call.function)];
            let count = call.arguments.len();
            for (position, argument) in call.arguments.iter().enumerate() {
                let mut argument_lines = expression_lines(argument, inner, inner, false)?;
                argument_lines[0].insert_str(0, &spaces(inner));
                let last_argument = position + 1 == count;
                if let Some(last) = argument_lines.last_mut() {
                    last.push_str(if last_argument && call.expand_final_argument {
                        "..."
                    } else {
                        ","
                    });
                }
                lines.extend(argument_lines);
            }
            lines.push(format!("{})", spaces(indent)));
            Ok(lines)
        }
        _ => Ok(vec![inline(expression)?.unwrap_or_default()]),
    }
}

fn breakable(expression: &Expression) -> bool {
    match expression {
        Expression::List(items) => !items.is_empty(),
        Expression::Map(entries) => !entries.is_empty(),
        Expression::Call(call) => !call.arguments.is_empty(),
        _ => false,
    }
}

/// The expression on one line, or `None` for a list or map of more items than a
/// line holds by style.
fn inline(expression: &Expression) -> Result<Option<String>, String> {
    Ok(Some(match expression {
        Expression::Null => "null".to_owned(),
        Expression::Bool(flag) => flag.to_string(),
        Expression::Number(text) => number(text)?,
        Expression::String(text) => quoted(text),
        Expression::List(items) => {
            if items.len() > MAX_INLINE_ITEMS {
                return Ok(None);
            }
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                let Some(part) = inline(item)? else {
                    return Ok(None);
                };
                parts.push(part);
            }
            format!("[{}]", parts.join(", "))
        }
        Expression::Map(entries) => {
            if entries.len() > MAX_INLINE_ITEMS {
                return Ok(None);
            }
            if entries.is_empty() {
                return Ok(Some("{}".to_owned()));
            }
            let mut parts = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                let Some(part) = inline(value)? else {
                    return Ok(None);
                };
                parts.push(format!("{} = {part}", map_key(key)));
            }
            format!("{{ {} }}", parts.join(", "))
        }
        Expression::Traversal(traversal) => traversal_text(traversal)?,
        Expression::Call(call) => {
            let Some(arguments) = call_arguments_inline(call)? else {
                return Ok(None);
            };
            format!("{}({arguments})", call.function)
        }
        Expression::Template(parts) => match parts.as_slice() {
            [TemplatePart::Interpolation(expression)] => return inline(expression),
            _ => {
                let mut text = String::from("\"");
                for part in parts {
                    match part {
                        TemplatePart::Text(literal) => text.push_str(&escape_quoted(literal)),
                        TemplatePart::Interpolation(expression) => {
                            let Some(inner) = inline(expression)? else {
                                return Ok(None);
                            };
                            text.push_str(&format!("${{{inner}}}"));
                        }
                    }
                }
                text.push('"');
                text
            }
        },
    }))
}

fn call_arguments_inline(call: &FunctionCall) -> Result<Option<String>, String> {
    let mut parts = Vec::with_capacity(call.arguments.len());
    for argument in &call.arguments {
        let Some(part) = inline(argument)? else {
            return Ok(None);
        };
        parts.push(part);
    }
    let mut text = parts.join(", ");
    if call.expand_final_argument && !parts.is_empty() {
        text.push_str("...");
    }
    Ok(Some(text))
}

fn traversal_text(traversal: &Traversal) -> Result<String, String> {
    if !is_identifier(&traversal.root) {
        return Err(format!("`{}` is not a traversal root", traversal.root));
    }
    let mut text = traversal.root.clone();
    for step in &traversal.steps {
        match step {
            TraversalStep::Attribute(name)
                if is_identifier(name) && !KEYWORDS.contains(&name.as_str()) =>
            {
                text.push('.');
                text.push_str(name);
            }
            TraversalStep::Attribute(name) => text.push_str(&format!("[{}]", quoted(name))),
            TraversalStep::Index(index) => {
                let inner = inline(index)?.ok_or("a traversal index is written on one line")?;
                text.push_str(&format!("[{inner}]"));
            }
        }
    }
    Ok(text)
}

/// A string or template holding a line break, as an indented heredoc.
fn heredoc_lines(expression: &Expression, indent: usize) -> Result<Option<Vec<String>>, String> {
    let text = match expression {
        Expression::String(text) if text.contains('\n') => escape_template(text),
        Expression::Template(parts)
            if parts.iter().any(
                |part| matches!(part, TemplatePart::Text(literal) if literal.contains('\n')),
            ) =>
        {
            let mut text = String::new();
            for part in parts {
                match part {
                    TemplatePart::Text(literal) => text.push_str(&escape_template(literal)),
                    TemplatePart::Interpolation(expression) => {
                        let inner =
                            inline(expression)?.ok_or("an interpolation is written on one line")?;
                        text.push_str(&format!("${{{inner}}}"));
                    }
                }
            }
            text
        }
        _ => return Ok(None),
    };
    let body = text.strip_suffix('\n').unwrap_or(&text);
    let mut marker = "EOT".to_owned();
    while body.lines().any(|line| line.trim() == marker) {
        marker.push('T');
    }
    let inner = spaces(indent + INDENT_WIDTH);
    let mut lines = vec![format!("{HEREDOC_OPENER}{marker}")];
    lines.extend(body.split('\n').map(|line| {
        if line.is_empty() {
            String::new()
        } else {
            format!("{inner}{line}")
        }
    }));
    lines.push(format!("{}{marker}", spaces(indent)));
    Ok(Some(lines))
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

fn map_key(key: &str) -> String {
    if is_identifier(key) && !KEYWORDS.contains(&key) {
        key.to_owned()
    } else {
        quoted(key)
    }
}

fn is_identifier(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

fn number(text: &str) -> Result<String, String> {
    let unsigned = text.strip_prefix('+').unwrap_or(text);
    let (negative, digits) = match unsigned.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, unsigned),
    };
    let sign = if negative { "-" } else { "" };
    for (prefix, radix) in [("0x", 16), ("0o", 8)] {
        if let Some(rest) = digits.strip_prefix(prefix) {
            let value = u128::from_str_radix(rest, radix)
                .map_err(|_| format!("`{text}` is not a number"))?;
            return Ok(format!("{sign}{value}"));
        }
    }
    if is_decimal(digits) {
        return Ok(format!("{sign}{digits}"));
    }
    match unsigned.parse::<f64>() {
        Ok(value) if value.is_finite() => Ok(format!("{value}")),
        _ => Err(format!("`{text}` is not a number HCL writes")),
    }
}

/// Whether `text` is a number in HCL's own grammar: digits, a fraction, an exponent.
fn is_decimal(text: &str) -> bool {
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(index) => (&text[..index], Some(&text[index + 1..])),
        None => (text, None),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (mantissa, None),
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    digits(whole)
        && fraction.is_none_or(digits)
        && exponent
            .is_none_or(|exponent| digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)))
}

fn quoted(text: &str) -> String {
    format!("\"{}\"", escape_quoted(text))
}

fn escape_quoted(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in escape_template(text).chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            control if control.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", control as u32))
            }
            other => escaped.push(other),
        }
    }
    escaped
}

/// Literal text in a template, with the sequences that would start an
/// interpolation or a directive escaped.
fn escape_template(text: &str) -> String {
    text.replace("${", "$${").replace("%{", "%%{")
}

fn spaces(count: usize) -> String {
    " ".repeat(count)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::writer::TargetLanguage;

    fn string(text: &str) -> Expression {
        Expression::String(text.to_owned())
    }

    fn file(body: Vec<BodyItem>) -> OutputFile {
        OutputFile {
            path: PathBuf::from("mock.tf"),
            language: TargetLanguage::Hcl,
            comment: None,
            body,
        }
    }

    #[test]
    fn writes_the_documented_style() {
        let config = Expression::Map(vec![
            ("name".into(), string("mock-orders")),
            ("visibility".into(), Expression::Number("30".into())),
            (
                "kms_key".into(),
                Expression::Traversal(Traversal::of_names("module", ["key_mock_main", "o", "arn"])),
            ),
            (
                "consumers".into(),
                Expression::List(vec![
                    string("mock-billing"),
                    string("mock-shipping"),
                    string("mock-audit"),
                ]),
            ),
            (
                "tags".into(),
                Expression::Map(vec![
                    ("team".into(), string("mock")),
                    ("tier".into(), string("gold")),
                ]),
            ),
        ]);
        let module = Block::new(
            "module",
            vec!["queue_mock_orders".into()],
            vec![
                BodyItem::Attribute(Attribute::new(
                    "source",
                    string("registry.example.com/mock/components//queue"),
                )),
                BodyItem::Attribute(Attribute::new("version", string("1.2.3"))),
                BodyItem::Attribute(Attribute::new("config", config)),
            ],
        );
        let expected = r#"module "queue_mock_orders" {
  source  = "registry.example.com/mock/components//queue"
  version = "1.2.3"

  config = {
    name       = "mock-orders"
    visibility = 30
    kms_key    = module.key_mock_main.o.arn
    consumers = [
      "mock-billing",
      "mock-shipping",
      "mock-audit",
    ]
    tags = { team = "mock", tier = "gold" }
  }
}
"#;
        assert_eq!(
            write(&file(vec![BodyItem::Block(module)])).unwrap(),
            expected
        );
    }

    #[test]
    fn a_call_too_wide_for_its_line_takes_one_argument_per_line() {
        let long = "mock-".repeat(18);
        let call = Expression::Call(FunctionCall {
            function: "merge".into(),
            arguments: vec![
                string(&long),
                Expression::Traversal(Traversal::of_names("local", ["mock_list"])),
            ],
            expand_final_argument: true,
        });
        let written = write(&file(vec![BodyItem::Attribute(Attribute::new(
            "value", call,
        ))]))
        .unwrap();
        assert_eq!(
            written,
            format!("value = merge(\n  \"{long}\",\n  local.mock_list...\n)\n")
        );
    }

    #[test]
    fn a_multi_line_string_is_a_heredoc_with_interpolation_escaped() {
        let written = write(&file(vec![BodyItem::Attribute(Attribute::new(
            "script",
            string("echo ${HOME}\nexit 0\n"),
        ))]))
        .unwrap();
        assert_eq!(written, "script = <<-EOT\n  echo $${HOME}\n  exit 0\nEOT\n");
    }

    #[test]
    fn numbers_are_written_in_hcl_grammar() {
        assert_eq!(number("0x1F").unwrap(), "31");
        assert_eq!(number("+12").unwrap(), "12");
        assert_eq!(number("1.5e3").unwrap(), "1.5e3");
        assert!(number(".inf").is_err());
    }

    #[test]
    fn keys_that_are_not_identifiers_are_quoted() {
        assert_eq!(map_key("mock-key"), "mock-key");
        assert_eq!(map_key("mock.key"), "\"mock.key\"");
        assert_eq!(map_key("for"), "\"for\"");
    }
}
