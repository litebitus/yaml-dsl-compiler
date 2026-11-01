use std::fmt;

use crate::position::Position;

/// A problem the compiler found, at the value it is about where there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub position: Option<Position>,
    pub message: String,
    pub notes: Option<Box<DiagnosticNotes>>,
}

/// What a report writes beside a problem.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiagnosticNotes {
    /// What is wrong with the text at the position.
    pub label: Option<String>,
    /// The schema rule the document breaks: its keyword and where the schema holds it.
    pub rule: Option<String>,
    /// How the problem may be fixed, such as the allowed name closest to a misspelt one.
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn at(position: &Position, message: impl Into<String>) -> Self {
        Self {
            position: Some(position.clone()),
            ..Self::unplaced(message)
        }
    }

    pub fn unplaced(message: impl Into<String>) -> Self {
        Self {
            position: None,
            message: message.into(),
            notes: None,
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.notes_mut().label = Some(label.into());
        self
    }

    pub fn with_rule(mut self, rule: impl Into<String>) -> Self {
        self.notes_mut().rule = Some(rule.into());
        self
    }

    pub fn with_help(mut self, help: Option<String>) -> Self {
        if help.is_some() {
            self.notes_mut().help = help;
        }
        self
    }

    fn notes_mut(&mut self) -> &mut DiagnosticNotes {
        self.notes.get_or_insert_with(Box::default)
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
