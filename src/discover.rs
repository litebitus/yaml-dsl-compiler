//! Finds a DSL's documents and groups each overlay with its common layer.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use yaml_dsl_spec::Diagnostic;
use yaml_dsl_spec::definition::{CommonLayerDiscovery, DslDefinition, Layers};

use crate::source::FileSource;

const CONFIG_ROOT: &str = "{config_root}/";
const CONFIG_FILE_NAME: &str = "yaml-dsl.yml";

/// What compiles as one: a document, or an overlay with the common layer it takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    /// The unit's document, relative to the config root: the overlay, or the lone file.
    pub id: String,
    /// The unit's document.
    pub file: PathBuf,
    /// The directory holding the unit's document; output paths are relative to it.
    pub directory: PathBuf,
    /// The overlay directory the document sits under, for an overlay.
    pub overlay: Option<String>,
    /// The common layer merged under the document.
    pub common_layer: Option<PathBuf>,
    /// An overlay holding no documents: its directory holds the overlay's place but
    /// no overlay file, so `file` is where the file would be, never read, and the
    /// fold is the common layer alone.
    pub holds_no_documents: bool,
}

pub fn units(
    definition: &DslDefinition,
    config_root: &Path,
    source: &dyn FileSource,
) -> Result<Vec<Unit>, Diagnostic> {
    let includes = glob_set(&definition.file_includes)?;
    let excludes = glob_set(&definition.file_excludes)?;
    let listed = source.list_files(config_root).map_err(|error| {
        Diagnostic::unplaced(format!("listing {}: {error}", config_root.display()))
    })?;
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut directories: BTreeSet<String> = BTreeSet::new();
    for path in listed {
        let Ok(relative) = path.strip_prefix(config_root) else {
            continue;
        };
        let relative = slash_path(relative);
        let mut parent = relative.as_str();
        while let Some((directory, _)) = parent.rsplit_once('/') {
            directories.insert(directory.to_owned());
            parent = directory;
        }
        if relative.rsplit('/').next() == Some(CONFIG_FILE_NAME) {
            continue;
        }
        if includes.is_match(&relative) && !excludes.is_match(&relative) {
            files.insert(relative);
        }
    }
    let Some(layers) = &definition.layers else {
        return Ok(files
            .iter()
            .map(|file| lone_unit(config_root, file, None))
            .collect());
    };
    if layers.common_layer_discovery == CommonLayerDiscovery::OverlayIncludes {
        return Err(Diagnostic::unplaced(format!(
            "DSL `{}`: common_layer_discovery `overlay_includes` is not supported yet",
            definition.id
        )));
    }
    layered_units(
        definition,
        layers,
        config_root,
        source,
        &files,
        &directories,
    )
}

fn layered_units(
    definition: &DslDefinition,
    layers: &Layers,
    config_root: &Path,
    source: &dyn FileSource,
    files: &BTreeSet<String>,
    directories: &BTreeSet<String>,
) -> Result<Vec<Unit>, Diagnostic> {
    let mut units = Vec::new();
    let mut taken_common_layers = BTreeSet::new();
    for file in files {
        let Some(unit) = overlay_unit(layers, config_root, source, file) else {
            continue;
        };
        if let Some(common_layer) = &unit.common_layer
            && let Ok(relative) = common_layer.strip_prefix(config_root)
        {
            taken_common_layers.insert(slash_path(relative));
        }
        units.push(unit);
    }
    let mut overlays_holding_no_documents: Vec<Unit> = Vec::new();
    for unit in units.iter().filter(|unit| unit.common_layer.is_some()) {
        for place in sibling_places(layers, &unit.id) {
            let holds_no_documents = !files.contains(&place.file)
                && directories.contains(&place.directory)
                && selected_relative(definition, &place.file)?
                && overlays_holding_no_documents
                    .iter()
                    .all(|listed_unit| listed_unit.id != place.file);
            if holds_no_documents {
                overlays_holding_no_documents.push(unit_holding_no_documents(
                    config_root,
                    &place,
                    unit,
                ));
            }
        }
    }
    units.extend(overlays_holding_no_documents);
    for file in files {
        if !under_overlay(layers, file) && !taken_common_layers.contains(file) {
            units.push(lone_unit(config_root, file, None));
        }
    }
    units.sort_by(|first, second| first.id.cmp(&second.id));
    Ok(units)
}

/// Where an overlay's file would be under another overlay directory of its stack.
struct OverlayPlace {
    overlay: String,
    directory: String,
    file: String,
}

/// The directories at the same path as `directory` below each other overlay
/// directory of its stack, each with its overlay directory's name.
fn sibling_directories(layers: &Layers, directory: &str) -> Vec<(String, String)> {
    let components: Vec<&str> = directory.split('/').collect();
    let Some(overlay_index) = overlay_index(layers, &components) else {
        return Vec::new();
    };
    let common_directory = components[..overlay_index].join("/");
    let below_overlay = components[overlay_index + 1..].join("/");
    layers
        .overlay_folders
        .iter()
        .filter(|folder| folder.as_str() != components[overlay_index])
        .map(|folder| {
            let overlay_directory = join_relative(&common_directory, folder);
            let sibling = if below_overlay.is_empty() {
                overlay_directory
            } else {
                join_relative(&overlay_directory, &below_overlay)
            };
            (folder.clone(), sibling)
        })
        .collect()
}

/// The places of the overlay `file` under each other overlay directory: the same
/// path below the overlay directory, beside the same common layer.
fn sibling_places(layers: &Layers, file: &str) -> Vec<OverlayPlace> {
    let Some((directory, name)) = file.rsplit_once('/') else {
        return Vec::new();
    };
    sibling_directories(layers, directory)
        .into_iter()
        .map(|(overlay, directory)| OverlayPlace {
            file: join_relative(&directory, name),
            overlay,
            directory,
        })
        .collect()
}

/// An overlay holding no documents at `place`, taking the common layer `sibling` takes.
fn unit_holding_no_documents(config_root: &Path, place: &OverlayPlace, sibling: &Unit) -> Unit {
    let mut unit = lone_unit(config_root, &place.file, Some(place.overlay.clone()));
    unit.common_layer = sibling.common_layer.clone();
    unit.holds_no_documents = true;
    unit
}

/// The overlays holding no documents whose place is `directory`: one for each overlay
/// file of the same stack under another overlay directory, at the same path, where
/// `directory` holds no file of that name. Only those sibling directories are listed.
pub fn overlays_holding_no_documents_at(
    definition: &DslDefinition,
    config_root: &Path,
    directory: &Path,
    source: &dyn FileSource,
) -> Result<Vec<Unit>, Diagnostic> {
    let Some(layers) = &definition.layers else {
        return Ok(Vec::new());
    };
    if layers.common_layer_discovery == CommonLayerDiscovery::OverlayIncludes {
        return Ok(Vec::new());
    }
    let Ok(relative) = directory.strip_prefix(config_root) else {
        return Ok(Vec::new());
    };
    let relative = slash_path(relative);
    let mut overlays: Vec<Unit> = Vec::new();
    for (_, sibling_directory) in sibling_directories(layers, &relative) {
        let sibling_directory = config_root.join(sibling_directory);
        let listed = source.list_files(&sibling_directory).map_err(|error| {
            Diagnostic::unplaced(format!("listing {}: {error}", sibling_directory.display()))
        })?;
        for path in listed {
            if path.parent() != Some(sibling_directory.as_path()) {
                continue;
            }
            let Some(sibling_file) = selected_file(definition, config_root, &path)? else {
                continue;
            };
            let Some(sibling) = overlay_unit(layers, config_root, source, &sibling_file) else {
                continue;
            };
            let Some(own_place) = sibling_places(layers, &sibling_file)
                .into_iter()
                .find(|place| place.directory == relative)
            else {
                continue;
            };
            let holds_no_documents = sibling.common_layer.is_some()
                && !source.exists(&config_root.join(&own_place.file))
                && selected_relative(definition, &own_place.file)?
                && overlays
                    .iter()
                    .all(|listed_unit| listed_unit.id != own_place.file);
            if holds_no_documents {
                overlays.push(unit_holding_no_documents(config_root, &own_place, &sibling));
            }
        }
    }
    overlays.sort_by(|first, second| first.id.cmp(&second.id));
    Ok(overlays)
}

/// The unit of the document at `document`, read without listing any directory:
/// an overlay with the common layer it takes, or a lone document. `None` where the
/// DSL's file patterns do not select the document.
pub fn unit_of(
    definition: &DslDefinition,
    config_root: &Path,
    document: &Path,
    source: &dyn FileSource,
) -> Result<Option<Unit>, Diagnostic> {
    let Some(file) = selected_file(definition, config_root, document)? else {
        return Ok(None);
    };
    let Some(layers) = &definition.layers else {
        return Ok(Some(lone_unit(config_root, &file, None)));
    };
    if layers.common_layer_discovery == CommonLayerDiscovery::OverlayIncludes {
        return Err(Diagnostic::unplaced(format!(
            "DSL `{}`: common_layer_discovery `overlay_includes` is not supported yet",
            definition.id
        )));
    }
    Ok(Some(
        overlay_unit(layers, config_root, source, &file)
            .unwrap_or_else(|| lone_unit(config_root, &file, None)),
    ))
}

/// Whether the DSL's file patterns select the document at `document`.
pub fn selects(
    definition: &DslDefinition,
    config_root: &Path,
    document: &Path,
) -> Result<bool, Diagnostic> {
    Ok(selected_file(definition, config_root, document)?.is_some())
}

fn selected_file(
    definition: &DslDefinition,
    config_root: &Path,
    document: &Path,
) -> Result<Option<String>, Diagnostic> {
    let Ok(relative) = document.strip_prefix(config_root) else {
        return Ok(None);
    };
    let relative = slash_path(relative);
    Ok(selected_relative(definition, &relative)?.then_some(relative))
}

/// Whether the DSL's file patterns select `relative`, a path from the config root.
fn selected_relative(definition: &DslDefinition, relative: &str) -> Result<bool, Diagnostic> {
    let includes = glob_set(&definition.file_includes)?;
    let excludes = glob_set(&definition.file_excludes)?;
    Ok(relative.rsplit('/').next() != Some(CONFIG_FILE_NAME)
        && includes.is_match(relative)
        && !excludes.is_match(relative))
}

fn under_overlay(layers: &Layers, file: &str) -> bool {
    file.split('/').rev().skip(1).any(|directory| {
        layers
            .overlay_folders
            .iter()
            .any(|folder| folder == directory)
    })
}

/// The overlay unit of `file`, with the common layer it takes, where `file` sits
/// under one of the layers' overlay folders.
fn overlay_unit(
    layers: &Layers,
    config_root: &Path,
    source: &dyn FileSource,
    file: &str,
) -> Option<Unit> {
    let components: Vec<&str> = file.split('/').collect();
    let directories = &components[..components.len() - 1];
    let overlay_index = overlay_index(layers, directories)?;
    let file_name = components[components.len() - 1];
    let concern_below = overlay_index + 1 < directories.len();
    let concern = directories[directories.len() - 1];
    let common_directory = directories[..overlay_index].join("/");
    let candidates = layers
        .common_layer_files
        .get(file_name)
        .cloned()
        .unwrap_or_else(|| vec![file_name.to_owned()]);
    let common_layer = candidates
        .iter()
        .filter(|candidate| concern_below || !candidate.contains("{concern}"))
        .map(|candidate| join_relative(&common_directory, &candidate.replace("{concern}", concern)))
        .find(|candidate| source.exists(&config_root.join(candidate)));
    let mut unit = lone_unit(
        config_root,
        file,
        Some(directories[overlay_index].to_owned()),
    );
    unit.common_layer = common_layer.map(|common_layer| config_root.join(common_layer));
    Some(unit)
}

/// Where among `directories` the overlay directory is: the last, for `parent`; the
/// nearest to the file, for `ancestor`.
fn overlay_index(layers: &Layers, directories: &[&str]) -> Option<usize> {
    let is_overlay_folder = |directory: &&str| {
        layers
            .overlay_folders
            .iter()
            .any(|folder| folder == directory)
    };
    match layers.common_layer_discovery {
        CommonLayerDiscovery::Parent => directories
            .len()
            .checked_sub(1)
            .filter(|&index| is_overlay_folder(&directories[index])),
        _ => directories.iter().rposition(is_overlay_folder),
    }
}

fn lone_unit(config_root: &Path, file: &str, overlay: Option<String>) -> Unit {
    let path = config_root.join(file);
    let directory = path
        .parent()
        .map_or_else(|| config_root.to_path_buf(), Path::to_path_buf);
    Unit {
        id: file.to_owned(),
        file: path,
        directory,
        overlay,
        common_layer: None,
        holds_no_documents: false,
    }
}

fn join_relative(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn glob_set(patterns: &[String]) -> Result<GlobSet, Diagnostic> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let anchored = pattern.strip_prefix(CONFIG_ROOT);
        let text = anchored.unwrap_or(pattern);
        let text = if anchored.is_none() && !text.starts_with("**/") {
            format!("**/{text}")
        } else {
            text.to_owned()
        };
        builder.add(build_glob(&text)?);
    }
    builder
        .build()
        .map_err(|error| Diagnostic::unplaced(error.to_string()))
}

fn build_glob(text: &str) -> Result<Glob, Diagnostic> {
    GlobBuilder::new(text)
        .literal_separator(true)
        .build()
        .map_err(|error| Diagnostic::unplaced(format!("glob `{text}`: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemorySource;

    fn definition(layers: &str) -> DslDefinition {
        let block: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"id": "mock", "file_includes": ["**/mock-dsl.yml"], "layers": {layers}}}"#
        ))
        .unwrap();
        DslDefinition::from_json(&block).unwrap()
    }

    fn source(files: &[&str]) -> MemorySource {
        files.iter().fold(MemorySource::new(), |source, file| {
            source.with_file(Path::new("/root").join(file), "")
        })
    }

    #[test]
    fn a_concern_above_its_overlays_pairs_each_overlay_with_its_parent() {
        let definition = definition(
            r#"{"overlay_folders": ["overlay-1", "overlay-2"],
                "common_layer_discovery": "parent"}"#,
        );
        let source = source(&[
            "concern-1/mock-dsl.yml",
            "concern-1/overlay-1/mock-dsl.yml",
            "concern-1/overlay-2/mock-dsl.yml",
            "lone/mock-dsl.yml",
        ]);
        let units = units(&definition, Path::new("/root"), &source).unwrap();
        let ids: Vec<&str> = units.iter().map(|unit| unit.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "concern-1/overlay-1/mock-dsl.yml",
                "concern-1/overlay-2/mock-dsl.yml",
                "lone/mock-dsl.yml"
            ]
        );
        assert_eq!(
            units[0].common_layer.as_deref(),
            Some(Path::new("/root/concern-1/mock-dsl.yml"))
        );
        assert_eq!(units[0].overlay.as_deref(), Some("overlay-1"));
        assert_eq!(units[2].common_layer, None);
    }

    #[test]
    fn a_concern_below_its_overlays_takes_its_own_common_layer_first() {
        let definition = definition(
            r#"{"overlay_folders": ["overlay-1"], "common_layer_discovery": "ancestor",
                "common_layer_files": {
                    "mock-dsl.yml": ["{concern}.mock-dsl.yml", "mock-dsl.yml"]
                }}"#,
        );
        let source = source(&[
            "group/mock-dsl.yml",
            "group/concern-1.mock-dsl.yml",
            "group/overlay-1/concern-1/mock-dsl.yml",
            "group/overlay-1/concern-2/mock-dsl.yml",
        ]);
        let units = units(&definition, Path::new("/root"), &source).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(
            units[0].common_layer.as_deref(),
            Some(Path::new("/root/group/concern-1.mock-dsl.yml"))
        );
        assert_eq!(
            units[1].common_layer.as_deref(),
            Some(Path::new("/root/group/mock-dsl.yml"))
        );
    }

    #[test]
    fn an_overlay_directory_holding_no_documents_is_an_overlay_of_its_stack() {
        let definition = definition(
            r#"{"overlay_folders": ["overlay-1", "overlay-2", "overlay-3"],
                "common_layer_discovery": "parent"}"#,
        );
        let source = source(&[
            "concern-1/mock-dsl.yml",
            "concern-1/overlay-1/mock-dsl.yml",
            "concern-1/overlay-2/mock-other.txt",
            "concern-2/mock-dsl.yml",
            "concern-2/overlay-1/mock-other.txt",
        ]);
        let units = units(&definition, Path::new("/root"), &source).unwrap();
        let listed: Vec<(&str, bool)> = units
            .iter()
            .map(|unit| (unit.id.as_str(), unit.holds_no_documents))
            .collect();
        assert_eq!(
            listed,
            [
                ("concern-1/overlay-1/mock-dsl.yml", false),
                ("concern-1/overlay-2/mock-dsl.yml", true),
                ("concern-2/mock-dsl.yml", false),
            ]
        );
        assert_eq!(units[1].overlay.as_deref(), Some("overlay-2"));
        assert_eq!(
            units[1].common_layer.as_deref(),
            Some(Path::new("/root/concern-1/mock-dsl.yml"))
        );
        assert_eq!(units[2].common_layer, None);
    }

    #[test]
    fn an_overlay_below_its_overlay_directory_takes_the_path_of_the_stack_s_others() {
        let definition = definition(
            r#"{"overlay_folders": ["overlay-1", "overlay-2"], "common_layer_discovery": "ancestor"}"#,
        );
        let source = source(&[
            "group/mock-dsl.yml",
            "group/overlay-1/concern-1/mock-dsl.yml",
            "group/overlay-2/concern-1/mock-other.txt",
            "group/overlay-2/mock-other.txt",
        ]);
        let root = Path::new("/root");
        let units = units(&definition, root, &source).unwrap();
        let listed: Vec<(&str, bool)> = units
            .iter()
            .map(|unit| (unit.id.as_str(), unit.holds_no_documents))
            .collect();
        assert_eq!(
            listed,
            [
                ("group/overlay-1/concern-1/mock-dsl.yml", false),
                ("group/overlay-2/concern-1/mock-dsl.yml", true),
            ]
        );
        let found = overlays_holding_no_documents_at(
            &definition,
            root,
            Path::new("/root/group/overlay-2/concern-1"),
            &source,
        )
        .unwrap();
        assert_eq!(found, [units[1].clone()]);
        let at_overlay_directory = overlays_holding_no_documents_at(
            &definition,
            root,
            Path::new("/root/group/overlay-2"),
            &source,
        );
        assert_eq!(at_overlay_directory.unwrap(), []);
        let with_its_document = overlays_holding_no_documents_at(
            &definition,
            root,
            Path::new("/root/group/overlay-1/concern-1"),
            &source,
        );
        assert_eq!(with_its_document.unwrap(), []);
    }

    struct UnlistableSource(MemorySource);

    impl FileSource for UnlistableSource {
        fn list_files(&self, _directory: &Path) -> std::io::Result<Vec<PathBuf>> {
            Err(std::io::Error::other("mock listing refused"))
        }

        fn read(&self, path: &Path) -> std::io::Result<String> {
            self.0.read(path)
        }

        fn exists(&self, path: &Path) -> bool {
            self.0.exists(path)
        }
    }

    #[test]
    fn a_sibling_overlay_directory_that_cannot_be_listed_is_an_error() {
        let definition = definition(
            r#"{"overlay_folders": ["overlay-1", "overlay-2"], "common_layer_discovery": "parent"}"#,
        );
        let source = UnlistableSource(source(&["concern-1/mock-dsl.yml"]));
        let listed = overlays_holding_no_documents_at(
            &definition,
            Path::new("/root"),
            Path::new("/root/concern-1/overlay-2"),
            &source,
        );
        assert!(listed.unwrap_err().message.contains("mock listing refused"));
    }

    #[test]
    fn one_document_finds_its_unit_without_a_listing() {
        let definition = definition(
            r#"{"overlay_folders": ["overlay-1"], "common_layer_discovery": "ancestor"}"#,
        );
        let source = source(&["group/mock-dsl.yml", "lone/mock-dsl.yml"]);
        let root = Path::new("/root");
        let overlay = unit_of(
            &definition,
            root,
            Path::new("/root/group/overlay-1/concern-1/mock-dsl.yml"),
            &source,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            overlay.common_layer.as_deref(),
            Some(Path::new("/root/group/mock-dsl.yml"))
        );
        let lone = unit_of(
            &definition,
            root,
            Path::new("/root/lone/mock-dsl.yml"),
            &source,
        )
        .unwrap()
        .unwrap();
        assert_eq!((lone.overlay, lone.common_layer), (None, None));
        let other = unit_of(
            &definition,
            root,
            Path::new("/root/lone/other.yml"),
            &source,
        );
        assert_eq!(other.unwrap(), None);
    }
}
