//! Declared text search configurations and dictionaries (#2728, DEC-2728.1).
//!
//! A different namespace from the declared types: a configuration and a
//! dictionary commonly share one name (`app.russian`), and either can share it
//! with a domain, so one map keyed by a type's name would collide. Held here, a
//! pull can read them and a declaration can name them; planning them arrives with
//! the next part.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;

use crate::TypeRef;

/// Which text search object a name is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SearchKind {
    Config,
    Dictionary,
}

impl SearchKind {
    /// The word the engine uses: `TEXT SEARCH CONFIGURATION`.
    #[must_use]
    pub fn engine_word(self) -> &'static str {
        match self {
            Self::Config => "text search configuration",
            Self::Dictionary => "text search dictionary",
        }
    }

    /// The leading key of its declaration file.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Config => "text_search_config",
            Self::Dictionary => "text_search_dictionary",
        }
    }
}

/// A text search object's identity: its kind and its schema-qualified name. The
/// kind is part of the key because the engine keeps the two in separate namespaces.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SearchRef {
    pub kind: SearchKind,
    pub name: TypeRef,
}

impl SearchRef {
    #[must_use]
    pub fn new(kind: SearchKind, name: TypeRef) -> Self {
        Self { kind, name }
    }

    /// The canonical spelling for a person, escaped exactly once.
    #[must_use]
    pub fn shown(&self) -> impl fmt::Display + '_ {
        ShownSearch(self)
    }

    /// The canonical spelling for identity and serialization: `config app.docs`.
    #[must_use]
    pub fn raw(&self) -> impl fmt::Display + '_ {
        RawSearch(self)
    }
}

struct ShownSearch<'a>(&'a SearchRef);
struct RawSearch<'a>(&'a SearchRef);

impl fmt::Display for ShownSearch<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} `{}`", self.0.kind.engine_word(), self.0.name.shown())
    }
}

impl fmt::Display for RawSearch<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let word = match self.0.kind {
            SearchKind::Config => "config",
            SearchKind::Dictionary => "dictionary",
        };
        write!(f, "{word} {}", self.0.name.raw())
    }
}

impl FromStr for SearchRef {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (word, name) = text
            .split_once(' ')
            .ok_or_else(|| format!("`{text}` is not `config <name>` or `dictionary <name>`"))?;
        let kind = match word {
            "config" => SearchKind::Config,
            "dictionary" => SearchKind::Dictionary,
            other => return Err(format!("`{other}` is not a text search kind")),
        };
        let name = name.parse::<TypeRef>().map_err(|e| e.to_string())?;
        Ok(Self { kind, name })
    }
}

impl serde::Serialize for SearchRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.raw())
    }
}

impl<'de> serde::Deserialize<'de> for SearchRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

string_json_schema!(SearchRef);

/// What a text search object is, without catalog identity or ownership. Names are
/// kept as the schema-qualified reference the catalog shows: a built-in parser or
/// template lives in `pg_catalog`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SearchObject {
    Config {
        /// The parser. Only the built-in one is held (`pg_catalog."default"`).
        parser: TypeRef,
        /// Per token type of the parser, by alias (`asciiword`), the dictionaries
        /// tried in order. Never by the token's number.
        mappings: BTreeMap<String, Vec<TypeRef>>,
    },
    Dictionary {
        /// The template. Only the built-in ones are held.
        template: TypeRef,
        /// The options, verbatim as the catalog keeps their values; a file-backed
        /// dictionary's file name is one of them.
        options: BTreeMap<String, String>,
    },
}

impl SearchObject {
    #[must_use]
    pub fn kind(&self) -> SearchKind {
        match self {
            Self::Config { .. } => SearchKind::Config,
            Self::Dictionary { .. } => SearchKind::Dictionary,
        }
    }
}
