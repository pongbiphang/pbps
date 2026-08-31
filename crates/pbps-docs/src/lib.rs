//! Documentation and ERD rendering from the declarations (SPEC §9.4).
//!
//! # Why this earns its own crate
//!
//! Rendering is a pure function from `(Schema, IdsFile)` to text: no dialect, no
//! connection, no configuration. Keeping it out of `pbps-cli` is what lets every
//! output format be tested directly, including the properties that matter most —
//! that the HTML references nothing external, and that identical declarations
//! produce byte-identical files.
//!
//! # Why the declarations, not the database
//!
//! Introspection can rebuild the structure; it can never produce the four things
//! this output exists for: `description` fields, deprecation reasons, the ids
//! file's tombstones, and — with them — the reason `pbps fmt` is allowed to
//! discard ordinary YAML comments (SPEC §4.3). The discipline of writing prose
//! into `description` is repaid here.

use pbps_model::{IdsFile, Schema};

pub mod erd;
pub mod html;
pub mod markdown;

/// Which rendering to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Html,
    /// The Mermaid `erDiagram` alone, for embedding in documentation a team
    /// already maintains.
    Erd,
}

impl Format {
    pub const fn extension(self) -> &'static str {
        match self {
            Format::Markdown => "md",
            Format::Html => "html",
            Format::Erd => "mmd",
        }
    }

    pub const ALL: [Format; 3] = [Format::Markdown, Format::Html, Format::Erd];

    pub const fn as_str(self) -> &'static str {
        match self {
            Format::Markdown => "markdown",
            Format::Html => "html",
            Format::Erd => "erd",
        }
    }
}

impl std::str::FromStr for Format {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Format::ALL
            .into_iter()
            .find(|f| f.as_str() == s)
            .ok_or_else(|| {
                let all: Vec<_> = Format::ALL.iter().map(|f| f.as_str()).collect();
                format!("unknown format `{s}`; available: {}", all.join(", "))
            })
    }
}

/// Renders the declarations in the chosen format.
pub fn render(schema: &Schema, ids: &IdsFile, format: Format, title: &str) -> String {
    match format {
        Format::Markdown => markdown::render(schema, ids, title),
        Format::Html => html::render(schema, ids, title),
        Format::Erd => erd::render(schema),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_names_round_trip() {
        for f in Format::ALL {
            assert_eq!(f.as_str().parse::<Format>().unwrap(), f);
        }
        assert!("pdf".parse::<Format>().is_err());
    }

    /// Every format must cope with a project that has nothing in it yet, rather
    /// than panicking on the first `pull` of an empty database.
    #[test]
    fn an_empty_schema_renders_in_every_format() {
        for f in Format::ALL {
            let out = render(&Schema::default(), &IdsFile::default(), f, "Empty");
            assert!(!out.is_empty(), "{f:?} produced nothing");
        }
    }

    /// A module's definition *is* the object (ADR-0002), so documentation that
    /// left it out would document the name and nothing else.
    #[test]
    fn a_modules_definition_is_documented_in_every_prose_format() {
        let mut schema = Schema::default();
        schema.modules.insert(
            "dbo.active_customer".parse().unwrap(),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: Some("Customers that are not legacy records".into()),
                on: None,
                definition: "SELECT customer_id FROM dbo.customer".into(),
            },
        );

        for f in [Format::Markdown, Format::Html] {
            let out = render(&schema, &IdsFile::default(), f, "Schema");
            assert!(out.contains("dbo.active_customer"), "{f:?}: {out}");
            assert!(
                out.contains("SELECT customer_id FROM dbo.customer"),
                "{f:?}"
            );
            assert!(
                out.contains("Customers that are not legacy records"),
                "{f:?}"
            );
            // Determinism is the promise of this command: the same declarations
            // produce byte-identical files.
            assert_eq!(out, render(&schema, &IdsFile::default(), f, "Schema"));
        }
    }
}
