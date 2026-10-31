//! The library's entry point: a compiler for one DSL over one file source.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;

use crate::definition::DslDefinition;
use crate::diagnostic::Diagnostic;
use crate::discover::{Unit, selects, unit_of, units};
use crate::output::OutputFile;
use crate::resolve::Resolution;
pub use crate::resolve::{BuiltinValues, ResolvedUnit};
use crate::source::FileSource;
use crate::writer::write_file;

/// The caller's code for one DSL: turns a resolved unit into the files to write.
pub trait Translator: Send + Sync {
    fn translate(&self, unit: &ResolvedUnit) -> Result<Vec<OutputFile>, Vec<Diagnostic>>;
}

/// A file a unit compiles to, written in its language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedFile {
    pub path: PathBuf,
    pub contents: String,
}

/// A unit and what it compiled to: its files, or every error it holds.
#[derive(Clone, Debug)]
pub struct CompiledUnit {
    pub unit: Unit,
    pub result: Result<Vec<GeneratedFile>, Vec<Diagnostic>>,
}

pub struct Compiler {
    resolution: Resolution,
    source: Arc<dyn FileSource>,
    builtins: Option<Arc<dyn BuiltinValues>>,
}

impl Compiler {
    /// A compiler for the DSL `definition`, whose config sits in `config_root`.
    pub fn new(
        definition: DslDefinition,
        config_root: impl Into<PathBuf>,
        source: Arc<dyn FileSource>,
    ) -> Result<Self, Diagnostic> {
        Ok(Self {
            resolution: Resolution::new(definition, config_root.into())?,
            source,
            builtins: None,
        })
    }

    pub fn with_builtin_values(mut self, builtins: Arc<dyn BuiltinValues>) -> Self {
        self.builtins = Some(builtins);
        self
    }

    pub fn units(&self) -> Result<Vec<Unit>, Diagnostic> {
        units(
            self.resolution.definition(),
            self.resolution.config_root(),
            self.source.as_ref(),
        )
    }

    /// The unit of one document, found without listing any directory; `None` where
    /// the DSL does not select the document.
    pub fn unit_of(&self, document: &Path) -> Result<Option<Unit>, Diagnostic> {
        unit_of(
            self.resolution.definition(),
            self.resolution.config_root(),
            document,
            self.source.as_ref(),
        )
    }

    /// Whether the DSL's file patterns select the document at `document`.
    pub fn selects(&self, document: &Path) -> Result<bool, Diagnostic> {
        selects(
            self.resolution.definition(),
            self.resolution.config_root(),
            document,
        )
    }

    pub fn resolve(&self, unit: &Unit) -> Result<ResolvedUnit, Vec<Diagnostic>> {
        self.resolution
            .resolve(unit, self.source.as_ref(), self.builtins.as_deref())
    }

    /// Resolves `unit` with `builtins` giving its builtin names their values, for a
    /// caller whose values differ from one resolution of a document to the next.
    pub fn resolve_with_builtins(
        &self,
        unit: &Unit,
        builtins: &dyn BuiltinValues,
    ) -> Result<ResolvedUnit, Vec<Diagnostic>> {
        self.resolution
            .resolve(unit, self.source.as_ref(), Some(builtins))
    }

    /// Compiles `unit`. A fragment compiles to no files.
    pub fn compile(&self, unit: &Unit, translator: &dyn Translator) -> CompiledUnit {
        CompiledUnit {
            unit: unit.clone(),
            result: self.compile_files(unit, translator),
        }
    }

    /// Compiles every unit, in parallel, in the order of their ids.
    pub fn compile_all(
        &self,
        translator: &dyn Translator,
    ) -> Result<Vec<CompiledUnit>, Diagnostic> {
        let units = self.units()?;
        Ok(units
            .par_iter()
            .map(|unit| self.compile(unit, translator))
            .collect())
    }

    fn compile_files(
        &self,
        unit: &Unit,
        translator: &dyn Translator,
    ) -> Result<Vec<GeneratedFile>, Vec<Diagnostic>> {
        let resolved = self.resolve(unit)?;
        if resolved.fragment {
            return Ok(Vec::new());
        }
        let files = translator.translate(&resolved)?;
        let mut generated = Vec::with_capacity(files.len());
        let mut diagnostics = Vec::new();
        for file in &files {
            match write_file(file) {
                Ok(contents) => generated.push(GeneratedFile {
                    path: unit.directory.join(&file.path),
                    contents,
                }),
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        if diagnostics.is_empty() {
            Ok(generated)
        } else {
            Err(diagnostics)
        }
    }
}
