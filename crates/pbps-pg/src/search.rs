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
use pbps_model::{Schema, SearchKind, SearchName, SearchObject, SearchRef, TypeRef};

/// The one definition of the engine's own parser and templates this model holds
/// (measured identical on 16 and 18, `measure` test). The pull's SQL, the census and
/// `validate` all read these literals, so a name is held by all three or by none: a
/// namespace test alone held a `pg_catalog` object that a declaration of it then refused.
macro_rules! builtin_parser {
    () => {
        "default"
    };
}
macro_rules! builtin_templates {
    () => {
        "simple,synonym,thesaurus,ispell,snowball"
    };
}
pub(crate) use {builtin_parser, builtin_templates};

/// The built-in parser, which this model holds alone.
pub const PARSER: &str = builtin_parser!();

/// The built-in templates.
pub fn templates() -> Vec<&'static str> {
    builtin_templates!().split(',').collect()
}
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
            ".dicttemplate AND t.tmplnamespace = 'pg_catalog'::pg_catalog.regnamespace \
                AND t.tmplname = ANY (pg_catalog.string_to_array('",
            $crate::search::builtin_templates!(),
            "', ','))))"
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
            ".cfgparser AND p.prsnamespace = 'pg_catalog'::pg_catalog.regnamespace \
                AND p.prsname = '",
            $crate::search::builtin_parser!(),
            "') \
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
    let target = SearchName::new(schema, name)
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
        let named =
            |schema: &str, name: &str| SearchName::new(schema, name).map_err(|e| e.to_string());
        match text(row, "kind")?.as_str() {
            "dictionary" => {
                let kind = SearchKind::Dictionary;
                let built = named(&schema, &name).and_then(|n| {
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
                        if let Ok(n) = named(&schema, &name) {
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
                SearchName::new(&schema, &name).map_err(|e| e.to_string())?,
            );
            let parser = TypeRef::new(&ref_schema, &ref_name).map_err(|e| e.to_string())?;
            let mut mappings: BTreeMap<String, Vec<SearchName>> = BTreeMap::new();
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
                let dictionary = SearchName::new(
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

/// `"schema"."name"`.
fn qualified(schema: &str, name: &str) -> Result<String, DialectError> {
    Ok(format!("{}.{}", crate::quote(schema)?, crate::quote(name)?))
}

/// The statements that create `definition` under `name`, qualified throughout so that no
/// search path decides what they mean. A dictionary is one `CREATE`; a configuration is its
/// `CREATE` and one `ADD MAPPING` per group of token types that share their dictionaries, so
/// the catalog ends with the declared rows in the engine's own order (measured: the order of
/// the groups does not matter, the order inside one does).
pub(crate) fn create(
    uid: &pbps_model::Uid,
    name: &SearchRef,
    definition: &SearchObject,
) -> Result<Vec<pbps_dialect::Statement>, DialectError> {
    let target = qualified(name.name.schema(), name.name.name())?;
    let statement = |sql: String| pbps_dialect::Statement::new(sql);
    let mut statements = match definition {
        SearchObject::Dictionary { template, options } => {
            let mut items = vec![format!(
                "TEMPLATE = {}",
                qualified(template.schema(), template.name())?
            )];
            for (key, value) in options {
                items.push(format!(
                    "{} = {}",
                    crate::quote(key)?,
                    crate::emit::literal(value)
                ));
            }
            vec![statement(format!(
                "CREATE TEXT SEARCH DICTIONARY {target} ({});",
                items.join(", ")
            ))]
        }
        SearchObject::Config { parser, mappings } => {
            let mut out = vec![statement(format!(
                "CREATE TEXT SEARCH CONFIGURATION {target} (PARSER = {});",
                qualified(parser.schema(), parser.name())?
            ))];
            let mut groups: Vec<(&Vec<SearchName>, Vec<&String>)> = Vec::new();
            for (alias, dictionaries) in mappings {
                match groups.iter_mut().find(|(used, _)| *used == dictionaries) {
                    Some((_, aliases)) => aliases.push(alias),
                    None => groups.push((dictionaries, vec![alias])),
                }
            }
            for (dictionaries, aliases) in groups {
                let tokens = aliases
                    .iter()
                    .map(|alias| crate::quote(alias))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(", ");
                let with = dictionaries
                    .iter()
                    .map(|d| qualified(d.schema(), d.name()))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(", ");
                out.push(statement(format!(
                    "ALTER TEXT SEARCH CONFIGURATION {target} ADD MAPPING FOR {tokens} WITH {with};"
                )));
            }
            out
        }
    };
    // The first statement is the one that brings the object into being.
    if let Some(first) = statements.first_mut() {
        first.search = Some(pbps_dialect::SearchStatement {
            uid: uid.clone(),
            name: name.clone(),
            created: true,
        });
    }
    Ok(statements)
}

/// `DROP TEXT SEARCH ... RESTRICT`: the engine refuses it while anything depends on the
/// object, and no `IF EXISTS` hides one the target lost (DEC-2728.2).
pub(crate) fn drop(
    uid: &pbps_model::Uid,
    name: &SearchRef,
) -> Result<Vec<pbps_dialect::Statement>, DialectError> {
    let word = match name.kind {
        SearchKind::Config => "CONFIGURATION",
        SearchKind::Dictionary => "DICTIONARY",
    };
    let mut statement = pbps_dialect::Statement::new(format!(
        "DROP TEXT SEARCH {word} {} RESTRICT;",
        qualified(name.name.schema(), name.name.name())?
    ));
    statement.search = Some(pbps_dialect::SearchStatement {
        uid: uid.clone(),
        name: name.clone(),
        created: false,
    });
    Ok(vec![statement])
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
                if dictionaries.is_empty() {
                    found.push(invalid(format!(
                        "{} maps `{}` to no dictionary; leave the token type out instead",
                        name.shown(),
                        pbps_model::escape::Escaped(alias)
                    )));
                }
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
            if !held(template, &templates()) {
                found.push(invalid(format!(
                    "{} names the template `{}`, which is created by C functions or by an \
                     extension; this tool holds only the built-in ones ({})",
                    name.shown(),
                    template.shown(),
                    templates().join(", ")
                )));
            }
            // The only free text these objects emit: PostgreSQL text cannot hold a NUL, so
            // the statement would reach the engine and fail there, after earlier steps.
            if let Some((key, _)) = options
                .iter()
                .find(|(k, v)| k.contains('\0') || v.contains('\0'))
            {
                found.push(invalid(format!(
                    "{} has an option ({}) with a NUL byte, which PostgreSQL text cannot hold",
                    name.shown(),
                    pbps_model::escape::Escaped(key)
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

    fn n(text: &str) -> SearchName {
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
        let config = |name: &str| SearchRef::new(SearchKind::Config, n(&format!("app.{name}")));
        let dictionary =
            |name: &str| SearchRef::new(SearchKind::Dictionary, n(&format!("app.{name}")));
        assert_eq!(
            held[&config("docs")],
            SearchObject::Config {
                parser: t("pg_catalog.\"default\""),
                mappings: [
                    (
                        "asciiword".into(),
                        vec![n("app.good"), n("pg_catalog.simple")]
                    ),
                    ("email".into(), vec![n("pg_catalog.simple")]),
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
        let config = SearchRef::new(SearchKind::Config, n("app.docs"));
        let dictionary = SearchRef::new(SearchKind::Dictionary, n("app.d"));
        let ok = SearchObject::Config {
            parser: t("pg_catalog.\"default\""),
            mappings: [("asciiword".into(), vec![n("app.d"), n("pg_catalog.simple")])].into(),
        };
        assert!(found(&config, &ok).is_empty());
        for template in templates() {
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
                mappings: [("wurd".into(), vec![n("pg_catalog.simple")])].into(),
            },
            &config,
            "not a token type",
        );
        refused(
            &SearchObject::Config {
                parser: t("pg_catalog.\"default\""),
                mappings: [("word".into(), vec![n("app.d"), n("app.d")])].into(),
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

    fn emitted(change: &pbps_model::Change) -> Vec<pbps_dialect::Statement> {
        crate::emit::emit(&crate::Postgres::new(), change, Default::default()).unwrap()
    }

    /// A name with a leading underscore is read from the catalog and written back quoted,
    /// not left out of the pull for a type-name rule that does not apply (ready round 2).
    #[test]
    fn a_name_with_a_leading_underscore_is_read_and_emitted() {
        let rows = vec![
            row(
                "dictionary",
                "_words",
                ("pg_catalog", "simple"),
                "",
                json!([]),
            ),
            row(
                "config",
                "_docs",
                ("pg_catalog", "default"),
                "",
                json!([["word", "app", "_words"]]),
            ),
        ];
        let (held, left_out) = decode(&rows).unwrap();
        assert!(left_out.is_empty(), "{left_out:?}");
        assert_eq!(held.len(), 2, "{held:?}");
        let object = held[&"config app._docs".parse().unwrap()].clone();
        let sql: Vec<_> = emitted(&create_change("config app._docs", object))
            .into_iter()
            .map(|s| s.sql)
            .collect();
        assert!(sql[0].contains("\"app\".\"_docs\""), "{sql:?}");
        assert!(sql[1].contains("\"app\".\"_words\""), "{sql:?}");
    }

    fn create_change(name: &str, definition: SearchObject) -> pbps_model::Change {
        pbps_model::Change::CreateSearch {
            uid: "s_aaaaaa".parse().unwrap(),
            name: name.parse().unwrap(),
            definition: Box::new(definition),
            unresolved: Vec::new(),
        }
    }

    /// A dictionary is one `CREATE` that names its template and writes each option as a
    /// quoted value; a configuration is its `CREATE` and one `ADD MAPPING` per group of token
    /// types that share their dictionaries, every name qualified so no search path decides
    /// anything. The first statement carries the identity a staged apply records.
    #[test]
    fn a_dictionary_and_a_configuration_emit_qualified_statements_and_mapping_groups() {
        let dictionary = create_change(
            "dictionary app.words",
            SearchObject::Dictionary {
                template: t("pg_catalog.snowball"),
                options: [
                    ("language".to_owned(), "english".to_owned()),
                    ("stopwords".to_owned(), "it's".to_owned()),
                ]
                .into(),
            },
        );
        let sql = emitted(&dictionary);
        assert_eq!(sql.len(), 1);
        assert_eq!(
            sql[0].sql,
            "CREATE TEXT SEARCH DICTIONARY \"app\".\"words\" (TEMPLATE = \"pg_catalog\".\"snowball\", \"language\" = 'english', \"stopwords\" = 'it''s');"
        );
        let carried = sql[0].search.as_ref().unwrap();
        assert!(carried.created && carried.name.raw().to_string() == "dictionary app.words");
        let config = create_change(
            "config app.docs",
            SearchObject::Config {
                parser: t("pg_catalog.\"default\""),
                mappings: [
                    (
                        "asciiword".to_owned(),
                        vec![n("app.words"), n("pg_catalog.simple")],
                    ),
                    (
                        "word".to_owned(),
                        vec![n("app.words"), n("pg_catalog.simple")],
                    ),
                    ("email".to_owned(), vec![n("pg_catalog.simple")]),
                ]
                .into(),
            },
        );
        let sql: Vec<_> = emitted(&config).into_iter().map(|s| s.sql).collect();
        assert_eq!(
            sql,
            [
                "CREATE TEXT SEARCH CONFIGURATION \"app\".\"docs\" (PARSER = \"pg_catalog\".\"default\");",
                "ALTER TEXT SEARCH CONFIGURATION \"app\".\"docs\" ADD MAPPING FOR \"asciiword\", \"word\" WITH \"app\".\"words\", \"pg_catalog\".\"simple\";",
                "ALTER TEXT SEARCH CONFIGURATION \"app\".\"docs\" ADD MAPPING FOR \"email\" WITH \"pg_catalog\".\"simple\";",
            ]
        );
        // A configuration with no mapping is only its `CREATE`.
        let bare = create_change(
            "config app.bare",
            SearchObject::Config {
                parser: t("pg_catalog.\"default\""),
                mappings: Default::default(),
            },
        );
        assert_eq!(emitted(&bare).len(), 1);
    }

    /// A drop is `RESTRICT` with no `IF EXISTS`: the engine refuses it while anything names the
    /// object, and a lost object is an error and not a silent success (DEC-2728.2). The
    /// statement says it removes the object.
    #[test]
    fn a_drop_is_restrict_and_never_if_exists() {
        for (name, word) in [
            ("config app.docs", "CONFIGURATION"),
            ("dictionary app.words", "DICTIONARY"),
        ] {
            let object = if word == "DICTIONARY" {
                SearchObject::Dictionary {
                    template: t("pg_catalog.simple"),
                    options: Default::default(),
                }
            } else {
                SearchObject::Config {
                    parser: t("pg_catalog.\"default\""),
                    mappings: Default::default(),
                }
            };
            let sql = emitted(&pbps_model::Change::DropSearch {
                uid: "s_aaaaaa".parse().unwrap(),
                name: name.parse().unwrap(),
                definition: Box::new(object),
            });
            assert_eq!(sql.len(), 1);
            let expected = format!(
                "DROP TEXT SEARCH {word} \"app\".\"{}\" RESTRICT;",
                name.split('.').nth(1).unwrap()
            );
            assert_eq!(sql[0].sql, expected);
            assert!(!sql[0].search.as_ref().unwrap().created);
        }
    }

    /// Validation refuses a token type that maps no dictionary, which the engine cannot
    /// express and a pull never produces: it would be a difference no plan could close.
    #[test]
    fn a_token_mapped_to_no_dictionary_is_invalid() {
        let name: SearchRef = "config app.docs".parse().unwrap();
        let object = SearchObject::Config {
            parser: t("pg_catalog.\"default\""),
            mappings: [("word".to_owned(), vec![])].into(),
        };
        let found = validate(&name, &object, &Schema::default());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].to_string().contains("no dictionary"));
        let fine = SearchObject::Config {
            parser: t("pg_catalog.\"default\""),
            mappings: [("word".to_owned(), vec![n("pg_catalog.simple")])].into(),
        };
        assert!(validate(&name, &fine, &Schema::default()).is_empty());
    }

    /// A NUL in an option name or value is refused at validation, by name: PostgreSQL text
    /// cannot hold one, so the statement would otherwise fail at execution. Negative: the
    /// same options without it, and an escaped backslash, are held.
    #[test]
    fn a_nul_in_a_dictionary_option_is_refused_by_name() {
        let name: SearchRef = "dictionary app.d".parse().unwrap();
        let with = |key: &str, value: &str| SearchObject::Dictionary {
            template: t("pg_catalog.synonym"),
            options: [(key.to_owned(), value.to_owned())].into(),
        };
        for object in [with("synonyms", "a\0b"), with("a\0b", "x")] {
            let said = found(&name, &object);
            assert_eq!(said.len(), 1, "{said:?}");
            assert!(
                said[0].contains("NUL byte") && said[0].contains("app.d"),
                "{said:?}"
            );
        }
        assert!(found(&name, &with("synonyms", "a\\0b")).is_empty());
        assert!(found(&name, &with("synonyms", "ab")).is_empty());
    }

    /// The pull holds a parser and a template by the same test `validate` applies: the SQL
    /// names the validator's list and no other, not the namespace alone. A `pg_catalog`
    /// parser or template outside the list is neither pulled nor accepted in a declaration,
    /// so the pull cannot write a file that validation then refuses. Negative: names that
    /// share the namespace but are not built in are in neither.
    #[test]
    fn the_pull_and_validate_hold_the_same_parser_and_templates() {
        let sql_list = |sql: &str| -> Vec<String> {
            let from = sql.find("string_to_array('").expect("a list") + "string_to_array('".len();
            let to = from + sql[from..].find('\'').unwrap();
            sql[from..to].split(',').map(str::to_owned).collect()
        };
        let templates_pulled = sql_list(dictionary_held!("d"));
        assert_eq!(templates_pulled, templates());
        assert!(config_held!("c").contains(&format!("p.prsname = '{PARSER}'")));
        let dictionary: SearchRef = "dictionary app.d".parse().unwrap();
        let config: SearchRef = "config app.c".parse().unwrap();
        let over = |template: &str| SearchObject::Dictionary {
            template: t(&format!("pg_catalog.{template}")),
            options: Default::default(),
        };
        for template in &templates_pulled {
            assert!(found(&dictionary, &over(template)).is_empty(), "{template}");
        }
        for custom in ["mytmpl", "simple2", "unaccent"] {
            assert!(!templates_pulled.iter().any(|n| n == custom));
            assert!(!found(&dictionary, &over(custom)).is_empty(), "{custom}");
        }
        let parser = |name: &str| SearchObject::Config {
            parser: t(&format!("pg_catalog.\"{name}\"")),
            mappings: Default::default(),
        };
        assert!(found(&config, &parser(PARSER)).is_empty());
        assert!(!found(&config, &parser("myparser")).is_empty());
    }
}
