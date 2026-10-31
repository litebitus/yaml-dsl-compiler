//! Compiles every DSL of a `yaml-dsl.yml` that states a `code_generation` block.

mod translator;
mod working_tree;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value as JsonValue;
use yaml_dsl_compiler::resolved::ValueKind;
use yaml_dsl_compiler::{BuiltinValues, Compiler, DslDefinition, Unit};

use crate::translator::MockTranslator;
use crate::working_tree::WorkingTree;

const CONFIG_FILE_NAME: &str = "yaml-dsl.yml";
const CODE_GENERATION_KEY: &str = "code_generation";

/// What a DSL compiles to, stated under its block's `code_generation` key.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeGeneration {
    /// The file each unit compiles to, beside the unit's document.
    pub output_file: PathBuf,
    /// Where the components the DSL's entries name are published.
    pub module_source: String,
}

/// The overlay folder a unit sits under is the environment it deploys to.
struct OverlayEnvironment;

impl BuiltinValues for OverlayEnvironment {
    fn value(&self, unit: &Unit, _scope: &str, name: &str) -> Option<ValueKind> {
        (name == "env")
            .then(|| unit.overlay.clone().map(ValueKind::String))
            .flatten()
    }
}

fn main() -> ExitCode {
    let config_root = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    match run(&config_root) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Compiles every unit and writes the files of those without errors; whether
/// every unit compiled.
fn run(config_root: &Path) -> Result<bool, String> {
    let config_root = config_root
        .canonicalize()
        .map_err(|error| format!("{}: {error}", config_root.display()))?;
    let config_path = config_root.join(CONFIG_FILE_NAME);
    let text = std::fs::read_to_string(&config_path)
        .map_err(|error| format!("{}: {error}", config_path.display()))?;
    let file: Arc<str> = Arc::from(config_path.to_string_lossy().as_ref());
    let config = yaml_dsl_compiler::yaml_reader::parse(&file, &text, &[])
        .map_err(|error| error.to_string())?
        .to_json();
    let blocks = config
        .get("dsls")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();
    let source = Arc::new(WorkingTree);
    let mut all_compiled = true;
    for block in blocks {
        let Some(code_generation) = block.get(CODE_GENERATION_KEY) else {
            continue;
        };
        let code_generation: CodeGeneration = serde_json::from_value(code_generation.clone())
            .map_err(|error| format!("{CODE_GENERATION_KEY}: {error}"))?;
        let definition = DslDefinition::from_json(&block)?;
        let compiler = Compiler::new(definition, config_root.clone(), source.clone())
            .map_err(|error| error.to_string())?
            .with_builtin_values(Arc::new(OverlayEnvironment));
        let translator = MockTranslator::new(code_generation, config_root.clone());
        for compiled in compiler
            .compile_all(&translator)
            .map_err(|error| error.to_string())?
        {
            match compiled.result {
                Ok(files) => {
                    for generated in files {
                        write_if_changed(&generated.path, &generated.contents)?;
                    }
                }
                Err(diagnostics) => {
                    all_compiled = false;
                    for diagnostic in diagnostics {
                        eprintln!("{diagnostic}");
                    }
                }
            }
        }
    }
    Ok(all_compiled)
}

fn write_if_changed(path: &Path, contents: &str) -> Result<(), String> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == contents) {
        return Ok(());
    }
    std::fs::write(path, contents).map_err(|error| format!("{}: {error}", path.display()))
}
