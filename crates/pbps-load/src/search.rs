//! Text search configuration and dictionary declaration files (#2728, DEC-2728.1).

use std::path::Path;

use pbps_model::{SearchKind, SearchObject, SearchRef, TypeRef};

use crate::{
    LoadError,
    dto::{SearchConfigDto, SearchDictionaryDto},
    fmt::scalar,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSearch {
    pub name: SearchRef,
    pub definition: SearchObject,
}

fn reference(text: &str, what: &str) -> Result<TypeRef, String> {
    text.parse::<TypeRef>()
        .map_err(|e| format!("{what} `{}`: {e}", pbps_model::escape::Escaped(text)))
}

/// Loads a `text_search_config:` or `text_search_dictionary:` file. Which of the
/// two is the leading key; `path` is only for diagnostics.
pub fn load_search_str(path: &Path, text: &str) -> Result<LoadedSearch, Vec<LoadError>> {
    let fail = |message: String| {
        vec![LoadError::Yaml {
            path: path.into(),
            message,
        }]
    };
    let probe: crate::dto::KindProbe =
        serde_saphyr::from_str(text).map_err(|e| fail(e.to_string()))?;
    match (probe.text_search_config, probe.text_search_dictionary) {
        (Some(_), None) => {
            let dto: SearchConfigDto =
                serde_saphyr::from_str(text).map_err(|e| fail(e.to_string()))?;
            let name =
                reference(&dto.text_search_config, "text search configuration").map_err(fail)?;
            let parser = reference(&dto.parser, "parser").map_err(fail)?;
            let mut mappings = std::collections::BTreeMap::new();
            for (alias, dictionaries) in dto.mappings {
                if dictionaries.is_empty() {
                    return Err(fail(format!(
                        "text search configuration `{}`: the mapping for `{}` lists no dictionary",
                        name.shown(),
                        pbps_model::escape::Escaped(&alias)
                    )));
                }
                let parsed = dictionaries
                    .iter()
                    .map(|d| reference(d, "dictionary"))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(fail)?;
                mappings.insert(alias, parsed);
            }
            Ok(LoadedSearch {
                name: SearchRef::new(SearchKind::Config, name),
                definition: SearchObject::Config { parser, mappings },
            })
        }
        (None, Some(_)) => {
            let dto: SearchDictionaryDto =
                serde_saphyr::from_str(text).map_err(|e| fail(e.to_string()))?;
            let name =
                reference(&dto.text_search_dictionary, "text search dictionary").map_err(fail)?;
            let template = reference(&dto.template, "template").map_err(fail)?;
            Ok(LoadedSearch {
                name: SearchRef::new(SearchKind::Dictionary, name),
                definition: SearchObject::Dictionary {
                    template,
                    options: dto.options,
                },
            })
        }
        _ => Err(fail(
            "a text search file declares exactly one of `text_search_config:` or \
             `text_search_dictionary:`"
                .into(),
        )),
    }
}

/// The declaration file text of one object: what [`load_search_str`] reads back.
pub fn render_search(name: &SearchRef, definition: &SearchObject) -> String {
    let mut out = format!(
        "{}: {}\n",
        name.kind.key(),
        scalar(&name.name.raw().to_string())
    );
    match definition {
        SearchObject::Config { parser, mappings } => {
            out.push_str(&format!("parser: {}\n", scalar(&parser.raw().to_string())));
            if mappings.is_empty() {
                out.push_str("mappings: {}\n");
            } else {
                out.push_str("mappings:\n");
                for (alias, dictionaries) in mappings {
                    let listed: Vec<String> = dictionaries
                        .iter()
                        .map(|d| scalar(&d.raw().to_string()))
                        .collect();
                    out.push_str(&format!("  {}: [{}]\n", scalar(alias), listed.join(", ")));
                }
            }
        }
        SearchObject::Dictionary { template, options } => {
            out.push_str(&format!(
                "template: {}\n",
                scalar(&template.raw().to_string())
            ));
            if !options.is_empty() {
                out.push_str("options:\n");
                for (key, value) in options {
                    out.push_str(&format!("  {}: {}\n", scalar(key), scalar(value)));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> LoadedSearch {
        load_search_str(Path::new("c.yml"), text).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn refusal(text: &str) -> String {
        load_search_str(Path::new("c.yml"), text)
            .unwrap_err()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Both kinds render to text that loads back to the same object, byte for byte,
    /// including names that need quoting and option values that look like other
    /// scalars.
    #[test]
    fn a_configuration_and_a_dictionary_round_trip() {
        let dictionary = "text_search_dictionary: app.d1\ntemplate: pg_catalog.snowball\n\
                          options:\n  language: english\n  stopwords: english\n";
        let loaded = config(dictionary);
        assert_eq!(loaded.name.kind, SearchKind::Dictionary);
        assert_eq!(render_search(&loaded.name, &loaded.definition), dictionary);

        let configuration = "text_search_config: app.docs\nparser: pg_catalog.\"default\"\n\
                             mappings:\n  asciiword: [app.d1, pg_catalog.simple]\n  email: [pg_catalog.simple]\n";
        let loaded = config(configuration);
        assert_eq!(loaded.name.kind, SearchKind::Config);
        let rendered = render_search(&loaded.name, &loaded.definition);
        assert_eq!(config(&rendered), loaded);

        // Values that other scalars would swallow stay text.
        let odd = SearchObject::Dictionary {
            template: "pg_catalog.simple".parse().unwrap(),
            options: [
                ("accept".into(), "false".into()),
                ("stopwords".into(), "5".into()),
                ("synonyms".into(), "with: colon".into()),
                ("empty".into(), String::new()),
            ]
            .into(),
        };
        let name = SearchRef::new(SearchKind::Dictionary, "app.\"Odd Name\"".parse().unwrap());
        let rendered = render_search(&name, &odd);
        let back = config(&rendered);
        assert_eq!((back.name, back.definition), (name, odd), "{rendered}");
        // An empty configuration is legal and round trips.
        let empty = SearchObject::Config {
            parser: "pg_catalog.\"default\"".parse().unwrap(),
            mappings: Default::default(),
        };
        let name = SearchRef::new(SearchKind::Config, "app.empty".parse().unwrap());
        assert_eq!(config(&render_search(&name, &empty)).definition, empty);
    }

    /// Negatives: no leading kind, both, an unqualified or malformed name, an empty
    /// mapping, an unknown key (a rename is not part of this first slice).
    #[test]
    fn a_text_search_file_that_cannot_be_read_is_refused_by_name() {
        for (text, expected) in [
            ("parser: pg_catalog.simple\n", "exactly one"),
            (
                "text_search_config: app.c\ntext_search_dictionary: app.d\nparser: x.y\n",
                "exactly one",
            ),
            (
                "text_search_config: c\nparser: pg_catalog.\"default\"\n",
                "c",
            ),
            (
                "text_search_config: app.c\nparser: pg_catalog.\"default\"\nmappings:\n  word: []\n",
                "lists no dictionary",
            ),
            (
                "text_search_config: app.c\nparser: pg_catalog.\"default\"\nrenamed_from: app.old\n",
                "renamed_from",
            ),
            ("text_search_dictionary: app.d\n", "template"),
            (
                "text_search_dictionary: app.d\ntemplate: pg_catalog.simple\noptions: [a]\n",
                "options",
            ),
        ] {
            let said = refusal(text);
            assert!(said.contains(expected), "{text:?}: {said}");
        }
    }

    /// The file kind is the leading key, so the two kinds of one name, and a domain of
    /// it, are three declarations.
    #[test]
    fn two_kinds_of_one_name_are_two_objects() {
        let a = config("text_search_config: app.russian\nparser: pg_catalog.\"default\"\n");
        let b = config("text_search_dictionary: app.russian\ntemplate: pg_catalog.simple\n");
        assert_eq!(a.name.name, b.name.name);
        assert_ne!(a.name, b.name);
    }
}
