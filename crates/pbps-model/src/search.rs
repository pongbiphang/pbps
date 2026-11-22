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

use crate::{CollationName, TypeRef};

/// A text search object's name. Not a [`TypeRef`]: that refuses a leading underscore, which
/// is how the engine spells an array type, and a text search object has none.
pub type SearchName = CollationName;

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
    pub name: SearchName,
}

impl SearchRef {
    #[must_use]
    pub fn new(kind: SearchKind, name: SearchName) -> Self {
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
        let name = name.parse::<SearchName>().map_err(|e| e.to_string())?;
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
        mappings: BTreeMap<String, Vec<SearchName>>,
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

    /// The `option = file` pairs of a dictionary that read a file of the server's
    /// `tsearch_data` directory, in option order: files this tool neither holds nor can
    /// see from SQL. The engine opens each when the dictionary is created (measured), so a
    /// create is the check and the plan says so rather than leave it to the apply.
    #[must_use]
    pub fn server_files(&self) -> Vec<(&str, &str)> {
        const FILE_OPTIONS: &[&str] = &["dictfile", "afffile", "stopwords", "synonyms"];
        match self {
            Self::Dictionary { options, .. } => options
                .iter()
                .filter(|(key, _)| FILE_OPTIONS.iter().any(|f| key.eq_ignore_ascii_case(f)))
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect(),
            Self::Config { .. } => Vec::new(),
        }
    }

    /// This object with the dictionaries `renames` moved (old reference to new) spelled their
    /// new way: what a configuration's mappings read once the plan has renamed a dictionary.
    /// The engine keeps a mapping by the dictionary's oid, so a rename changes how the mapping
    /// is spelled and nothing it does (DEC-2728.5). A name is respelled once, so two renames
    /// that exchange or chain names never apply twice.
    #[must_use]
    pub fn respelled(&self, renames: &BTreeMap<SearchRef, SearchRef>) -> Self {
        let mut object = self.clone();
        if let Self::Config { mappings, .. } = &mut object {
            for name in mappings.values_mut().flatten() {
                let used = SearchRef::new(SearchKind::Dictionary, name.clone());
                if let Some(moved) = renames.get(&used) {
                    *name = moved.name.clone();
                }
            }
        }
        object
    }

    /// The dictionaries this object uses, each once, as references: what must stand
    /// before it is created and be gone before they are dropped. A configuration uses the
    /// ones it maps; a dictionary uses the one its `dictionary` option names (a thesaurus's
    /// sub-dictionary), when that is spelled with its schema. An unqualified name is the
    /// engine's to resolve by its search path and is not guessed at.
    #[must_use]
    pub fn dictionaries(&self) -> std::collections::BTreeSet<SearchRef> {
        match self {
            Self::Config { mappings, .. } => mappings
                .values()
                .flatten()
                .map(|name| SearchRef::new(SearchKind::Dictionary, name.clone()))
                .collect(),
            Self::Dictionary { .. } => self
                .sub_dictionary()
                .and_then(|value| value.parse::<SearchName>().ok())
                .map(|name| SearchRef::new(SearchKind::Dictionary, name))
                .into_iter()
                .collect(),
        }
    }

    /// The dictionary option that names another dictionary (a thesaurus's), as written.
    fn sub_dictionary(&self) -> Option<&str> {
        match self {
            Self::Dictionary { options, .. } => options
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("dictionary"))
                .map(|(_, value)| value.as_str()),
            Self::Config { .. } => None,
        }
    }

    /// The sub-dictionary this object names in a spelling that is not `schema.name`, as
    /// written. [`Self::dictionaries`] cannot see it: the engine resolves it by the search
    /// path of whoever uses it, so the create order and the drop guard would silently miss
    /// the edge. Callers refuse it (load) or leave the object out (pull) instead.
    #[must_use]
    pub fn unqualified_sub_dictionary(&self) -> Option<&str> {
        self.sub_dictionary()
            .filter(|value| value.parse::<SearchName>().is_err())
    }
}

/// What the target's catalog says stands on a text search object a plan would alter in place
/// (DEC-2728.3, DEC-2728.4), one line per dependent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchCensus {
    /// Stored dependents the catalog proves reach the object: refused by name.
    pub stored: Vec<String>,
    /// Stored dependents behind a function body or a dictionary the catalog cannot see into,
    /// each line naming both: carried by the alteration, which needs `stale-results`.
    pub opaque: Vec<String>,
}

/// One statement of a configuration's change: `DROP`, `ALTER` or `ADD`, the token types it
/// names, and the dictionaries it maps them to (none for a `DROP`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingStep {
    pub verb: &'static str,
    pub aliases: Vec<String>,
    pub dictionaries: Vec<SearchName>,
}

/// The statements that bring the mappings `was` to `now`, in the order they run. `ALTER MAPPING`
/// on a token type that is not mapped does nothing and `ADD MAPPING` on one that is fails
/// (measured on 16 and 18), so each token type gets the one verb that does what it says: the
/// ones given up are dropped, the ones changed altered and the new ones added, one statement per
/// group of token types that share their dictionaries (the order of the groups does not matter,
/// the order inside one does). The emit and the chain a staged apply checkpoints on
/// (`SearchObject::alteration_chain`) both come from here, so a checkpoint is judged against
/// exactly what the statements do.
#[must_use]
pub fn mapping_steps(
    was: &BTreeMap<String, Vec<SearchName>>,
    now: &BTreeMap<String, Vec<SearchName>>,
) -> Vec<MappingStep> {
    let gone: Vec<String> = was
        .keys()
        .filter(|alias| !now.contains_key(*alias))
        .cloned()
        .collect();
    let mut steps = Vec::new();
    if !gone.is_empty() {
        steps.push(MappingStep {
            verb: "DROP",
            aliases: gone,
            dictionaries: Vec::new(),
        });
    }
    for verb in ["ALTER", "ADD"] {
        let changed = now
            .iter()
            .filter(|(alias, list)| was.get(*alias) != Some(*list))
            .filter(|(alias, _)| was.contains_key(*alias) == (verb == "ALTER"));
        let mut groups: Vec<MappingStep> = Vec::new();
        for (alias, list) in changed {
            match groups.iter_mut().find(|g| &g.dictionaries == list) {
                Some(group) => group.aliases.push(alias.clone()),
                None => groups.push(MappingStep {
                    verb,
                    aliases: vec![alias.clone()],
                    dictionaries: list.clone(),
                }),
            }
        }
        steps.extend(groups);
    }
    steps
}

impl SearchObject {
    /// The definitions an in-place change from `self` to `after` stands at after each of its
    /// statements, in order, the last being `after` (DEC-2728.3). Empty when no statement runs.
    /// A mapping change of several statements passes through definitions that are neither end,
    /// and a staged apply checkpoints on each; the options of a dictionary are one statement.
    #[must_use]
    pub fn alteration_chain(&self, after: &Self) -> Vec<Self> {
        match (self, after) {
            (
                Self::Config { mappings: was, .. },
                Self::Config {
                    mappings: now,
                    parser,
                },
            ) => {
                let mut standing = was.clone();
                mapping_steps(was, now)
                    .into_iter()
                    .map(|step| {
                        for alias in step.aliases {
                            if step.dictionaries.is_empty() {
                                standing.remove(&alias);
                            } else {
                                standing.insert(alias, step.dictionaries.clone());
                            }
                        }
                        Self::Config {
                            parser: parser.clone(),
                            mappings: standing.clone(),
                        }
                    })
                    .collect()
            }
            (Self::Dictionary { options: was, .. }, Self::Dictionary { options: now, .. })
                if was != now =>
            {
                vec![after.clone()]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(text: &str) -> TypeRef {
        text.parse().unwrap()
    }

    fn named(text: &str) -> SearchName {
        text.parse().unwrap()
    }

    fn config(mapped: &[(&str, &[&str])]) -> SearchObject {
        SearchObject::Config {
            parser: reference("pg_catalog.\"default\""),
            mappings: mapped
                .iter()
                .map(|(alias, names)| {
                    (
                        (*alias).to_owned(),
                        names.iter().map(|n| named(n)).collect(),
                    )
                })
                .collect(),
        }
    }

    fn dictionary(options: &[(&str, &str)]) -> SearchObject {
        SearchObject::Dictionary {
            template: reference("pg_catalog.thesaurus"),
            options: options
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    /// A configuration uses each dictionary it maps once, however many tokens map it; a
    /// thesaurus uses the dictionary its option names when that is qualified. Negatives: a
    /// plain dictionary uses none, and an unqualified name is the engine's to resolve.
    #[test]
    fn the_dictionaries_an_object_uses_are_the_ones_it_maps_or_names_qualified() {
        let used = config(&[
            ("asciiword", &["app.a", "pg_catalog.simple"]),
            ("word", &["app.a"]),
        ])
        .dictionaries();
        let names: Vec<_> = used.iter().map(|d| d.raw().to_string()).collect();
        assert_eq!(names, ["dictionary app.a", "dictionary pg_catalog.simple"]);
        let thesaurus = dictionary(&[("DICTIONARY", "app.sub"), ("dictfile", "mythes")]);
        assert_eq!(thesaurus.dictionaries().len(), 1);
        assert!(
            dictionary(&[("dictionary", "english_stem")])
                .dictionaries()
                .is_empty()
        );
        assert!(dictionary(&[]).dictionaries().is_empty());
        assert!(config(&[]).dictionaries().is_empty());
    }

    /// A sub-dictionary spelled without its schema is reported, whatever the case of the
    /// option name; a qualified one, no option, and a configuration are not. Negative: a
    /// name that is not `schema.name` at all (three parts, empty) is reported too.
    #[test]
    fn a_sub_dictionary_without_its_schema_is_reported() {
        let unqualified = |o: &[(&str, &str)]| {
            dictionary(o)
                .unqualified_sub_dictionary()
                .map(str::to_owned)
        };
        assert_eq!(
            unqualified(&[("dictionary", "english_stem")]).as_deref(),
            Some("english_stem")
        );
        assert_eq!(
            unqualified(&[("DICTIONARY", "english_stem")]).as_deref(),
            Some("english_stem")
        );
        assert_eq!(
            unqualified(&[("dictionary", "a.b.c")]).as_deref(),
            Some("a.b.c")
        );
        assert_eq!(unqualified(&[("dictionary", "")]).as_deref(), Some(""));
        assert_eq!(unqualified(&[("dictionary", "app.sub")]), None);
        assert_eq!(unqualified(&[("dictionary", "_sub.x")]), None);
        assert_eq!(unqualified(&[("dictfile", "mythes")]), None);
        assert_eq!(unqualified(&[]), None);
        assert_eq!(config(&[]).unqualified_sub_dictionary(), None);
    }

    /// A text search object has no array type, so its name may start with an underscore,
    /// which a type's may not: it parses, reads back through serde, and is the dictionary a
    /// configuration maps or a thesaurus names (ready round 2). Negatives: a type name still
    /// refuses it, and an empty or unqualified name is refused as a search name.
    #[test]
    fn a_search_name_may_start_with_an_underscore_where_a_type_name_may_not() {
        let name: SearchRef = "dictionary app._words".parse().unwrap();
        assert_eq!(name.raw().to_string(), "dictionary app._words");
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(serde_json::from_str::<SearchRef>(&json).unwrap(), name);
        assert!(
            config(&[("word", &["app._words"])])
                .dictionaries()
                .contains(&name)
        );
        assert!(
            dictionary(&[("dictionary", "app._words")])
                .dictionaries()
                .contains(&name)
        );
        assert!("app._words".parse::<TypeRef>().is_err());
        for bad in [
            "dictionary words",
            "dictionary app.",
            "dictionary .words",
            "dictionary ",
        ] {
            assert!(bad.parse::<SearchRef>().is_err(), "{bad}");
        }
    }

    /// The options that read a file of the server are found by name, whatever their case;
    /// a configuration reads none.
    #[test]
    fn the_server_files_a_dictionary_reads_are_its_file_options() {
        let object = dictionary(&[("DictFile", "mythes"), ("language", "english")]);
        assert_eq!(object.server_files(), [("DictFile", "mythes")]);
        assert!(
            dictionary(&[("language", "english")])
                .server_files()
                .is_empty()
        );
        assert!(config(&[("word", &["app.a"])]).server_files().is_empty());
    }

    fn steps(was: &[(&str, &[&str])], now: &[(&str, &[&str])]) -> Vec<String> {
        let map = |entries: &[(&str, &[&str])]| match config(entries) {
            SearchObject::Config { mappings, .. } => mappings,
            SearchObject::Dictionary { .. } => unreachable!(),
        };
        mapping_steps(&map(was), &map(now))
            .into_iter()
            .map(|step| {
                format!(
                    "{} {} -> {}",
                    step.verb,
                    step.aliases.join("+"),
                    step.dictionaries
                        .iter()
                        .map(|d| format!("{}.{}", d.schema(), d.name()))
                        .collect::<Vec<_>>()
                        .join("+")
                )
            })
            .collect()
    }

    /// A configuration's change is the one verb each token type needs, in the order they run: the
    /// ones given up are dropped, the ones changed are altered and the new ones added, and token
    /// types that share their dictionaries share a statement (DEC-2728.3). Negatives: a token type
    /// that is the same on both sides is not a step, and the same mappings are no step at all.
    #[test]
    fn a_configurations_change_is_one_statement_per_group_with_the_verb_each_token_type_needs() {
        let was = [
            ("asciiword", &["app.a"][..]),
            ("email", &["app.a"]),
            ("url", &["app.a"]),
            ("word", &["app.a"]),
        ];
        let now = [
            ("asciiword", &["app.a"][..]),
            ("int", &["app.b"]),
            ("uint", &["app.b"]),
            ("url", &["app.b"]),
            ("word", &["app.b"]),
        ];
        assert_eq!(
            steps(&was, &now),
            [
                "DROP email -> ",
                "ALTER url+word -> app.b",
                "ADD int+uint -> app.b"
            ]
        );
        assert!(steps(&was, &was).is_empty());
        // Created from nothing, every token type is an `ADD`.
        assert_eq!(steps(&[], &was[..2]), ["ADD asciiword+email -> app.a"]);
    }

    /// The definitions an alteration stands at between its statements are the ones each statement
    /// leaves, the last being the result, and they come from the same steps the emit runs
    /// (SPEC §7.6, DEC-2728.4): a mapping change of three statements has three, none of which
    /// need be either end; a dictionary's options are one statement. Negatives: no change has no
    /// statement and so no definition, and another kind of object has none either.
    #[test]
    fn an_alteration_stands_at_the_definition_after_each_of_its_statements() {
        let was = config(&[
            ("email", &["app.a"]),
            ("url", &["app.a"]),
            ("word", &["app.a"]),
        ]);
        let now = config(&[
            ("int", &["app.b"]),
            ("url", &["app.b"]),
            ("word", &["app.a"]),
        ]);
        let chain = was.alteration_chain(&now);
        assert_eq!(chain.len(), 3);
        assert_eq!(
            chain,
            [
                config(&[("url", &["app.a"]), ("word", &["app.a"])]),
                config(&[("url", &["app.b"]), ("word", &["app.a"])]),
                now.clone(),
            ]
        );
        assert!(!chain.contains(&was));
        assert!(was.alteration_chain(&was).is_empty());
        let (one, two) = (dictionary(&[("a", "1")]), dictionary(&[("a", "2")]));
        assert_eq!(one.alteration_chain(&two), std::slice::from_ref(&two));
        assert!(one.alteration_chain(&one).is_empty());
        assert!(one.alteration_chain(&was).is_empty());
    }
}
