use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// How the compiler lists and reads files. The compiler reads nothing else from
/// the disk, so a caller decides what it sees: a working tree, an editor's open
/// buffers, or an in-memory fixture.
pub trait FileSource: Send + Sync {
    /// Every file under `directory`, at any depth; none where `directory` does not exist.
    fn list_files(&self, directory: &Path) -> io::Result<Vec<PathBuf>>;

    fn read(&self, path: &Path) -> io::Result<String>;

    fn exists(&self, path: &Path) -> bool;
}

/// Files held in memory, keyed by path.
#[derive(Clone, Debug, Default)]
pub struct MemorySource {
    files: BTreeMap<PathBuf, String>,
}

impl MemorySource {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_file(mut self, path: impl Into<PathBuf>, text: impl Into<String>) -> Self {
        self.files.insert(path.into(), text.into());
        self
    }
}

impl FileSource for MemorySource {
    fn list_files(&self, directory: &Path) -> io::Result<Vec<PathBuf>> {
        Ok(self
            .files
            .keys()
            .filter(|path| path.starts_with(directory))
            .cloned()
            .collect())
    }

    fn read(&self, path: &Path) -> io::Result<String> {
        self.files.get(path).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} not found", path.display()),
            )
        })
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }
}
