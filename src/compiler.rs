//! The library's entry point: a compiler for one DSL over one file source.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;
use yaml_dsl_spec::Diagnostic;
use yaml_dsl_spec::definition::DslDefinition;

use crate::discover::{Unit, overlays_holding_no_documents_at, selects, unit_of, units};
use crate::output::OutputFile;
use crate::resolve::Resolution;
pub use crate::resolve::{BuiltinValues, ResolveOptions, ResolvedUnit};
use crate::schema_provider::SchemaProvider;
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
    schema_provider: Option<Arc<dyn SchemaProvider>>,
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
            schema_provider: None,
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

    /// The overlays holding no documents whose place is `directory`, each folding to
    /// its common layer alone.
    pub fn overlays_holding_no_documents_at(
        &self,
        directory: &Path,
    ) -> Result<Vec<Unit>, Diagnostic> {
        overlays_holding_no_documents_at(
            self.resolution.definition(),
            self.resolution.config_root(),
            directory,
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
        let options = ResolveOptions {
            builtins: self.builtins.as_deref(),
            schema_provider: self.schema_provider.as_deref(),
        };
        self.resolution
            .resolve(unit, self.source.as_ref(), &options)
    }

    /// Resolves `unit` with `options`, for a caller whose builtin values or schema
    /// provider differ from one resolution of a document to the next. An option the
    /// caller leaves out is the compiler's own.
    pub fn resolve_with(
        &self,
        unit: &Unit,
        options: &ResolveOptions<'_>,
    ) -> Result<ResolvedUnit, Vec<Diagnostic>> {
        let options = ResolveOptions {
            builtins: options.builtins.or(self.builtins.as_deref()),
            schema_provider: options.schema_provider.or(self.schema_provider.as_deref()),
        };
        self.resolution
            .resolve(unit, self.source.as_ref(), &options)
    }

    /// The caller's code that finds each unit's schema.
    pub fn with_schema_provider(mut self, provider: Arc<dyn SchemaProvider>) -> Self {
        self.schema_provider = Some(provider);
        self
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
