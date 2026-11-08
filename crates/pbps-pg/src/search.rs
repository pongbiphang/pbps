//! Text search configurations and dictionaries: what the pull holds, how a declaration
//! is checked (#2728, DEC-2728.1).
//!
//! Held are the objects users create: not a system object (`oid < 16384`), not a
//! package's member, over a built-in parser or template. A custom parser or template
//! is built from C functions or comes with a package, so an object over one is left out
//! and named by the census, with the configuration that maps it. A mapping that names a
//! system or a package dictionary is held as that reference.
//!
//! One SQL text decides "held" for the pull and for the census (the macros below), so
//! an object is named exactly when it is not read.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::catalog::{Limitation, LimitationTarget};
use pbps_dialect::DialectError;
use pbps_model::{Schema, SearchKind, SearchObject, SearchRef, TypeRef};

/// The built-in parser, which this model holds alone.
pub const PARSER: &str = "default";
/// The built-in templates (measured identical on 16 and 18, `measure` test).
pub const TEMPLATES: &[&str] = &["simple", "synonym", "thesaurus", "ispell", "snowball"];
/// The token types of the built-in parser: the aliases a mapping may name.
pub const TOKEN_ALIASES: &[&str] = &[
    "asciiword",
    "word",
    "numword",
    "email",
    "url",
    "host",
    "sfloat",
    "version",
    "hword_numpart",
    "hword_part",
    "hword_asciipart",
    "blank",
    "tag",
    "protocol",
    "numhword",
    "asciihword",
    "hword",
    "url_path",
    "file",
    "float",
    "int",
    "uint",
    "entity",
];

/// Whether the dictionary behind `$d` (a `pg_ts_dict` alias) is one the pull holds: a
/// user's, not a package's, over a built-in template.
macro_rules! dictionary_held {
    ($d:literal) => {
        concat!(
            "(",
            $d,
            ".oid >= 16384 AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend x \
               WHERE x.classid = 'pg_catalog.pg_ts_dict'::pg_catalog.regclass \
                 AND x.objid = ",
            $d,
            ".oid AND x.deptype = 'e') \
             AND EXISTS (SELECT 1 FROM pg_catalog.pg_ts_template t \
               WHERE t.oid = ",
            $d,
            ".dicttemplate AND t.tmplnamespace = 'pg_catalog'::pg_catalog.regnamespace))"
        )
    };
}

/// Whether the configuration behind `$c` is held: a user's, not a package's, over the
/// built-in parser, mapping no user dictionary that is not itself held.
macro_rules! config_held {
    ($c:literal) => {
        concat!(
            "(",
            $c,
            ".oid >= 16384 AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend x \
               WHERE x.classid = 'pg_catalog.pg_ts_config'::pg_catalog.regclass \
                 AND x.objid = ",
            $c,
            ".oid AND x.deptype = 'e') \
             AND EXISTS (SELECT 1 FROM pg_catalog.pg_ts_parser p \
               WHERE p.oid = ",
            $c,
            ".cfgparser AND p.prsnamespace = 'pg_catalog'::pg_catalog.regnamespace) \
             AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_ts_config_map m \
               JOIN pg_catalog.pg_ts_dict md ON md.oid = m.mapdict \
              WHERE m.mapcfg = ",
            $c,
            ".oid AND md.oid >= 16384 \
                AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend y \
                   WHERE y.classid = 'pg_catalog.pg_ts_dict'::pg_catalog.regclass \
                     AND y.objid = md.oid AND y.deptype = 'e') \
                AND NOT ",
            $crate::search::dictionary_held!("md"),
            "))"
        )
    };
}
pub(crate) use {config_held, dictionary_held};

/// Every held configuration and dictionary, one row each. A configuration's mappings
/// are expanded from `pg_ts_config_map` joined to the parser's own token types, so a
/// mapping is by alias and never by number, in the engine's order.
pub fn query() -> String {
    format!(
        "SELECT 'config' AS kind, n.nspname::pg_catalog.text AS schema,
                c.cfgname::pg_catalog.text AS name,
                pn.nspname::pg_catalog.text AS ref_schema,
                p.prsname::pg_catalog.text AS ref_name,
                NULL::pg_catalog.text AS options,
                COALESCE((SELECT pg_catalog.jsonb_agg(pg_catalog.jsonb_build_array(
                                 tt.alias, dn.nspname::pg_catalog.text, d.dictname::pg_catalog.text)
                              ORDER BY m.maptokentype, m.mapseqno)
                            FROM pg_catalog.pg_ts_config_map m
                            JOIN pg_catalog.pg_ts_dict d ON d.oid = m.mapdict
                            JOIN pg_catalog.pg_namespace dn ON dn.oid = d.dictnamespace
                            LEFT JOIN pg_catalog.ts_token_type(c.cfgparser) tt
                                   ON tt.tokid = m.maptokentype
                           WHERE m.mapcfg = c.oid), '[]'::pg_catalog.jsonb) AS mappings
           FROM pg_catalog.pg_ts_config c
           JOIN pg_catalog.pg_namespace n ON n.oid = c.cfgnamespace
           JOIN pg_catalog.pg_ts_parser p ON p.oid = c.cfgparser
           JOIN pg_catalog.pg_namespace pn ON pn.oid = p.prsnamespace
          WHERE {config}
            AND n.nspname NOT IN ('pg_catalog', 'information_schema')
            AND pg_catalog.left(n.nspname, 3) <> 'pg_'
         UNION ALL
         SELECT 'dictionary', n.nspname::pg_catalog.text, d.dictname::pg_catalog.text,
                tn.nspname::pg_catalog.text, t.tmplname::pg_catalog.text,
                COALESCE(d.dictinitoption, ''), '[]'::pg_catalog.jsonb
           FROM pg_catalog.pg_ts_dict d
           JOIN pg_catalog.pg_namespace n ON n.oid = d.dictnamespace
           JOIN pg_catalog.pg_ts_template t ON t.oid = d.dicttemplate
           JOIN pg_catalog.pg_namespace tn ON tn.oid = t.tmplnamespace
          WHERE {dictionary}
            AND n.nspname NOT IN ('pg_catalog', 'information_schema')
            AND pg_catalog.left(n.nspname, 3) <> 'pg_'",
        config = config_held!("c"),
        dictionary = dictionary_held!("d"),
    )
}

/// The options text the engine keeps (`language = 'english', stopwords = 'english'`) as a
/// map. A value is quoted with the single quote doubled; a bare value is read up to the
/// next comma. A key twice, or text this reader does not know, is an error that names it:
/// the dictionary is then left out rather than held with an option missing.
pub fn parse_options(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace() || *c == ',').is_some() {}
        if chars.peek().is_none() {
            return Ok(out);
        }
        let mut key = String::new();
        if chars.next_if_eq(&'"').is_some() {
            loop {
                match chars.next() {
                    Some('"') if chars.next_if_eq(&'"').is_some() => key.push('"'),
                    Some('"') => break,
                    Some(c) => key.push(c),
                    None => return Err("an unterminated quoted option name".to_owned()),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| *c != '=' && *c != ',') {
                key.push(c);
            }
        }
        let key = key.trim().to_owned();
        if key.is_empty() {
            return Err("an option with no name".to_owned());
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        // An option with no value is the engine's way of removing one; it is never stored.
        if chars.next_if_eq(&'=').is_none() {
            return Err(format!("the option `{key}` has no value"));
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let mut value = String::new();
        if chars.next_if_eq(&'\'').is_some() {
            loop {
                match chars.next() {
                    Some('\'') if chars.next_if_eq(&'\'').is_some() => value.push('\''),
                    Some('\'') => break,
                    Some(c) => value.push(c),
                    None => return Err(format!("the value of `{key}` is not terminated")),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| *c != ',') {
                value.push(c);
            }
            value = value.trim().to_owned();
        }
        if out.insert(key.clone(), value).is_some() {
            return Err(format!("the option `{key}` twice"));
        }
    }
}

fn limitation(kind: SearchKind, schema: &str, name: &str, why: String) -> Limitation {
    let target = TypeRef::new(schema, name)
        .map(|name| LimitationTarget::Search(SearchRef::new(kind, name)))
        .unwrap_or_else(|_| LimitationTarget::Type(pbps_model::TableName::new(schema, name)));
    Limitation {
        detail: format!(
            "{} `{}.{}` cannot be represented: {why}. It is left out of the pull; this is not \
             an absent {}.",
            kind.engine_word(),
            pbps_model::escape::Escaped(schema),
            pbps_model::escape::Escaped(name),
            kind.engine_word()
        ),
        target,
    }
}

/// The held objects of one batch's rows, and what could not be held after all. A
/// configuration that maps a dictionary left out here is left out too, never half held.
pub fn decode(
    rows: &[serde_json::Value],
) -> Result<(BTreeMap<SearchRef, SearchObject>, Vec<Limitation>), pbps_db::DbError> {
    let bad = |what: &str| pbps_db::DbError::BadRow(format!("text search row: {what}"));
    let text = |row: &serde_json::Value, column: &str| {
        row.get(column)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| bad(column))
    };
    let mut held = BTreeMap::new();
    let mut left_out = Vec::new();
    let mut dropped: BTreeSet<SearchRef> = BTreeSet::new();
    let mut configs = Vec::new();
    for row in rows {
        let (schema, name) = (text(row, "schema")?, text(row, "name")?);
        let (ref_schema, ref_name) = (text(row, "ref_schema")?, text(row, "ref_name")?);
        let reference =
            |schema: &str, name: &str| TypeRef::new(schema, name).map_err(|e| e.to_string());
        match text(row, "kind")?.as_str() {
            "dictionary" => {
                let kind = SearchKind::Dictionary;
                let built = reference(&schema, &name).and_then(|n| {
                    Ok((
                        SearchRef::new(kind, n),
                        reference(&ref_schema, &ref_name)?,
                        parse_options(&text(row, "options").map_err(|e| e.to_string())?)?,
                    ))
                });
                match built {
                    Ok((key, template, options)) => {
                        held.insert(key, SearchObject::Dictionary { template, options });
                    }
                    Err(why) => {
                        if let Ok(n) = reference(&schema, &name) {
                            dropped.insert(SearchRef::new(kind, n));
                        }
                        left_out.push(limitation(kind, &schema, &name, why));
                    }
                }
            }
            "config" => configs.push((row, schema, name, ref_schema, ref_name)),
            other => return Err(bad(other)),
        }
    }
    for (row, schema, name, ref_schema, ref_name) in configs {
        let kind = SearchKind::Config;
        let built = (|| -> Result<(SearchRef, SearchObject), String> {
            let key = SearchRef::new(
                kind,
                TypeRef::new(&schema, &name).map_err(|e| e.to_string())?,
            );
            let parser = TypeRef::new(&ref_schema, &ref_name).map_err(|e| e.to_string())?;
            let mut mappings: BTreeMap<String, Vec<TypeRef>> = BTreeMap::new();
            for entry in row
                .get("mappings")
                .and_then(serde_json::Value::as_array)
                .ok_or("its mappings were not read")?
            {
                let [alias, dict_schema, dict_name] = entry
                    .as_array()
                    .map(Vec::as_slice)
                    .ok_or("a mapping row is not a list")?
                else {
                    return Err("a mapping row is not (alias, schema, name)".to_owned());
                };
                let alias = alias
                    .as_str()
                    .ok_or("a mapping names a token type the parser does not list")?;
                let dictionary = TypeRef::new(
                    dict_schema.as_str().ok_or("a mapping names no schema")?,
                    dict_name.as_str().ok_or("a mapping names no dictionary")?,
                )
                .map_err(|e| e.to_string())?;
                if dropped.contains(&SearchRef::new(SearchKind::Dictionary, dictionary.clone())) {
                    return Err(format!(
                        "it maps the dictionary `{}`, which is left out",
                        dictionary.shown()
                    ));
                }
                mappings
                    .entry(alias.to_owned())
                    .or_default()
                    .push(dictionary);
            }
            Ok((key, SearchObject::Config { parser, mappings }))
        })();
        match built {
            Ok((key, object)) => {
                held.insert(key, object);
            }
            Err(why) => left_out.push(limitation(kind, &schema, &name, why)),
        }
    }
    Ok((held, left_out))
}

fn invalid(message: String) -> DialectError {
    DialectError::Invalid {
        dialect: crate::types::DIALECT,
        message,
    }
}

/// What the engine would refuse, or this model does not hold, before a plan is made: the
/// parser and template, the token aliases and the dictionaries a mapping names. A
/// dictionary the project does not declare is the target's to resolve (a system or a
/// package's), so it is checked when connected.
pub(crate) fn validate(
    name: &SearchRef,
    definition: &SearchObject,
    _schema: &Schema,
) -> Vec<DialectError> {
    let mut found = Vec::new();
    let held = |reference: &TypeRef, list: &[&str]| {
        reference.schema() == "pg_catalog" && list.contains(&reference.name())
    };
    match definition {
        SearchObject::Config { parser, mappings } => {
            if !held(parser, &[PARSER]) {
                found.push(invalid(format!(
                    "{} names the parser `{}`, which is created by C functions or by an \
                     extension; this tool holds only the built-in one, `pg_catalog.\"default\"`",
                    name.shown(),
                    parser.shown()
                )));
            }
            for (alias, dictionaries) in mappings {
                if !TOKEN_ALIASES.contains(&alias.as_str()) {
                    found.push(invalid(format!(
                        "{} maps `{}`, which is not a token type of the built-in parser",
                        name.shown(),
                        pbps_model::escape::Escaped(alias)
                    )));
                }
                let mut seen = BTreeSet::new();
                for dictionary in dictionaries {
                    if !seen.insert(dictionary) {
                        found.push(invalid(format!(
                            "{} lists the dictionary `{}` twice for `{}`",
                            name.shown(),
                            dictionary.shown(),
                            pbps_model::escape::Escaped(alias)
                        )));
                    }
                }
            }
        }
        SearchObject::Dictionary { template, options } => {
            if !held(template, TEMPLATES) {
                found.push(invalid(format!(
                    "{} names the template `{}`, which is created by C functions or by an \
                     extension; this tool holds only the built-in ones ({})",
                    name.shown(),
                    template.shown(),
                    TEMPLATES.join(", ")
                )));
            }
            if let Some(key) = options.keys().find(|k| k.trim().is_empty()) {
                found.push(invalid(format!(
                    "{} has an option with no name ({})",
                    name.shown(),
                    pbps_model::escape::Escaped(key)
                )));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(text: &str) -> TypeRef {
        text.parse().unwrap()
    }

    /// The engine's options text as a map: quoted values with the quote doubled, bare
    /// values, an option name that needs quoting. Negatives: a key twice, a name or a
    /// value that is cut off, an option with no value (the engine's removal, never stored).
    #[test]
    fn dictionary_options_are_read_as_a_map_and_a_text_not_understood_is_an_error() {
        let held = |text: &str| parse_options(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert!(held("").is_empty());
        assert_eq!(
            held("language = 'english', stopwords = 'english'"),
            [
                ("language".into(), "english".into()),
                ("stopwords".into(), "english".into())
            ]
            .into()
        );
        assert_eq!(held("accept = false")["accept"], "false");
        assert_eq!(held("synonyms = 'it''s, here'")["synonyms"], "it's, here");
        assert_eq!(held("\"Odd Name\" = 'x'")["Odd Name"], "x");
        assert_eq!(held("a = 'x',b='y'").len(), 2);
        for text in [
            "a = 'x', a = 'y'",
            "a = 'x",
            "\"a = 'x'",
            "a",
            "= 'x'",
            "a = 'x', b",
        ] {
            assert!(parse_options(text).is_err(), "{text:?}");
        }
    }

    fn row(
        kind: &str,
        name: &str,
        reference: (&str, &str),
        options: &str,
        mappings: serde_json::Value,
    ) -> serde_json::Value {
        json!({"kind": kind, "schema": "app", "name": name, "ref_schema": reference.0,
               "ref_name": reference.1, "options": options, "mappings": mappings})
    }

    /// A dictionary and a configuration over it are held, in the engine's order; a
    /// configuration that maps a dictionary left out is left out with it, never half held.
    #[test]
    fn a_configuration_is_held_with_its_dictionaries_and_left_out_with_a_dictionary_that_is() {
        let rows = vec![
            row(
                "dictionary",
                "good",
                ("pg_catalog", "simple"),
                "accept = false",
                json!([]),
            ),
            row(
                "dictionary",
                "bad",
                ("pg_catalog", "simple"),
                "a = 'x', a = 'y'",
                json!([]),
            ),
            row(
                "config",
                "docs",
                ("pg_catalog", "default"),
                "",
                json!([
                    ["asciiword", "app", "good"],
                    ["asciiword", "pg_catalog", "simple"],
                    ["email", "pg_catalog", "simple"]
                ]),
            ),
            row(
                "config",
                "over_bad",
                ("pg_catalog", "default"),
                "",
                json!([["word", "app", "bad"]]),
            ),
            row(
                "config",
                "unnamed",
                ("pg_catalog", "default"),
                "",
                json!([[null, "app", "good"]]),
            ),
        ];
        let (held, left_out) = decode(&rows).unwrap();
        let config = |name: &str| SearchRef::new(SearchKind::Config, t(&format!("app.{name}")));
        let dictionary =
            |name: &str| SearchRef::new(SearchKind::Dictionary, t(&format!("app.{name}")));
        assert_eq!(
            held[&config("docs")],
            SearchObject::Config {
                parser: t("pg_catalog.\"default\""),
                mappings: [
                    (
                        "asciiword".into(),
                        vec![t("app.good"), t("pg_catalog.simple")]
                    ),
                    ("email".into(), vec![t("pg_catalog.simple")]),
                ]
                .into(),
            }
        );
        assert_eq!(
            held[&dictionary("good")],
            SearchObject::Dictionary {
                template: t("pg_catalog.simple"),
                options: [("accept".into(), "false".into())].into(),
            }
        );
        // Negatives: the bad dictionary, the configuration over it, and a mapping whose
        // token type the parser does not list.
        assert_eq!(held.len(), 2, "{held:?}");
        let said: Vec<&str> = left_out.iter().map(|l| l.detail.as_str()).collect();
        assert!(
            said.iter()
                .any(|d| d.contains("app.bad") && d.contains("twice")),
            "{said:?}"
        );
        assert!(
            said.iter()
                .any(|d| d.contains("app.over_bad") && d.contains("is left out")),
            "{said:?}"
        );
        assert!(said.iter().any(|d| d.contains("app.unnamed")), "{said:?}");
        assert!(
            left_out
                .iter()
                .all(|l| matches!(l.target, LimitationTarget::Search(_)))
        );
    }

    #[test]
    fn a_row_that_cannot_be_read_is_an_error_and_not_an_empty_set() {
        assert!(decode(&[json!({"kind": "config"})]).is_err());
        assert!(decode(&[row("nonsense", "x", ("a", "b"), "", json!([]))]).is_err());
        assert_eq!(decode(&[]).unwrap().0.len(), 0);
    }

    fn found(name: &SearchRef, object: &SearchObject) -> Vec<String> {
        validate(name, object, &Schema::default())
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// The built-in parser and templates, the token types of the parser, and each
    /// dictionary once per mapping are held; each other shape is refused by name.
    #[test]
    fn a_declaration_is_checked_for_parser_template_alias_and_repeats() {
        let config = SearchRef::new(SearchKind::Config, t("app.docs"));
        let dictionary = SearchRef::new(SearchKind::Dictionary, t("app.d"));
        let ok = SearchObject::Config {
            parser: t("pg_catalog.\"default\""),
            mappings: [("asciiword".into(), vec![t("app.d"), t("pg_catalog.simple")])].into(),
        };
        assert!(found(&config, &ok).is_empty());
        for template in TEMPLATES {
            let object = SearchObject::Dictionary {
                template: t(&format!("pg_catalog.{template}")),
                options: Default::default(),
            };
            assert!(found(&dictionary, &object).is_empty(), "{template}");
        }
        let refused = |object: &SearchObject, name: &SearchRef, expected: &str| {
            let said = found(name, object).join("\n");
            assert!(said.contains(expected), "{expected}: {said}");
        };
        refused(
            &SearchObject::Config {
                parser: t("ext.myparser"),
                mappings: Default::default(),
            },
            &config,
            "built-in one",
        );
        refused(
            &SearchObject::Config {
                parser: t("pg_catalog.\"default\""),
                mappings: [("wurd".into(), vec![t("pg_catalog.simple")])].into(),
            },
            &config,
            "not a token type",
        );
        refused(
            &SearchObject::Config {
                parser: t("pg_catalog.\"default\""),
                mappings: [("word".into(), vec![t("app.d"), t("app.d")])].into(),
            },
            &config,
            "twice",
        );
        refused(
            &SearchObject::Dictionary {
                template: t("ext.mytemplate"),
                options: Default::default(),
            },
            &dictionary,
            "built-in ones",
        );
        // A parser or template of the right name in the wrong schema is another object.
        refused(
            &SearchObject::Dictionary {
                template: t("app.simple"),
                options: Default::default(),
            },
            &dictionary,
            "built-in ones",
        );
    }

    /// The census and the pull ask one SQL text, so what the pull holds the census does not
    /// name: the held test of a dictionary is inside the configuration's.
    /// What the held test rests on, in the SQL itself: the boundary of the engine's own
    /// objects, the package's `e` dependency on the object and on a mapped dictionary, and
    /// the built-in parser and template by schema. The live `the_pull_reads_...` test proves
    /// each against the engine; this keeps a rewrite from dropping one unseen.
    #[test]
    fn the_held_test_names_the_system_boundary_the_package_edge_and_the_builtin_schema() {
        for held in [config_held!("c"), dictionary_held!("d")] {
            assert!(held.contains(".oid >= 16384"), "{held}");
            assert!(held.contains("deptype = 'e'"), "{held}");
            assert!(held.contains("regnamespace"), "{held}");
        }
        // A configuration also asks it of every dictionary it maps.
        assert!(config_held!("c").matches(".oid >= 16384").count() >= 2);
        assert!(config_held!("c").matches("deptype = 'e'").count() >= 2);
        // Negative: the pull's own query applies them to both kinds.
        let query = query();
        assert_eq!(
            query.matches(".oid >= 16384").count(),
            config_held!("c").matches(".oid >= 16384").count()
                + dictionary_held!("d").matches(".oid >= 16384").count()
        );
    }

    #[test]
    fn the_pull_and_the_census_share_one_held_test() {
        let query = query();
        assert!(query.contains(dictionary_held!("d")) && query.contains(config_held!("c")));
        assert!(config_held!("c").contains(dictionary_held!("md")));
        assert!(query.contains("ts_token_type(c.cfgparser)"));
    }
}
