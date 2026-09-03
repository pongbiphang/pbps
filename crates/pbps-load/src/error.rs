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

    /// Where the label lands, 1-based.
    ///
    /// Derived at construction rather than on demand: the machine-readable
    /// output (SPEC §14.1) needs a line number, and by the time it asks, the
    /// only copy of the source text is inside miette's `NamedSource`, which
    /// exists to be rendered rather than measured.
    pub line: usize,
    pub column: usize,
}

impl LoadError {
    pub fn semantic(
        src: &SourceFile,
        span: SourceSpan,
        message: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        let (line, column) = line_and_column(&src.text, span.offset());
        LoadError::Semantic(Box::new(Semantic {
            message: message.into(),
            src: src.named_source(),
            span,
            label: label.into(),
            help: None,
            line,
            column,
        }))
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        if let LoadError::Semantic(s) = &mut self {
            s.help = Some(help.into());
        }
        self
    }

    /// The file the problem is in.
    pub fn path(&self) -> Option<&Path> {
        match self {
            LoadError::Io { path, .. } | LoadError::Yaml { path, .. } => Some(path),
            LoadError::Semantic(s) => Some(Path::new(s.src.name())),
        }
    }

    /// The 1-based line, when one is known.
    ///
    /// A YAML structural error has one too, but only inside the parser's own
    /// pre-rendered message; re-parsing that text to recover a number the
    /// diagnostic already prints would be a second, more fragile copy of it.
    pub fn line(&self) -> Option<usize> {
        match self {
            LoadError::Semantic(s) => Some(s.line),
            LoadError::Io { .. } | LoadError::Yaml { .. } => None,
        }
    }

    /// A stable identifier for the kind of problem, for `--format json`.
    pub fn id(&self) -> &'static str {
        match self {
            LoadError::Io { .. } => "load.io",
            LoadError::Yaml { .. } => "load.yaml",
            LoadError::Semantic(_) => "load.semantic",
        }
    }
}

/// The 1-based line and column of a byte offset.
///
/// Both counted in characters for the column, because that is what an editor
/// shows; the offset itself is a byte offset (see [`to_span`]).
fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    // `get` rather than a slice: an offset that is not on a character boundary
    // would panic, and a diagnostic must never be the thing that crashes the
    // run it was trying to explain.
    let Some(before) = text.get(..offset) else {
        return (1, 1);
    };
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit_once('\n')
        .map_or(before, |(_, last)| last)
        .chars()
        .count()
        + 1;
    (line, column)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_offset_maps_to_the_line_an_editor_shows() {
        let text = "a: 1\nbb: 2\nccc: 3\n";
        assert_eq!(line_and_column(text, 0), (1, 1));
        assert_eq!(line_and_column(text, 5), (2, 1));
        assert_eq!(line_and_column(text, 7), (2, 3));
        assert_eq!(line_and_column(text, 11), (3, 1));
    }

    /// The column is in characters while the offset is in bytes (see
    /// [`to_span`]); counting bytes here would put the caret past the end of a
    /// line containing any non-ASCII text, which a `description:` very often
    /// does.
    #[test]
    fn a_multibyte_line_does_not_shift_the_column() {
        let text = "description: 中文\nname: t\n";
        let offset = text.find("name").unwrap();
        assert_eq!(line_and_column(text, offset), (2, 1));
        // Three characters into the second line, not three bytes into it.
        assert_eq!(line_and_column(text, offset + 3), (2, 4));
    }

    /// Offsets reach this from a parser, and a parser that is wrong about one
    /// must not take the process down with it.
    #[test]
    fn an_out_of_range_or_split_offset_is_not_a_panic() {
        let text = "中文";
        assert_eq!(line_and_column(text, 1), (1, 1));
        assert_eq!(line_and_column(text, 9_999), (1, 1));
    }
}
