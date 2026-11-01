//! A unit's schema, found as the editor finds a file's: its modeline's path, else
//! the first of the DSL's search paths on disk.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use yaml_dsl_compiler::{DocumentSchema, SchemaProvider, Unit};
use yaml_dsl_spec::{Diagnostic, Position};

const MODELINE_MARKER: &str = "yaml-language-server: $schema=";
const CONFIG_ROOT: &str = "{config_root}/";

pub struct SchemaFiles {
    pub config_root: PathBuf,
    pub search_paths: Vec<String>,
}

impl SchemaProvider for SchemaFiles {
    fn schema_of(&self, unit: &Unit, document_text: &str) -> Result<DocumentSchema, Diagnostic> {
        let modeline = document_text.lines().find_map(|line| {
            let comment = line.trim_start().strip_prefix('#')?;
            Some(
                comment
                    .trim_start()
                    .strip_prefix(MODELINE_MARKER)?
                    .trim()
                    .to_owned(),
            )
        });
        let (modeline_path, fragment) = match modeline.as_deref().map(|uri| uri.split_once('#')) {
            Some(Some((path, fragment))) => (Some(path.to_owned()), Some(fragment.to_owned())),
            Some(None) => (modeline.clone(), None),
            None => (None, None),
        };
        let candidates =
            modeline_path
                .map(|path| unit.directory.join(path))
                .into_iter()
                .chain(self.search_paths.iter().map(
                    |entry| match entry.strip_prefix(CONFIG_ROOT) {
                        Some(rest) => self.config_root.join(rest),
                        None => unit.directory.join(entry),
                    },
                ));
        for candidate in candidates {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                return Ok(DocumentSchema {
                    name: file_name(&candidate),
                    text,
                    fragment,
                    warnings: Vec::new(),
                });
            }
        }
        let file: Arc<str> = Arc::from(unit.file.to_string_lossy().as_ref());
        Err(Diagnostic::at(
            &Position::file_start(&file),
            "no schema found: neither the modeline nor schema_search_paths names a file on disk",
        ))
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}
