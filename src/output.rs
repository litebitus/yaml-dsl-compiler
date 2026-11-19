//! The output tree a translator returns: files of blocks, attributes and
//! expressions, in the order they are written.

use std::collections::BTreeSet;
use std::path::PathBuf;

use yaml_dsl_spec::resolved::{LoopBody, Value};

use crate::writer::TargetLanguage;

/// A file to write, relative to the directory of the unit's document.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputFile {
    pub path: PathBuf,
    pub language: TargetLanguage,
    /// Written at the top of the file.
    pub comment: Option<String>,
    pub body: Body,
}

pub type Body = Vec<BodyItem>;

#[derive(Clone, Debug, PartialEq)]
pub enum BodyItem {
    Attribute(Attribute),
    Block(Block),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub value: Expression,
    pub comment: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub block_type: String,
    pub labels: Vec<String>,
    pub body: Body,
    pub comment: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expression {
    Null,
    Bool(bool),
    /// A number as written.
    Number(String),
    String(String),
    List(Vec<Expression>),
    Map(Vec<(ObjectKey, Expression)>),
    Traversal(Traversal),
    Call(FunctionCall),
    Template(Vec<TemplatePart>),
    Conditional(Box<Conditional>),
    For(Box<ForExpression>),
    Unary(UnaryOperator, Box<Expression>),
    Binary(Box<BinaryOperation>),
    /// An expression the source wrote in parentheses.
    Parenthesis(Box<Expression>),
}

/// A map's key: a name, or an expression evaluated for the key.
#[derive(Clone, Debug, PartialEq)]
pub enum ObjectKey {
    Name(String),
    Expression(Expression),
}

/// A reference in the target's syntax: a root and the steps after it.
#[derive(Clone, Debug, PartialEq)]
pub struct Traversal {
    pub root: TraversalRoot,
    pub steps: Vec<TraversalStep>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TraversalRoot {
    Variable(String),
    Expression(Box<Expression>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum TraversalStep {
    Attribute(String),
    Index(Expression),
    /// `.*`
    AttributeSplat,
    /// `[*]`
    FullSplat,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Conditional {
    pub condition: Expression,
    pub if_true: Expression,
    pub if_false: Expression,
}

/// A `for` expression: a list, or a map where `key` is given.
#[derive(Clone, Debug, PartialEq)]
pub struct ForExpression {
    pub key_variable: Option<String>,
    pub value_variable: String,
    pub collection: Expression,
    pub key: Option<Expression>,
    pub value: Expression,
    /// Whether values of one key are grouped into a list, `...`.
    pub grouping: bool,
    pub condition: Option<Expression>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOperator {
    Not,
    Negate,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BinaryOperation {
    pub left: Expression,
    pub operator: BinaryOperator,
    pub right: Expression,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOperator {
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionCall {
    pub function: String,
    pub arguments: Vec<Expression>,
    /// Whether the last argument is expanded into the call's arguments.
    pub expand_final_argument: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TemplatePart {
    Text(String),
    Interpolation(Interpolation),
    If(Box<TemplateIf>),
    For(Box<TemplateFor>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Interpolation {
    pub expression: Expression,
    pub strip: Strip,
}

/// Whether a template tag strips the whitespace before it and after it, `~`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Strip {
    pub before: bool,
    pub after: bool,
}

/// `%{ if }`, with an optional `%{ else }`, to `%{ endif }`.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateIf {
    pub condition: Expression,
    pub if_strip: Strip,
    pub then_parts: Vec<TemplatePart>,
    pub else_branch: Option<(Strip, Vec<TemplatePart>)>,
    pub endif_strip: Strip,
}

/// `%{ for }` to `%{ endfor }`.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateFor {
    pub key_variable: Option<String>,
    pub value_variable: String,
    pub collection: Expression,
    pub for_strip: Strip,
    pub body: Vec<TemplatePart>,
    pub endfor_strip: Strip,
}

impl Attribute {
    pub fn new(name: impl Into<String>, value: Expression) -> Self {
        Self {
            name: name.into(),
            value,
            comment: None,
        }
    }
}

impl Block {
    pub fn new(block_type: impl Into<String>, labels: Vec<String>, body: Body) -> Self {
        Self {
            block_type: block_type.into(),
            labels,
            body,
            comment: None,
        }
    }
}

impl Traversal {
    /// A traversal of attribute names, such as `module.queue.o.arn`.
    pub fn of_names<'n>(root: impl Into<String>, names: impl IntoIterator<Item = &'n str>) -> Self {
        Self {
            root: TraversalRoot::Variable(root.into()),
            steps: names
                .into_iter()
                .map(|name| TraversalStep::Attribute(name.to_owned()))
                .collect(),
        }
    }
}

/// The symbols a loop's variables are written as in its `for` expression.
#[derive(Clone, Debug, PartialEq)]
pub struct LoopSymbols {
    /// The key or index, for a loop of two loop variables.
    pub key: Option<String>,
    pub item: String,
}

/// A loop left for the target as a `for` expression over `collection`, its loop
/// variables written as `symbols` and each value of its body as `write` writes it:
/// a list body yields its item each pass, a map body of one entry its key and
/// value, and a map body of several entries its entries, flattened into one map.
pub fn loop_expression<E>(
    collection: Expression,
    body: &LoopBody,
    symbols: &LoopSymbols,
    mut write: impl FnMut(&Value) -> Result<Expression, E>,
) -> Result<Expression, E> {
    let (key, value) = match body {
        LoopBody::Item(item) => (None, write(item)?),
        LoopBody::Entries(entries) if entries.len() == 1 => {
            (Some(write(&entries[0].key)?), write(&entries[0].value)?)
        }
        LoopBody::Entries(entries) => {
            let mut pass_entries = Vec::with_capacity(entries.len());
            for entry in entries {
                let key = match write(&entry.key)? {
                    read @ Expression::Traversal(_) => Expression::Parenthesis(Box::new(read)),
                    key => key,
                };
                pass_entries.push((ObjectKey::Expression(key), write(&entry.value)?));
            }
            return Ok(flattened_entries(
                symbols,
                collection,
                Expression::Map(pass_entries),
            ));
        }
    };
    Ok(Expression::For(Box::new(ForExpression {
        key_variable: symbols.key.clone(),
        value_variable: symbols.item.clone(),
        collection,
        key,
        value,
        grouping: false,
        condition: None,
    })))
}

/// `{for entry in flatten([for <symbols> in <collection> : [for k, v in <pass> :
/// {k = k, v = v}]]) : entry.k => entry.v}`: the entries of every pass in one map.
fn flattened_entries(
    symbols: &LoopSymbols,
    collection: Expression,
    pass: Expression,
) -> Expression {
    let variable = |name: &str| Expression::Traversal(Traversal::of_names(name, []));
    let pass_pairs = Expression::For(Box::new(ForExpression {
        key_variable: Some("k".to_owned()),
        value_variable: "v".to_owned(),
        collection: pass,
        key: None,
        value: Expression::Map(vec![
            ("k".into(), variable("k")),
            ("v".into(), variable("v")),
        ]),
        grouping: false,
        condition: None,
    }));
    let passes = Expression::For(Box::new(ForExpression {
        key_variable: symbols.key.clone(),
        value_variable: symbols.item.clone(),
        collection,
        key: None,
        value: pass_pairs,
        grouping: false,
        condition: None,
    }));
    let entry_field = |field: &str| Expression::Traversal(Traversal::of_names("entry", [field]));
    Expression::For(Box::new(ForExpression {
        key_variable: None,
        value_variable: "entry".to_owned(),
        collection: Expression::Call(FunctionCall {
            function: "flatten".to_owned(),
            arguments: vec![passes],
            expand_final_argument: false,
        }),
        key: Some(entry_field("k")),
        value: entry_field("v"),
        grouping: false,
        condition: None,
    }))
}

impl Expression {
    /// The variables the expression reads: each traversal's root name that no `for`
    /// around it binds.
    pub fn variables_read(&self) -> BTreeSet<String> {
        let mut variables = BTreeSet::new();
        self.collect_variables_read(&[], &mut variables);
        variables
    }

    fn collect_variables_read(&self, bound: &[&str], variables: &mut BTreeSet<String>) {
        match self {
            Expression::Null
            | Expression::Bool(_)
            | Expression::Number(_)
            | Expression::String(_) => {}
            Expression::List(items) => items
                .iter()
                .for_each(|item| item.collect_variables_read(bound, variables)),
            Expression::Map(entries) => {
                for (key, value) in entries {
                    if let ObjectKey::Expression(key) = key {
                        key.collect_variables_read(bound, variables);
                    }
                    value.collect_variables_read(bound, variables);
                }
            }
            Expression::Traversal(traversal) => {
                match &traversal.root {
                    TraversalRoot::Variable(name) if !bound.contains(&name.as_str()) => {
                        variables.insert(name.clone());
                    }
                    TraversalRoot::Variable(_) => {}
                    TraversalRoot::Expression(root) => {
                        root.collect_variables_read(bound, variables)
                    }
                }
                for step in &traversal.steps {
                    if let TraversalStep::Index(index) = step {
                        index.collect_variables_read(bound, variables);
                    }
                }
            }
            Expression::Call(call) => call
                .arguments
                .iter()
                .for_each(|argument| argument.collect_variables_read(bound, variables)),
            Expression::Template(parts) => template_variables_read(parts, bound, variables),
            Expression::Conditional(conditional) => {
                conditional
                    .condition
                    .collect_variables_read(bound, variables);
                conditional.if_true.collect_variables_read(bound, variables);
                conditional
                    .if_false
                    .collect_variables_read(bound, variables);
            }
            Expression::For(for_expression) => {
                for_expression
                    .collection
                    .collect_variables_read(bound, variables);
                let inner = for_bound(
                    bound,
                    for_expression.key_variable.as_deref(),
                    &for_expression.value_variable,
                );
                let body = [
                    for_expression.key.as_ref(),
                    Some(&for_expression.value),
                    for_expression.condition.as_ref(),
                ];
                for expression in body.into_iter().flatten() {
                    expression.collect_variables_read(&inner, variables);
                }
            }
            Expression::Unary(_, operand) => operand.collect_variables_read(bound, variables),
            Expression::Binary(operation) => {
                operation.left.collect_variables_read(bound, variables);
                operation.right.collect_variables_read(bound, variables);
            }
            Expression::Parenthesis(inner) => inner.collect_variables_read(bound, variables),
        }
    }
}

fn template_variables_read(
    parts: &[TemplatePart],
    bound: &[&str],
    variables: &mut BTreeSet<String>,
) {
    for part in parts {
        match part {
            TemplatePart::Text(_) => {}
            TemplatePart::Interpolation(interpolation) => interpolation
                .expression
                .collect_variables_read(bound, variables),
            TemplatePart::If(template_if) => {
                template_if
                    .condition
                    .collect_variables_read(bound, variables);
                template_variables_read(&template_if.then_parts, bound, variables);
                if let Some((_, else_parts)) = &template_if.else_branch {
                    template_variables_read(else_parts, bound, variables);
                }
            }
            TemplatePart::For(template_for) => {
                template_for
                    .collection
                    .collect_variables_read(bound, variables);
                let inner = for_bound(
                    bound,
                    template_for.key_variable.as_deref(),
                    &template_for.value_variable,
                );
                template_variables_read(&template_for.body, &inner, variables);
            }
        }
    }
}

/// The variables bound inside a `for`: those around it, and its own.
fn for_bound<'b>(
    bound: &[&'b str],
    key_variable: Option<&'b str>,
    value_variable: &'b str,
) -> Vec<&'b str> {
    bound
        .iter()
        .copied()
        .chain(key_variable)
        .chain([value_variable])
        .collect()
}

impl Interpolation {
    pub fn new(expression: Expression) -> Self {
        Self {
            expression,
            strip: Strip::default(),
        }
    }
}

impl From<&str> for ObjectKey {
    fn from(name: &str) -> Self {
        Self::Name(name.to_owned())
    }
}

impl From<String> for ObjectKey {
    fn from(name: String) -> Self {
        Self::Name(name)
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Arc;

    use yaml_dsl_spec::Position;
    use yaml_dsl_spec::resolved::{LoopEntry, ValueKind};

    use super::*;
    use crate::writer::{TargetLanguage, write_file};

    fn read(root: &str) -> Expression {
        Expression::Traversal(Traversal::of_names(root, ["mock"]))
    }

    fn text(value: &str) -> Value {
        Value::string(value, Position::file_start(&Arc::from("mock.yml")))
    }

    /// A body value written as a traversal of its text, such as `item.name`.
    fn written(value: &Value) -> Result<Expression, Infallible> {
        let ValueKind::String(path) = &value.kind else {
            panic!("{value:?}")
        };
        let mut names = path.split('.');
        let root = names.next().unwrap();
        Ok(Expression::Traversal(Traversal::of_names(root, names)))
    }

    fn written_loop(body: &LoopBody) -> String {
        let symbols = LoopSymbols {
            key: Some("key".to_owned()),
            item: "item".to_owned(),
        };
        let expression = loop_expression(read("local"), body, &symbols, written).unwrap();
        let file = OutputFile {
            path: PathBuf::from("mock.tf"),
            language: TargetLanguage::Hcl,
            comment: None,
            body: vec![BodyItem::Attribute(Attribute::new("value", expression))],
        };
        write_file(&file).unwrap()
    }

    #[test]
    fn a_loop_of_one_map_entry_is_a_map_for_expression() {
        let body = LoopBody::Entries(vec![LoopEntry {
            key: text("key"),
            value: text("item.name"),
        }]);
        assert_eq!(
            written_loop(&body),
            "value = { for key, item in local.mock : key => item.name }\n"
        );
    }

    #[test]
    fn a_loop_of_several_map_entries_flattens_every_pass_into_one_map() {
        let body = LoopBody::Entries(vec![
            LoopEntry {
                key: text("key"),
                value: text("item.name"),
            },
            LoopEntry {
                key: text("item.id"),
                value: text("item.arn"),
            },
        ]);
        assert_eq!(
            written_loop(&body),
            concat!(
                "value = {\n",
                "  for entry in flatten(\n",
                "    [\n",
                "      for key, item in local.mock : [\n",
                "        for k, v in { (key) = item.name, (item.id) = item.arn } : { k = k, v = v }\n",
                "      ]\n",
                "    ],\n",
                "  ) : entry.k => entry.v\n",
                "}\n",
            )
        );
    }

    #[test]
    fn a_for_reads_its_collection_and_what_its_body_reads_but_its_own_variables() {
        let for_expression = Expression::For(Box::new(ForExpression {
            key_variable: Some("key".to_owned()),
            value_variable: "item".to_owned(),
            collection: read("item"),
            key: Some(read("key")),
            value: Expression::Call(FunctionCall {
                function: "merge".to_owned(),
                arguments: vec![read("item"), read("module")],
                expand_final_argument: false,
            }),
            grouping: false,
            condition: None,
        }));
        let template = Expression::Template(vec![TemplatePart::Interpolation(Interpolation::new(
            read("local"),
        ))]);
        let expression = Expression::List(vec![for_expression, template]);
        assert_eq!(
            expression.variables_read(),
            BTreeSet::from(["item".to_owned(), "local".to_owned(), "module".to_owned()])
        );
    }
}
