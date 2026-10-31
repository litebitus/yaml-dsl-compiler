//! HCL, in the style `terraform fmt` leaves unchanged.

use crate::output::{
    Attribute, BinaryOperator, Block, BodyItem, Conditional, Expression, ForExpression,
    FunctionCall, ObjectKey, OutputFile, Strip, TemplatePart, Traversal, TraversalRoot,
    TraversalStep, UnaryOperator,
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
                let stands_apart = lines.len() > 1
                    && match value {
                        Expression::Map(_) => true,
                        Expression::For(for_expression) => for_expression.key.is_some(),
                        _ => false,
                    };
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
                .map(|(key, value)| {
                    Ok(Item::Assignment {
                        key: object_key(key)?,
                        value,
                        comment: None,
                    })
                })
                .collect::<Result<_, String>>()?;
            let mut lines = vec!["{".to_owned()];
            lines.extend(item_lines(&items, inner)?);
            lines.push(format!("{}}}", spaces(indent)));
            Ok(lines)
        }
        Expression::Call(call) => {
            if let Some(lines) = hugging_call_lines(call, indent, used)? {
                return Ok(lines);
            }
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
        Expression::For(for_expression) => for_lines(for_expression, indent),
        Expression::Conditional(conditional) => conditional_lines(conditional, indent, used),
        _ => Ok(vec![flat(expression)?]),
    }
}

/// A call whose last argument is a map or a list, written around it: its other
/// arguments and the map's opening on the call's line, and its closing with the
/// map's, `f(a, {` to `})`.
fn hugging_call_lines(
    call: &FunctionCall,
    indent: usize,
    used: usize,
) -> Result<Option<Vec<String>>, String> {
    let Some((last, leading)) = call.arguments.split_last() else {
        return Ok(None);
    };
    if !matches!(last, Expression::Map(entries) if !entries.is_empty())
        && !matches!(last, Expression::List(items) if !items.is_empty())
    {
        return Ok(None);
    }
    let mut opening = format!("{}(", call.function);
    for argument in leading {
        let Some(text) = inline(argument)? else {
            return Ok(None);
        };
        opening.push_str(&text);
        opening.push_str(", ");
    }
    if used + opening.len() + 1 > MAX_LINE_WIDTH {
        return Ok(None);
    }
    let mut lines = expression_lines(last, indent, used + opening.len(), false)?;
    if lines.len() == 1 {
        return Ok(None);
    }
    lines[0].insert_str(0, &opening);
    let closing = lines.last_mut().expect("a broken map has lines");
    if call.expand_final_argument {
        closing.push_str("...");
    }
    closing.push(')');
    Ok(Some(lines))
}

/// A conditional too wide for one line: its condition on the first line, and each
/// branch broken as any value is.
fn conditional_lines(
    conditional: &Conditional,
    indent: usize,
    used: usize,
) -> Result<Vec<String>, String> {
    let needs_parentheses = precedence(&conditional.condition) < OPERAND_PRECEDENCE
        || precedence(&conditional.if_true) < OPERAND_PRECEDENCE;
    if needs_parentheses {
        return Ok(vec![flat(&Expression::Conditional(Box::new(
            conditional.clone(),
        )))?]);
    }
    let condition = format!("{} ? ", flat(&conditional.condition)?);
    let mut lines = expression_lines(&conditional.if_true, indent, used + condition.len(), false)?;
    lines[0].insert_str(0, &condition);
    let last_width = if lines.len() == 1 {
        used + lines[0].len()
    } else {
        lines.last().map_or(0, String::len)
    };
    let mut otherwise = expression_lines(&conditional.if_false, indent, last_width + 3, false)?;
    let last = lines.last_mut().expect("a branch has a line");
    last.push_str(" : ");
    last.push_str(&otherwise.remove(0));
    lines.extend(otherwise);
    Ok(lines)
}

/// A `for` expression too wide for one line: its brackets on lines of their own,
/// and its collection and value broken as any value is.
fn for_lines(for_expression: &ForExpression, indent: usize) -> Result<Vec<String>, String> {
    let inner = indent + INDENT_WIDTH;
    let (open, close) = brackets(for_expression);
    let variables = match &for_expression.key_variable {
        Some(key) => format!("{key}, {}", for_expression.value_variable),
        None => for_expression.value_variable.clone(),
    };
    let head = format!("{}for {variables} in ", spaces(inner));
    let mut lines = vec![open.to_owned()];
    let mut collection = expression_lines(&for_expression.collection, inner, head.len(), false)?;
    collection[0].insert_str(0, &head);
    let mut prefix = collection.pop().expect("a collection has a line");
    lines.extend(collection);
    prefix.push_str(" : ");
    if let Some(key) = &for_expression.key {
        prefix.push_str(&format!("{} => ", flat(key)?));
    }
    let mut value_lines = expression_lines(&for_expression.value, inner, prefix.len(), false)?;
    value_lines[0].insert_str(0, &prefix);
    let last = value_lines.last_mut().expect("an expression has a line");
    last.push_str(&for_tail(for_expression)?);
    lines.extend(value_lines);
    lines.push(format!("{}{close}", spaces(indent)));
    Ok(lines)
}

fn brackets(for_expression: &ForExpression) -> (&'static str, &'static str) {
    if for_expression.key.is_some() {
        ("{", "}")
    } else {
        ("[", "]")
    }
}

/// `for key, value in collection :`
fn for_head(for_expression: &ForExpression) -> Result<String, String> {
    let variables = match &for_expression.key_variable {
        Some(key) => format!("{key}, {}", for_expression.value_variable),
        None => for_expression.value_variable.clone(),
    };
    Ok(format!(
        "for {variables} in {} :",
        flat(&for_expression.collection)?
    ))
}

/// The grouping marker and the condition after a `for` expression's value.
fn for_tail(for_expression: &ForExpression) -> Result<String, String> {
    let mut tail = String::new();
    if for_expression.grouping {
        tail.push_str("...");
    }
    if let Some(condition) = &for_expression.condition {
        tail.push_str(&format!(" if {}", flat(condition)?));
    }
    Ok(tail)
}

fn breakable(expression: &Expression) -> bool {
    match expression {
        Expression::List(items) => !items.is_empty(),
        Expression::Map(entries) => !entries.is_empty(),
        Expression::Call(call) => !call.arguments.is_empty(),
        Expression::For(_) | Expression::Conditional(_) => true,
        _ => false,
    }
}

/// The expression on one line, or `None` for a list or map of more items than a
/// line holds by style.
fn inline(expression: &Expression) -> Result<Option<String>, String> {
    inline_within(expression, Some(MAX_INLINE_ITEMS))
}

/// The expression on one line, whatever its lists and maps hold.
fn flat(expression: &Expression) -> Result<String, String> {
    Ok(inline_within(expression, None)?.expect("a flat expression is always written"))
}

fn inline_within(
    expression: &Expression,
    item_limit: Option<usize>,
) -> Result<Option<String>, String> {
    let over_limit = |count: usize| item_limit.is_some_and(|limit| count > limit);
    let operand = |operand: &Expression, minimum: u8| -> Result<Option<String>, String> {
        let Some(text) = inline_within(operand, item_limit)? else {
            return Ok(None);
        };
        Ok(Some(if precedence(operand) < minimum {
            format!("({text})")
        } else {
            text
        }))
    };
    Ok(Some(match expression {
        Expression::Null => "null".to_owned(),
        Expression::Bool(flag) => flag.to_string(),
        Expression::Number(text) => number(text)?,
        Expression::String(text) => quoted(text),
        Expression::List(items) => {
            if over_limit(items.len()) {
                return Ok(None);
            }
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                let Some(part) = inline_within(item, item_limit)? else {
                    return Ok(None);
                };
                parts.push(part);
            }
            format!("[{}]", parts.join(", "))
        }
        Expression::Map(entries) => {
            if over_limit(entries.len()) {
                return Ok(None);
            }
            if entries.is_empty() {
                return Ok(Some("{}".to_owned()));
            }
            let mut parts = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                let Some(part) = inline_within(value, item_limit)? else {
                    return Ok(None);
                };
                parts.push(format!("{} = {part}", object_key(key)?));
            }
            format!("{{ {} }}", parts.join(", "))
        }
        Expression::Traversal(traversal) => traversal_text(traversal)?,
        Expression::Call(call) => {
            let Some(arguments) = call_arguments_inline(call, item_limit)? else {
                return Ok(None);
            };
            format!("{}({arguments})", call.function)
        }
        Expression::Template(parts) => match parts.as_slice() {
            [TemplatePart::Interpolation(interpolation)]
                if interpolation.strip == Strip::default() =>
            {
                return inline_within(&interpolation.expression, item_limit);
            }
            _ => format!("\"{}\"", template_text(parts, escape_quoted)?),
        },
        Expression::Conditional(conditional) => {
            let (Some(condition), Some(if_true), Some(if_false)) = (
                operand(&conditional.condition, OPERAND_PRECEDENCE)?,
                operand(&conditional.if_true, OPERAND_PRECEDENCE)?,
                operand(&conditional.if_false, CONDITIONAL_PRECEDENCE)?,
            ) else {
                return Ok(None);
            };
            format!("{condition} ? {if_true} : {if_false}")
        }
        Expression::For(for_expression) => {
            let Some(value) = inline_within(&for_expression.value, item_limit)? else {
                return Ok(None);
            };
            let head = for_head(for_expression)?;
            let tail = for_tail(for_expression)?;
            match &for_expression.key {
                Some(key) => format!("{{ {head} {} => {value}{tail} }}", flat(key)?),
                None => format!("[{head} {value}{tail}]"),
            }
        }
        Expression::Unary(operator, operand_expression) => {
            let Some(text) = operand(operand_expression, UNARY_PRECEDENCE)? else {
                return Ok(None);
            };
            let symbol = match operator {
                UnaryOperator::Not => "!",
                UnaryOperator::Negate => "-",
            };
            format!("{symbol}{text}")
        }
        Expression::Binary(operation) => {
            let level = operator_precedence(operation.operator);
            let (Some(left), Some(right)) = (
                operand(&operation.left, level)?,
                operand(&operation.right, level + 1)?,
            ) else {
                return Ok(None);
            };
            format!("{left} {} {right}", operator_symbol(operation.operator))
        }
        Expression::Parenthesis(inner) => {
            let Some(text) = inline_within(inner, item_limit)? else {
                return Ok(None);
            };
            format!("({text})")
        }
    }))
}

const CONDITIONAL_PRECEDENCE: u8 = 0;
const OPERAND_PRECEDENCE: u8 = 1;
const UNARY_PRECEDENCE: u8 = 7;
const PRIMARY_PRECEDENCE: u8 = 8;

/// How tightly an expression binds: an operand binding less tightly than its
/// operator needs parentheses.
fn precedence(expression: &Expression) -> u8 {
    match expression {
        Expression::Conditional(_) => CONDITIONAL_PRECEDENCE,
        Expression::Binary(operation) => operator_precedence(operation.operator),
        Expression::Unary(..) => UNARY_PRECEDENCE,
        _ => PRIMARY_PRECEDENCE,
    }
}

fn operator_precedence(operator: BinaryOperator) -> u8 {
    match operator {
        BinaryOperator::Or => 1,
        BinaryOperator::And => 2,
        BinaryOperator::Equal | BinaryOperator::NotEqual => 3,
        BinaryOperator::Less
        | BinaryOperator::LessOrEqual
        | BinaryOperator::Greater
        | BinaryOperator::GreaterOrEqual => 4,
        BinaryOperator::Add | BinaryOperator::Subtract => 5,
        BinaryOperator::Multiply | BinaryOperator::Divide | BinaryOperator::Modulo => 6,
    }
}

fn operator_symbol(operator: BinaryOperator) -> &'static str {
    match operator {
        BinaryOperator::Or => "||",
        BinaryOperator::And => "&&",
        BinaryOperator::Equal => "==",
        BinaryOperator::NotEqual => "!=",
        BinaryOperator::Less => "<",
        BinaryOperator::LessOrEqual => "<=",
        BinaryOperator::Greater => ">",
        BinaryOperator::GreaterOrEqual => ">=",
        BinaryOperator::Add => "+",
        BinaryOperator::Subtract => "-",
        BinaryOperator::Multiply => "*",
        BinaryOperator::Divide => "/",
        BinaryOperator::Modulo => "%",
    }
}

fn call_arguments_inline(
    call: &FunctionCall,
    item_limit: Option<usize>,
) -> Result<Option<String>, String> {
    let mut parts = Vec::with_capacity(call.arguments.len());
    for argument in &call.arguments {
        let Some(part) = inline_within(argument, item_limit)? else {
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
    let mut text = match &traversal.root {
        TraversalRoot::Variable(name) if is_identifier(name) => name.clone(),
        TraversalRoot::Variable(name) => return Err(format!("`{name}` is not a traversal root")),
        TraversalRoot::Expression(expression) => {
            let root = flat(expression)?;
            if precedence(expression) < PRIMARY_PRECEDENCE {
                format!("({root})")
            } else {
                root
            }
        }
    };
    for step in &traversal.steps {
        match step {
            TraversalStep::Attribute(name)
                if is_identifier(name) && !KEYWORDS.contains(&name.as_str()) =>
            {
                text.push('.');
                text.push_str(name);
            }
            TraversalStep::Attribute(name) => text.push_str(&format!("[{}]", quoted(name))),
            TraversalStep::Index(index) => text.push_str(&format!("[{}]", flat(index)?)),
            TraversalStep::AttributeSplat => text.push_str(".*"),
            TraversalStep::FullSplat => text.push_str("[*]"),
        }
    }
    Ok(text)
}

/// A template's text, its literal parts escaped by `escape`.
fn template_text(parts: &[TemplatePart], escape: fn(&str) -> String) -> Result<String, String> {
    let mut text = String::new();
    for part in parts {
        match part {
            TemplatePart::Text(literal) => text.push_str(&escape(literal)),
            TemplatePart::Interpolation(interpolation) => {
                let inner = flat(&interpolation.expression)?;
                text.push_str(&template_tag("$", &inner, interpolation.strip));
            }
            TemplatePart::If(directive) => {
                let condition = format!("if {}", flat(&directive.condition)?);
                text.push_str(&template_tag("%", &condition, directive.if_strip));
                text.push_str(&template_text(&directive.then_parts, escape)?);
                if let Some((strip, else_parts)) = &directive.else_branch {
                    text.push_str(&template_tag("%", "else", *strip));
                    text.push_str(&template_text(else_parts, escape)?);
                }
                text.push_str(&template_tag("%", "endif", directive.endif_strip));
            }
            TemplatePart::For(directive) => {
                let variables = match &directive.key_variable {
                    Some(key) => format!("{key}, {}", directive.value_variable),
                    None => directive.value_variable.clone(),
                };
                let head = format!("for {variables} in {}", flat(&directive.collection)?);
                text.push_str(&template_tag("%", &head, directive.for_strip));
                text.push_str(&template_text(&directive.body, escape)?);
                text.push_str(&template_tag("%", "endfor", directive.endfor_strip));
            }
        }
    }
    Ok(text)
}

/// `${inner}` or `%{ inner }`, with `~` where the tag strips whitespace.
fn template_tag(sigil: &str, inner: &str, strip: Strip) -> String {
    let padding = if sigil == "%" { " " } else { "" };
    let before = if strip.before { "~ " } else { padding };
    let after = if strip.after { " ~" } else { padding };
    format!("{sigil}{{{before}{inner}{after}}}")
}

fn holds_line_break(parts: &[TemplatePart]) -> bool {
    parts.iter().any(|part| match part {
        TemplatePart::Text(literal) => literal.contains('\n'),
        TemplatePart::Interpolation(_) => false,
        TemplatePart::If(directive) => {
            holds_line_break(&directive.then_parts)
                || directive
                    .else_branch
                    .as_ref()
                    .is_some_and(|(_, else_parts)| holds_line_break(else_parts))
        }
        TemplatePart::For(directive) => holds_line_break(&directive.body),
    })
}

/// A string or template holding a line break, as an indented heredoc.
fn heredoc_lines(expression: &Expression, indent: usize) -> Result<Option<Vec<String>>, String> {
    let text = match expression {
        Expression::String(text) if text.contains('\n') => escape_template(text),
        Expression::Template(parts) if holds_line_break(parts) => {
            template_text(parts, escape_template)?
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

/// A map key: a name, a template as itself, any other expression in parentheses,
/// so HCL evaluates it rather than reading it as a name.
fn object_key(key: &ObjectKey) -> Result<String, String> {
    Ok(match key {
        ObjectKey::Name(name) => map_key(name),
        ObjectKey::Expression(Expression::Template(parts)) => {
            format!("\"{}\"", template_text(parts, escape_quoted)?)
        }
        ObjectKey::Expression(
            expression @ (Expression::String(_) | Expression::Parenthesis(_)),
        ) => flat(expression)?,
        ObjectKey::Expression(expression) => format!("({})", flat(expression)?),
    })
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

    fn variable(name: &str) -> Expression {
        Expression::Traversal(Traversal::of_names(name, []))
    }

    fn binary(left: Expression, operator: BinaryOperator, right: Expression) -> Expression {
        Expression::Binary(Box::new(crate::output::BinaryOperation {
            left,
            operator,
            right,
        }))
    }

    fn written_value(value: Expression) -> String {
        write(&file(vec![BodyItem::Attribute(Attribute::new(
            "value", value,
        ))]))
        .unwrap()
    }

    #[test]
    fn operations_take_parentheses_only_where_precedence_needs_them() {
        let sum = binary(variable("a"), BinaryOperator::Add, variable("b"));
        let product = binary(sum.clone(), BinaryOperator::Multiply, variable("c"));
        assert_eq!(written_value(product), "value = (a + b) * c\n");
        let left_first = binary(sum.clone(), BinaryOperator::Subtract, variable("c"));
        assert_eq!(written_value(left_first), "value = a + b - c\n");
        let right_first = binary(variable("c"), BinaryOperator::Subtract, sum);
        assert_eq!(written_value(right_first), "value = c - (a + b)\n");
        let negated = Expression::Unary(
            UnaryOperator::Not,
            Box::new(binary(variable("a"), BinaryOperator::And, variable("b"))),
        );
        assert_eq!(written_value(negated), "value = !(a && b)\n");
    }

    #[test]
    fn a_conditional_and_a_splat_are_written_on_one_line() {
        let conditional = Expression::Conditional(Box::new(crate::output::Conditional {
            condition: binary(variable("a"), BinaryOperator::Equal, string("x")),
            if_true: Expression::Traversal(Traversal {
                root: TraversalRoot::Variable("module".into()),
                steps: vec![
                    TraversalStep::Attribute("mock".into()),
                    TraversalStep::FullSplat,
                    TraversalStep::Attribute("arn".into()),
                ],
            }),
            if_false: Expression::List(Vec::new()),
        }));
        assert_eq!(
            written_value(conditional),
            "value = a == \"x\" ? module.mock[*].arn : []\n"
        );
    }

    #[test]
    fn a_call_hugs_its_map_and_a_conditional_breaks_in_its_branches() {
        let long = "x".repeat(40);
        let map = Expression::Map(vec![
            ("bucket".into(), string(&long)),
            ("region".into(), string("mock-region-1")),
            ("role".into(), string(&long)),
        ]);
        let hugging = Expression::Call(FunctionCall {
            function: "tomap".into(),
            arguments: vec![map],
            expand_final_argument: false,
        });
        let conditional = Expression::Conditional(Box::new(crate::output::Conditional {
            condition: binary(variable("kind"), BinaryOperator::Equal, string("remote")),
            if_true: hugging,
            if_false: Expression::Call(FunctionCall {
                function: "tomap".into(),
                arguments: vec![Expression::Map(Vec::new())],
                expand_final_argument: false,
            }),
        }));
        assert_eq!(
            written_value(conditional),
            format!(
                "value = kind == \"remote\" ? tomap({{\n  bucket = \"{long}\"\n  \
                 region = \"mock-region-1\"\n  role   = \"{long}\"\n}}) : tomap({{}})\n"
            )
        );
    }

    #[test]
    fn a_for_expression_breaks_its_collection_when_the_line_is_too_wide() {
        let long = "x".repeat(60);
        let for_expression = Expression::For(Box::new(ForExpression {
            key_variable: None,
            value_variable: "name".into(),
            collection: Expression::List(vec![string(&long), string(&long), string(&long)]),
            key: None,
            value: variable("name"),
            grouping: false,
            condition: None,
        }));
        assert_eq!(
            written_value(for_expression),
            format!(
                "value = [\n  for name in [\n    \"{long}\",\n    \"{long}\",\n    \"{long}\",\n  ] \
                 : name\n]\n"
            )
        );
    }

    #[test]
    fn a_for_expression_breaks_its_value_when_the_line_is_too_wide() {
        let long = "x".repeat(60);
        let for_expression = |value: Expression| {
            Expression::For(Box::new(ForExpression {
                key_variable: Some("name".into()),
                value_variable: "lookup".into(),
                collection: variable("lookups"),
                key: Some(variable("name")),
                value,
                grouping: false,
                condition: Some(variable("enabled")),
            }))
        };
        assert_eq!(
            written_value(for_expression(variable("lookup"))),
            "value = { for name, lookup in lookups : name => lookup if enabled }\n"
        );
        let merge = Expression::Call(FunctionCall {
            function: "merge".into(),
            arguments: vec![variable("lookup"), string(&long)],
            expand_final_argument: false,
        });
        assert_eq!(
            written_value(for_expression(merge)),
            format!(
                "value = {{\n  for name, lookup in lookups : name => merge(\n    lookup,\n    \
                 \"{long}\",\n  ) if enabled\n}}\n"
            )
        );
    }

    #[test]
    fn a_template_writes_its_directives_and_strip_markers() {
        let template = Expression::Template(vec![
            TemplatePart::Text("mock-".into()),
            TemplatePart::If(Box::new(crate::output::TemplateIf {
                condition: variable("tier"),
                if_strip: Strip::default(),
                then_parts: vec![TemplatePart::Interpolation(crate::output::Interpolation {
                    expression: variable("tier"),
                    strip: Strip {
                        before: true,
                        after: false,
                    },
                })],
                else_branch: Some((Strip::default(), vec![TemplatePart::Text("none".into())])),
                endif_strip: Strip::default(),
            })),
        ]);
        assert_eq!(
            written_value(template),
            "value = \"mock-%{ if tier }${~ tier}%{ else }none%{ endif }\"\n"
        );
    }

    #[test]
    fn an_expression_key_is_evaluated_and_a_template_key_stays_quoted() {
        let map = Expression::Map(vec![
            (ObjectKey::Expression(variable("mock_key")), string("a")),
            (
                ObjectKey::Expression(Expression::Template(vec![
                    TemplatePart::Interpolation(crate::output::Interpolation::new(variable("env"))),
                    TemplatePart::Text("-b".into()),
                ])),
                string("b"),
            ),
        ]);
        assert_eq!(
            written_value(map),
            "value = { (mock_key) = \"a\", \"${env}-b\" = \"b\" }\n"
        );
    }

    #[test]
    fn keys_that_are_not_identifiers_are_quoted() {
        assert_eq!(map_key("mock-key"), "mock-key");
        assert_eq!(map_key("mock.key"), "\"mock.key\"");
        assert_eq!(map_key("for"), "\"for\"");
    }
}
