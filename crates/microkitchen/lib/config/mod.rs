//! `[_.microkitchen]` configuration: discovery, parsing, validation and edits.

pub mod discover;
pub mod edit;
pub mod hostpat;
pub mod schema;
pub mod size;
pub mod validate;

use std::fmt;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::mise::Mise;
use crate::mise::declarations::EnvDeclarations;
use crate::mise::env::{self, ResolvedEnv};

use self::discover::Discovery;
use self::schema::ParsedKitchen;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Where the microkitchen table lives in the kitchen file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionLocation {
    /// `[_.microkitchen]`: mise documents top-level `_` as never parsed.
    Underscore,

    /// `[microkitchen]`: works, but mise warns about an unknown field.
    Legacy,
}

/// Severity of a [`Diagnostic`], ordered from least to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Notice,
    Warning,
    Error,
}

/// One problem found while loading a project, located in its file if possible.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub message: String,
}

/// All diagnostics of one load, reported together.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(transparent)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

/// A config file's text, kept to map byte spans to lines and columns.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    pub text: String,
}

/// Everything known about the project in the current directory.
#[derive(Debug)]
pub struct Project {
    pub discovery: Discovery,

    /// `None` when the kitchen file is not valid TOML.
    pub kitchen: Option<ParsedKitchen>,

    pub declarations: EnvDeclarations,

    /// `None` when mise could not resolve the environment.
    pub env: Option<ResolvedEnv>,

    pub diagnostics: Diagnostics,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl SectionLocation {
    /// The TOML table path, as written in a header.
    pub fn table_path(self) -> &'static str {
        match self {
            Self::Underscore => "_.microkitchen",
            Self::Legacy => "microkitchen",
        }
    }
}

impl Diagnostic {
    /// A diagnostic that belongs to no particular file.
    pub fn general(severity: Severity, message: impl Into<String>) -> Self {
        Self {
            severity,
            file: None,
            line: None,
            column: None,
            key: None,
            message: message.into(),
        }
    }

    /// `file:line:column: key: message`, leaving out the parts that are unknown.
    pub fn located_message(&self) -> String {
        let mut out = String::new();
        if let Some(file) = &self.file {
            out.push_str(&file.display().to_string());
            if let (Some(line), Some(column)) = (self.line, self.column) {
                out.push_str(&format!(":{line}:{column}"));
            }
            out.push_str(": ");
        }
        if let Some(key) = &self.key {
            out.push_str(key);
            out.push_str(": ");
        }
        out.push_str(&self.message);
        out
    }
}

impl Diagnostics {
    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.items.push(diagnostic);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn error_count(&self) -> usize {
        self.items
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    pub fn has_errors(&self) -> bool {
        self.error_count() > 0
    }
}

impl Source {
    pub fn new(path: impl Into<PathBuf>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            text: text.into(),
        }
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Self::new(path, text))
    }

    /// 1-based line and column (in characters) of a byte offset.
    pub fn position(&self, offset: usize) -> (usize, usize) {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        let before = &self.text[..offset];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        (line, column)
    }

    /// A diagnostic located at `span` in this file.
    pub fn diagnostic(
        &self,
        severity: Severity,
        span: Option<Range<usize>>,
        key: Option<&str>,
        message: impl Into<String>,
    ) -> Diagnostic {
        let position = span.map(|s| self.position(s.start));
        Diagnostic {
            severity,
            file: Some(self.path.clone()),
            line: position.map(|p| p.0),
            column: position.map(|p| p.1),
            key: key.map(str::to_owned),
            message: message.into(),
        }
    }
}

impl Project {
    /// Discover, parse and validate the project containing `cwd`, and resolve
    /// its environment on the host.
    ///
    /// Problems with the configuration end up in [`Project::diagnostics`];
    /// only failures to run mise or read files are returned as errors.
    pub fn load(mise: &Mise, cwd: &Path) -> Result<Self> {
        let discovery = discover::discover(mise, cwd)?;
        let source = Source::read(&discovery.kitchen_file)?;
        let mut diagnostics = Diagnostics::default();

        let kitchen = schema::parse(&source, &discovery.kitchen_dir, &mut diagnostics);
        let declarations = EnvDeclarations::collect(&discovery.loaded_files)?;

        // Resolve where discovery ran: the loaded files, and so the
        // declarations, depend on the directory mise starts in.
        let env = match mise.env(cwd)? {
            Ok(output) => Some(env::merge(output, &declarations, |name| {
                std::env::var(name).ok()
            })),
            Err(message) => {
                diagnostics.push(Diagnostic::general(
                    Severity::Error,
                    format!("mise could not resolve the environment: {message}"),
                ));
                None
            }
        };

        if let Some(kitchen) = &kitchen {
            validate::check(
                kitchen,
                &source,
                &declarations,
                env.as_ref(),
                &mut diagnostics,
            );
        }

        Ok(Self {
            discovery,
            kitchen,
            declarations,
            env,
            diagnostics,
        })
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Notice => "note",
            Self::Warning => "warning",
            Self::Error => "error",
        })
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.severity, self.located_message())
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_are_one_based() {
        let source = Source::new("m.toml", "a = 1\nbé = 2\n");
        assert_eq!(source.position(0), (1, 1));
        assert_eq!(source.position(6), (2, 1));
        assert_eq!(source.position(10), (2, 4));
        assert_eq!(source.position(999), (3, 1));
    }

    #[test]
    fn located_message_omits_unknown_parts() {
        let source = Source::new("/p/mise.toml", "x\ncpus = 99\n");
        let d = source.diagnostic(
            Severity::Error,
            Some(9..11),
            Some("_.microkitchen.cpus"),
            "bad",
        );
        assert_eq!(
            d.to_string(),
            "error: /p/mise.toml:2:8: _.microkitchen.cpus: bad"
        );
        assert_eq!(
            Diagnostic::general(Severity::Warning, "w").to_string(),
            "warning: w"
        );
    }
}
