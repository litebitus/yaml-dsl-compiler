//! The files of a working tree, without hidden directories or gitignored paths.

use std::io;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use yaml_dsl_compiler::FileSource;

pub struct WorkingTree;

impl FileSource for WorkingTree {
    fn list_files(&self, directory: &Path) -> io::Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        if !directory.is_dir() {
            return Ok(files);
        }
        for entry in WalkBuilder::new(directory)
            .hidden(true)
            .git_ignore(true)
            .require_git(false)
            .build()
        {
            let entry = entry.map_err(io::Error::other)?;
            if entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                files.push(entry.into_path());
            }
        }
        Ok(files)
    }

    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.is_file()
    }
}
