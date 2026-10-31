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
    Map(Vec<(String, Expression)>),
    Traversal(Traversal),
    Call(FunctionCall),
    Template(Vec<TemplatePart>),
}

/// A reference in the target's syntax: a root name and the steps after it.
#[derive(Clone, Debug, PartialEq)]
pub struct Traversal {
    pub root: String,
    pub steps: Vec<TraversalStep>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TraversalStep {
    Attribute(String),
    Index(Expression),
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
    Interpolation(Expression),
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
            root: root.into(),
            steps: names
                .into_iter()
                .map(|name| TraversalStep::Attribute(name.to_owned()))
                .collect(),
        }
    }
}
