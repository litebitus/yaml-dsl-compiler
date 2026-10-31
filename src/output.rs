//! The output tree a translator returns: files of blocks, attributes and
//! expressions, in the order they are written.

use std::path::PathBuf;

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
