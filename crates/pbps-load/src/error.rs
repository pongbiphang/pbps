//! Load-time diagnostics.
//!
//! There are two kinds, deliberately presented differently:
//!
//! - **YAML structural errors** (syntax, unknown fields, type mismatches,
//!   duplicate keys) are detected by `serde-saphyr`. Its messages already carry
//!   line, column and a caret-annotated source excerpt, and they are good, so we
//!   pass them through — wrapping them in miette as well would only print the
//!   same source twice.
//! - **Semantic errors** (an invalid type name, a malformed table name, …) are
//!   our own checks. The YAML itself is valid and it is the value that is wrong,
//!   so we locate it ourselves and render a full miette diagnostic.

use std::path::{Path, PathBuf};

use miette::{Diagnostic, NamedSource, SourceSpan};

#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum LoadError {
    #[error("cannot read `{path}`")]
    #[diagnostic(code(pbps::load::io))]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("there is a problem with the YAML in `{path}`\n\n{message}")]
    #[diagnostic(code(pbps::load::yaml))]
    Yaml { path: PathBuf, message: String },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Semantic(#[from] Box<Semantic>),
}

/// A value that is semantically invalid: the YAML parses, but its content is
/// wrong.
#[derive(Debug, thiserror::Error, Diagnostic)]
#[error("{message}")]
#[diagnostic(code(pbps::load::semantic))]
pub struct Semantic {
    pub message: String,

    #[source_code]
    pub src: NamedSource<String>,

    #[label("{label}")]
    pub span: SourceSpan,

    pub label: String,

    #[help]
    pub help: Option<String>,
}

impl LoadError {
    pub fn semantic(
        src: &SourceFile,
        span: SourceSpan,
        message: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        LoadError::Semantic(Box::new(Semantic {
            message: message.into(),
            src: src.named_source(),
            span,
            label: label.into(),
            help: None,
        }))
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        if let LoadError::Semantic(s) = &mut self {
            s.help = Some(help.into());
        }
        self
    }
}

/// The name and content of one source file, for diagnostics to quote.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub name: String,
    pub text: String,
}

impl SourceFile {
    pub fn new(path: &Path, text: impl Into<String>) -> Self {
        Self {
            name: path.display().to_string(),
            text: text.into(),
        }
    }

    pub fn named_source(&self) -> NamedSource<String> {
        NamedSource::new(&self.name, self.text.clone())
    }
}

/// Converts a `serde-saphyr` location into a miette span.
///
/// **Byte offsets are mandatory.** `Span::offset()` returns a character offset
/// while miette expects a byte offset; the moment a file contains any non-ASCII
/// character (a `description` very often will), the two diverge and the label
/// lands in the wrong place.
pub fn to_span(loc: &serde_saphyr::Location) -> SourceSpan {
    let s = loc.span();
    let offset = s.byte_offset().unwrap_or_else(|| s.offset()) as usize;
    let len = s.byte_len().unwrap_or_else(|| s.len()) as usize;
    (offset, len).into()
}
