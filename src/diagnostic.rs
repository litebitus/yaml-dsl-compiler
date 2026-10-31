use std::fmt;

use crate::position::Position;

/// A problem the compiler found, at the value it is about where there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub position: Option<Position>,
    pub message: String,
}

impl Diagnostic {
    pub fn at(position: &Position, message: impl Into<String>) -> Self {
        Self {
            position: Some(position.clone()),
            message: message.into(),
        }
    }

    pub fn unplaced(message: impl Into<String>) -> Self {
        Self {
            position: None,
            message: message.into(),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.position {
            Some(position) => write!(formatter, "{position}: {}", self.message),
            None => write!(formatter, "{}", self.message),
        }
    }
}

impl std::error::Error for Diagnostic {}
