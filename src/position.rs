use std::fmt;
use std::sync::Arc;

/// Where a value was written: a file, a 1-based line and a 1-based column.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Position {
    pub file: Arc<str>,
    pub line: usize,
    pub column: usize,
}

impl Position {
    pub fn new(file: &Arc<str>, line: usize, column: usize) -> Self {
        Self {
            file: Arc::clone(file),
            line,
            column,
        }
    }

    /// The start of a file, for a value that has no narrower place.
    pub fn file_start(file: &Arc<str>) -> Self {
        Self::new(file, 1, 1)
    }
}

impl fmt::Display for Position {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}:{}", self.file, self.line, self.column)
    }
}
