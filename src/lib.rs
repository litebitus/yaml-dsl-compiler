//! Compiles YAML DSLs defined by configuration into code.

mod compiler;
mod declarations;
pub mod definition;
mod diagnostic;
mod discover;
mod fold;
mod function_table;
mod json_path;
mod macros;
mod node;
pub mod output;
mod position;
mod reference_scan;
mod resolve;
pub mod resolved;
mod schema;
mod source;
pub mod writer;
pub mod yaml_reader;

pub use compiler::{
    BuiltinValues, CompiledUnit, Compiler, GeneratedFile, ResolvedUnit, Translator,
};
pub use definition::DslDefinition;
pub use diagnostic::Diagnostic;
pub use discover::Unit;
pub use node::{Entry, Node, NodeKind};
pub use position::Position;
pub use source::{FileSource, MemorySource};
pub use writer::TargetLanguage;
