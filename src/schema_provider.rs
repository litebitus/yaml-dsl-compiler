//! The caller's code that finds a unit's schema: the compiler looks for none and
//! reads none itself.

use yaml_dsl_spec::Diagnostic;

use crate::discover::Unit;

/// A document's schema, as its caller found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentSchema {
    /// The schema as reports name it, such as its file's name or its URL.
    pub name: String,
    pub text: String,
    /// Where in the schema the document's root is, for a fragment, which is never
    /// compiled alone; `None` for a full document.
    pub fragment: Option<String>,
    /// What finding the schema warns of, such as a credential its server refused.
    pub warnings: Vec<String>,
}

/// The caller's code that finds a unit's schema.
pub trait SchemaProvider: Send + Sync {
    fn schema_of(&self, unit: &Unit, document_text: &str) -> Result<DocumentSchema, Diagnostic>;
}
