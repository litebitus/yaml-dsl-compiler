//! Renders an output file in its target language, in that language's one style.

mod hcl;
mod json;
mod yaml;

use crate::diagnostic::Diagnostic;
use crate::output::OutputFile;

/// The longest line a writer writes where the language lets it break the line.
pub const MAX_LINE_WIDTH: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetLanguage {
    Hcl,
    Json,
    Yaml,
}

pub fn write_file(file: &OutputFile) -> Result<String, Diagnostic> {
    let written = match file.language {
        TargetLanguage::Hcl => hcl::write(file),
        TargetLanguage::Json => json::write(file),
        TargetLanguage::Yaml => yaml::write(file),
    };
    written.map_err(|message| {
        Diagnostic::unplaced(format!("writing {}: {message}", file.path.display()))
    })
}
