//! Compiles YAML DSLs defined by configuration into code. Reading a DSL's
//! definition, its documents' YAML and their validation are `yaml-dsl-spec`'s;
//! this crate folds, expands and resolves a DSL's documents, and writes what a
//! translator turns them into.

mod compiler;
mod declarations;
mod discover;
mod fold;
mod macros;
pub mod output;
mod reference_scan;
mod resolve;
mod schema_provider;
mod source;
pub mod writer;

pub use compiler::{
    BuiltinValues, CompiledUnit, Compiler, GeneratedFile, ResolveOptions, ResolvedUnit, Translator,
};
pub use discover::Unit;
pub use schema_provider::{DocumentSchema, SchemaProvider};
pub use source::{FileSource, MemorySource};
pub use writer::TargetLanguage;
