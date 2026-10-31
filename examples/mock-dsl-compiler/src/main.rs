//! Compiles every DSL of a `yaml-dsl.yml` that the tool's own config gives a
//! translator.

mod translator;
mod working_tree;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value as JsonValue;
use yaml_dsl_compiler::resolved::ValueKind;
use yaml_dsl_compiler::{BuiltinValues, Compiler, DslDefinition, Unit};

use crate::translator::MockTranslator;
use crate::working_tree::WorkingTree;

const DSL_CONFIG_FILE_NAME: &str = "yaml-dsl.yml";
const TOOL_CONFIG_FILE_NAME: &str = "mock-dsl-compiler.yml";

/// The tool's own config: each DSL it compiles, by the DSL's id in `yaml-dsl.yml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolConfig {
    translators: BTreeMap<String, TranslatorSettings>,
}

/// What a DSL compiles to.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranslatorSettings {
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
    let tool_config: ToolConfig =
        serde_json::from_value(read_yaml(&config_root.join(TOOL_CONFIG_FILE_NAME))?)
            .map_err(|error| format!("{TOOL_CONFIG_FILE_NAME}: {error}"))?;
    let dsl_config = read_yaml(&config_root.join(DSL_CONFIG_FILE_NAME))?;
    let blocks = dsl_config
        .get("dsls")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();
    let mut definitions = BTreeMap::new();
    for block in &blocks {
        let definition = DslDefinition::from_json(block)?;
        definitions.insert(definition.id.clone(), definition);
    }
    let source = Arc::new(WorkingTree);
    let mut all_compiled = true;
    for (id, settings) in tool_config.translators {
        let definition = definitions.remove(&id).ok_or_else(|| {
            format!("{TOOL_CONFIG_FILE_NAME}: `{id}` is no DSL of {DSL_CONFIG_FILE_NAME}")
        })?;
        let compiler = Compiler::new(definition, config_root.clone(), source.clone())
            .map_err(|error| error.to_string())?
            .with_builtin_values(Arc::new(OverlayEnvironment));
        let translator = MockTranslator::new(settings, config_root.clone());
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

fn read_yaml(path: &Path) -> Result<JsonValue, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let file: Arc<str> = Arc::from(path.to_string_lossy().as_ref());
    yaml_dsl_compiler::yaml_reader::parse(&file, &text, &[])
        .map(|node| node.to_json())
        .map_err(|error| error.to_string())
}

fn write_if_changed(path: &Path, contents: &str) -> Result<(), String> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == contents) {
        return Ok(());
    }
    std::fs::write(path, contents).map_err(|error| format!("{}: {error}", path.display()))
}
