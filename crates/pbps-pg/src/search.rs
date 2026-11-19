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
                    let object = SearchObject::Dictionary {
                        template: reference(&ref_schema, &ref_name)?,
                        options: parse_options(&text(row, "options").map_err(|e| e.to_string())?)?,
                    };
                    // The engine keeps the name as it was written and resolves it by the search
                    // path of whoever uses it, so there is no schema to write here that is true:
                    // the declaration file refuses it (`pbps-load`), and so it is left out.
                    if let Some(value) = object.unqualified_sub_dictionary() {
                        return Err(format!(
                            "its option `dictionary` names `{value}` without a schema; \
                             `ALTER TEXT SEARCH DICTIONARY` it to `schema.name` and it can be held"
                        ));
                    }
                    Ok((SearchRef::new(kind, n), object))
                });
                match built {
                    Ok((key, object)) => {
                        held.insert(key, object);
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
            out.extend(mapping_statements(&target, "ADD", mappings.iter())?);
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

/// One `ALTER TEXT SEARCH CONFIGURATION ... <verb> MAPPING` per group of token types that
/// share their dictionaries, so the catalog ends with the declared rows in the engine's own
/// order (measured: the order of the groups does not matter, the order inside one does).
fn mapping_statements<'a>(
    target: &str,
    verb: &str,
    mappings: impl Iterator<Item = (&'a String, &'a Vec<SearchName>)>,
) -> Result<Vec<pbps_dialect::Statement>, DialectError> {
    let mut groups: Vec<(&Vec<SearchName>, Vec<&String>)> = Vec::new();
    for (alias, dictionaries) in mappings {
        match groups.iter_mut().find(|(used, _)| *used == dictionaries) {
            Some((_, aliases)) => aliases.push(alias),
            None => groups.push((dictionaries, vec![alias])),
        }
    }
    let mut out = Vec::new();
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
        out.push(pbps_dialect::Statement::new(format!(
            "ALTER TEXT SEARCH CONFIGURATION {target} {verb} MAPPING FOR {tokens} WITH {with};"
        )));
    }
    Ok(out)
}

/// The statements that bring `before` to `after` in place, qualified throughout
/// (DEC-2728.3). A configuration adds the token types it newly maps, replaces the dictionary
/// list of the ones it changes and drops the ones it gives up: `ALTER MAPPING` on a token
/// type that is not mapped does nothing and `ADD MAPPING` on one that is fails (measured on
/// 16 and 18), so each token type gets the one verb that does what it says. A dictionary is
/// one `ALTER` that writes each option that is new or changed and names, without a value,
/// each one given up, which is how the engine removes one. The object keeps its identity, so
/// no statement carries one.
pub(crate) fn alter(
    name: &SearchRef,
    before: &SearchObject,
    after: &SearchObject,
) -> Result<Vec<pbps_dialect::Statement>, DialectError> {
    let target = qualified(name.name.schema(), name.name.name())?;
    match (before, after) {
        (
            SearchObject::Config { mappings: was, .. },
            SearchObject::Config { mappings: now, .. },
        ) => {
            let changed =
                |(alias, list): &(&String, &Vec<SearchName>)| was.get(*alias) != Some(*list);
            let mut out = Vec::new();
            let gone: Vec<_> = was
                .keys()
                .filter(|alias| !now.contains_key(*alias))
                .collect();
            if !gone.is_empty() {
                let tokens = gone
                    .iter()
                    .map(|alias| crate::quote(alias))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(", ");
                out.push(pbps_dialect::Statement::new(format!(
                    "ALTER TEXT SEARCH CONFIGURATION {target} DROP MAPPING FOR {tokens};"
                )));
            }
            let (standing, fresh): (Vec<_>, Vec<_>) = now
                .iter()
                .filter(|entry| changed(entry))
                .partition(|(alias, _)| was.contains_key(*alias));
            out.extend(mapping_statements(&target, "ALTER", standing.into_iter())?);
            out.extend(mapping_statements(&target, "ADD", fresh.into_iter())?);
            Ok(out)
        }
        (
            SearchObject::Dictionary { options: was, .. },
            SearchObject::Dictionary { options: now, .. },
        ) => {
            let mut items = Vec::new();
            for (key, value) in now.iter().filter(|(k, v)| was.get(*k) != Some(*v)) {
                items.push(format!(
                    "{} = {}",
                    crate::quote(key)?,
                    crate::emit::literal(value)
                ));
            }
            for key in was.keys().filter(|key| !now.contains_key(*key)) {
                items.push(crate::quote(key)?);
            }
            if items.is_empty() {
                return Ok(Vec::new());
            }
            Ok(vec![pbps_dialect::Statement::new(format!(
                "ALTER TEXT SEARCH DICTIONARY {target} ({});",
                items.join(", ")
            ))])
        }
        _ => Err(invalid(format!(
            "{} cannot be altered into another kind of object",
            name.shown()
        ))),
    }
}

/// What stands on one object and keeps what the old behaviour computed: an index, a
/// constraint (a check over the object), a stored generated column and a materialized view.
/// A view, a plain default and a function are evaluated on use and are not here. A
/// dictionary also answers for the configurations that map it (`pg_depend` holds that edge),
/// whose dependents stale with it. The walk follows what only reads at run time (a view, a
/// function) and stops at what stores: a materialized view over a view over the object is as
/// stale as one over the object itself, and a path that reaches no stored dependent makes
/// none (the `UNION` drops a repeated node, so a cycle ends). Measured on 16 and 18 (DEC-2728.3): the catalog lists an
/// expression index, a generated column's default, a check, a view and a materialized view
/// over `'app.docs'::regconfig` as dependents of the configuration, and not a thesaurus's
/// sub-dictionary, which the model reads from the options instead. Each row is a stored
/// dependent (`what`) or a function or view the walk crossed (`via`, the identity [`CALLS`]
/// uses): a body that names the second kind reaches the object, though no edge says so.
const STALE: &str = "WITH RECURSIVE subject(cls, oid) AS (
        SELECT 'pg_catalog.pg_ts_config'::pg_catalog.regclass, c.oid
          FROM pg_catalog.pg_ts_config c
          JOIN pg_catalog.pg_namespace n ON n.oid = c.cfgnamespace
         WHERE $1 = 'config' AND n.nspname = $2 AND c.cfgname = $3
        UNION ALL
        SELECT 'pg_catalog.pg_ts_dict'::pg_catalog.regclass, d.oid
          FROM pg_catalog.pg_ts_dict d
          JOIN pg_catalog.pg_namespace n ON n.oid = d.dictnamespace
         WHERE $1 = 'dictionary' AND n.nspname = $2 AND d.dictname = $3
     ), reach(cls, oid) AS (
        SELECT cls, oid FROM subject
        UNION
        SELECT n.cls, n.oid
          FROM reach r
          JOIN pg_catalog.pg_depend x ON x.refclassid = r.cls AND x.refobjid = r.oid
         CROSS JOIN LATERAL (
              SELECT 'pg_catalog.pg_ts_config'::pg_catalog.regclass, x.objid
               WHERE x.classid = 'pg_catalog.pg_ts_config'::pg_catalog.regclass
                 AND r.cls = 'pg_catalog.pg_ts_dict'::pg_catalog.regclass
              UNION ALL
              SELECT 'pg_catalog.pg_proc'::pg_catalog.regclass, x.objid
               WHERE x.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass
              UNION ALL
              SELECT 'pg_catalog.pg_class'::pg_catalog.regclass, w.ev_class
                FROM pg_catalog.pg_rewrite w
                JOIN pg_catalog.pg_class k ON k.oid = w.ev_class
               WHERE x.classid = 'pg_catalog.pg_rewrite'::pg_catalog.regclass
                 AND w.oid = x.objid AND k.relkind = 'v'
         ) n(cls, oid)
     )
     SELECT what, via FROM (
     SELECT DISTINCT CASE WHEN x.classid = 'pg_catalog.pg_rewrite'::pg_catalog.regclass
                 THEN 'materialized view ' || (SELECT i.identity FROM pg_catalog.pg_identify_object(
                        'pg_catalog.pg_class'::pg_catalog.regclass,
                        (SELECT w.ev_class FROM pg_catalog.pg_rewrite w WHERE w.oid = x.objid), 0) i)
                 ELSE (SELECT i.type || ' ' || i.identity
                         FROM pg_catalog.pg_identify_object(x.classid, x.objid, x.objsubid) i)
            END AS what, NULL::text AS via
       FROM reach r
       JOIN pg_catalog.pg_depend x ON x.refclassid = r.cls AND x.refobjid = r.oid
      WHERE (x.classid = 'pg_catalog.pg_class'::pg_catalog.regclass
             AND EXISTS (SELECT 1 FROM pg_catalog.pg_class k
                          WHERE k.oid = x.objid AND k.relkind IN ('i', 'I')))
         OR x.classid = 'pg_catalog.pg_constraint'::pg_catalog.regclass
         OR (x.classid = 'pg_catalog.pg_attrdef'::pg_catalog.regclass
             AND EXISTS (SELECT 1 FROM pg_catalog.pg_attrdef a
                           JOIN pg_catalog.pg_attribute t
                             ON t.attrelid = a.adrelid AND t.attnum = a.adnum
                          WHERE a.oid = x.objid AND t.attgenerated = 's'))
         OR (x.classid = 'pg_catalog.pg_rewrite'::pg_catalog.regclass
             AND EXISTS (SELECT 1 FROM pg_catalog.pg_rewrite w
                           JOIN pg_catalog.pg_class k ON k.oid = w.ev_class
                          WHERE w.oid = x.objid AND k.relkind = 'm'))
     UNION
     SELECT NULL::text, i.identity
       FROM reach r
      CROSS JOIN LATERAL pg_catalog.pg_identify_object(r.cls, r.oid, 0) i
      WHERE r.cls IN ('pg_catalog.pg_proc'::pg_catalog.regclass,
                      'pg_catalog.pg_class'::pg_catalog.regclass)
     ) s
      ORDER BY 1, 2";

/// The user functions, views and materialized views the catalog says something reaches, one
/// row per `(root, node)`: the roots are every stored dependent (an index, a constraint, a
/// stored generated column, a materialized view) and every user function, view and
/// materialized view, and each row names a node the root reaches through `pg_depend`, through a
/// view's rule and through a function whose SQL-standard body (`BEGIN ATOMIC`) calls another. A
/// node reaches itself. Measured on 16.15 and 18.6 (DEC-2728.3): the engine records no edge
/// from a `LANGUAGE sql` string body or a PL/pgSQL body to anything it names, a configuration,
/// a function or a view, so [`STALE`] cannot see it and neither can this walk past such a body;
/// [`named_in_bodies`] adds those hops. The text of a function in `pg_catalog` or
/// `information_schema`, one with a SQL-standard body and one written in `internal` or `c` is
/// not read (it has none, or no text of the kind), and a view has none to read.
const CALLS: &str = "WITH RECURSIVE start(stored, cls, oid) AS (
        SELECT true, 'pg_catalog.pg_class'::pg_catalog.regclass, c.oid
          FROM pg_catalog.pg_class c
          JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relkind IN ('i', 'I') AND n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
        UNION ALL
        SELECT true, 'pg_catalog.pg_constraint'::pg_catalog.regclass, k.oid
          FROM pg_catalog.pg_constraint k
          JOIN pg_catalog.pg_namespace n ON n.oid = k.connamespace
         WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
        UNION ALL
        SELECT true, 'pg_catalog.pg_attrdef'::pg_catalog.regclass, a.oid
          FROM pg_catalog.pg_attrdef a
          JOIN pg_catalog.pg_attribute t ON t.attrelid = a.adrelid AND t.attnum = a.adnum
          JOIN pg_catalog.pg_class c ON c.oid = a.adrelid
          JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE t.attgenerated = 's' AND n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
        UNION ALL
        SELECT k.relkind = 'm', 'pg_catalog.pg_rewrite'::pg_catalog.regclass, w.oid
          FROM pg_catalog.pg_rewrite w
          JOIN pg_catalog.pg_class k ON k.oid = w.ev_class
          JOIN pg_catalog.pg_namespace n ON n.oid = k.relnamespace
         WHERE k.relkind IN ('v', 'm') AND n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
        UNION ALL
        SELECT false, 'pg_catalog.pg_rewrite'::pg_catalog.regclass, w.oid
          FROM pg_catalog.pg_rewrite w
          JOIN pg_catalog.pg_class k ON k.oid = w.ev_class
          JOIN pg_catalog.pg_namespace n ON n.oid = k.relnamespace
         WHERE k.relkind = 'm' AND n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
        UNION ALL
        SELECT false, 'pg_catalog.pg_proc'::pg_catalog.regclass, p.oid
          FROM pg_catalog.pg_proc p
          JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
         WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
     ), walk(stored, root_cls, root_oid, cls, oid) AS (
        SELECT stored, cls, oid, cls, oid FROM start
        UNION
        SELECT w.stored, w.root_cls, w.root_oid, n.cls, n.oid
          FROM walk w
          JOIN pg_catalog.pg_depend x ON x.classid = w.cls AND x.objid = w.oid
         CROSS JOIN LATERAL (
              SELECT 'pg_catalog.pg_proc'::pg_catalog.regclass, x.refobjid
               WHERE x.refclassid = 'pg_catalog.pg_proc'::pg_catalog.regclass
              UNION ALL
              SELECT 'pg_catalog.pg_rewrite'::pg_catalog.regclass, r.oid
                FROM pg_catalog.pg_rewrite r
                JOIN pg_catalog.pg_class k ON k.oid = r.ev_class
               WHERE x.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass
                 AND r.ev_class = x.refobjid AND k.relkind = 'v'
         ) n(cls, oid)
     )
     SELECT DISTINCT
            CASE WHEN w.stored AND w.root_cls = 'pg_catalog.pg_rewrite'::pg_catalog.regclass
                 THEN 'materialized view ' || (SELECT i.identity FROM pg_catalog.pg_identify_object(
                        'pg_catalog.pg_class'::pg_catalog.regclass,
                        (SELECT r.ev_class FROM pg_catalog.pg_rewrite r WHERE r.oid = w.root_oid), 0) i)
                 WHEN w.stored
                 THEN (SELECT i.type || ' ' || i.identity
                         FROM pg_catalog.pg_identify_object(w.root_cls, w.root_oid, 0) i)
            END AS what,
            CASE WHEN NOT w.stored
                 THEN (SELECT i.identity FROM pg_catalog.pg_identify_object(
                        CASE WHEN w.root_cls = 'pg_catalog.pg_rewrite'::pg_catalog.regclass
                             THEN 'pg_catalog.pg_class'::pg_catalog.regclass ELSE w.root_cls END,
                        CASE WHEN w.root_cls = 'pg_catalog.pg_rewrite'::pg_catalog.regclass
                             THEN (SELECT r.ev_class FROM pg_catalog.pg_rewrite r WHERE r.oid = w.root_oid)
                             ELSE w.root_oid END, 0) i)
            END AS caller,
            t.via, t.kind, t.name, t.src
       FROM walk w
      CROSS JOIN LATERAL (
            SELECT i.identity, 'function', p.proname::text,
                   CASE WHEN p.prosqlbody IS NULL AND l.lanname NOT IN ('internal', 'c')
                        THEN p.prosrc ELSE '' END
              FROM pg_catalog.pg_proc p
              JOIN pg_catalog.pg_namespace pn ON pn.oid = p.pronamespace
              JOIN pg_catalog.pg_language l ON l.oid = p.prolang
             CROSS JOIN LATERAL pg_catalog.pg_identify_object(w.cls, w.oid, 0) i
             WHERE w.cls = 'pg_catalog.pg_proc'::pg_catalog.regclass AND p.oid = w.oid
               AND pn.nspname NOT IN ('pg_catalog', 'information_schema')
            UNION ALL
            SELECT i.identity,
                   CASE k.relkind WHEN 'm' THEN 'materialized view' ELSE 'view' END,
                   k.relname::text, ''
              FROM pg_catalog.pg_rewrite r
              JOIN pg_catalog.pg_class k ON k.oid = r.ev_class
              JOIN pg_catalog.pg_namespace kn ON kn.oid = k.relnamespace
             CROSS JOIN LATERAL pg_catalog.pg_identify_object(
                    'pg_catalog.pg_class'::pg_catalog.regclass, k.oid, 0) i
             WHERE w.cls = 'pg_catalog.pg_rewrite'::pg_catalog.regclass AND r.oid = w.oid
               AND k.relkind IN ('v', 'm') AND kn.nspname NOT IN ('pg_catalog', 'information_schema')
      ) t(via, kind, name, src)
      ORDER BY 1, 2, 3";

/// The configurations, of any parser and whoever declared them, that map one dictionary: the
/// names a function's text may spell to reach it (`pg_ts_config_map` holds one row per token
/// type).
const MAPPING_CONFIGS: &str = "SELECT DISTINCT c.cfgname AS name
       FROM pg_catalog.pg_ts_config_map m
       JOIN pg_catalog.pg_ts_config c ON c.oid = m.mapcfg
       JOIN pg_catalog.pg_ts_dict d ON d.oid = m.mapdict
       JOIN pg_catalog.pg_namespace n ON n.oid = d.dictnamespace
      WHERE n.nspname = $1 AND d.dictname = $2
      ORDER BY 1";

/// One row of [`CALLS`]: `node`, a function, a view or a materialized view, is reached,
/// through the catalog's edges, from a stored dependent (`what`) or from another node (`caller`).
struct Call {
    what: Option<String>,
    caller: Option<String>,
    node: String,
    kind: String,
    name: String,
    body: String,
}

/// The census lines the names in bodies add (DEC-2728.3): every stored dependent that reaches a
/// function whose body names one of `names`, or a node in `touched` (a function or view the
/// catalog says reads the object) that a body names, and that the catalog did not list
/// (`listed`). The nodes a dependent reaches are the fixpoint of two steps, from what it
/// calls in the catalog (the rows of [`CALLS`]) to what the text of each reached function
/// names in turn: a body that spells another user function, view or materialized view, of
/// any schema, reaches it, and the catalog's edges lead on from there. The engine records no
/// edge for a call or a read made inside a string body, so a wrapper of a wrapper of a
/// function that spells the object, or of a view over it, is invisible to the catalog alone. A
/// repeated node ends the walk, so mutually recursive functions end it. A name counts when the
/// body holds it as a whole identifier, or the same read inside a string literal
/// (`'app.docs'::regconfig`): a bare name folds ASCII case as the engine does, a quoted one is
/// exact, and the schema is not compared, so a "yes" is a "may" that errs toward refusing
/// (`generated::may_call`). A name built at run time is never spelled, and the catalog cannot
/// show it.
///
/// In Rust and not in a recursive query, because the matcher is Rust's: a second one in SQL
/// would read names differently, and the walk is over the nodes of one database, read once.
fn named_in_bodies(
    rows: &[Call],
    names: &BTreeSet<String>,
    touched: &BTreeSet<String>,
    listed: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut nodes: BTreeMap<&str, &Call> = BTreeMap::new();
    let mut calls: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut stored: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for row in rows {
        nodes.insert(&row.node, row);
        match (&row.what, &row.caller) {
            (Some(what), _) => stored.entry(what).or_default().push(&row.node),
            (None, Some(caller)) => calls.entry(caller).or_default().push(&row.node),
            (None, None) => {}
        }
    }
    let mut lines = BTreeSet::new();
    for (what, direct) in stored {
        let mut seen = BTreeSet::new();
        // `true` marks a node a body named: the one the line is about, not what the catalog
        // reaches from it.
        let mut todo: Vec<(&str, bool)> = direct.into_iter().map(|node| (node, false)).collect();
        while let Some((node, spelled)) = todo.pop() {
            if !seen.insert((node, spelled)) {
                continue;
            }
            todo.extend(
                calls
                    .get(node)
                    .into_iter()
                    .flatten()
                    .map(|next| (*next, false)),
            );
            let Call { kind, body, .. } = nodes[node];
            // A node the catalog links to the object already made its dependent a catalog line.
            if spelled && touched.contains(node) && !listed.contains(what) {
                lines.insert(format!(
                    "{what} (reached through {kind} {node}, which a body names)"
                ));
            }
            if body.is_empty() {
                continue;
            }
            todo.extend(
                nodes
                    .iter()
                    .filter(|(_, other)| crate::generated::may_call(body, &other.name))
                    .map(|(identity, _)| (*identity, true)),
            );
            if names
                .iter()
                .any(|name| crate::generated::may_call(body, name))
            {
                lines.insert(format!(
                    "{what} (reached through {kind} {node}, named in its body)"
                ));
            }
        }
    }
    lines
}

/// For each of `subjects`, the dependents that would go stale if it changed in place, one
/// line each, read from the target's `pg_depend` (DEC-2728.3). `standing` is what the target
/// holds: a thesaurus names its sub-dictionary in its options and the catalog records no edge
/// for it, so a dictionary also answers for the dictionaries that name it, transitively. Every
/// subject has an entry, empty when nothing stands on it: an entry that is not there was not
/// read.
pub async fn stale_dependents(
    conn: &mut pbps_db::Conn,
    standing: &BTreeMap<SearchRef, SearchObject>,
    subjects: &BTreeSet<SearchRef>,
) -> Result<BTreeMap<SearchRef, Vec<String>>, pbps_db::DbError> {
    let mut out = BTreeMap::new();
    // One read of every node a stored dependent or another node reaches; asked only when there
    // is a subject, and not per subject.
    let mut bodies = Vec::new();
    if !subjects.is_empty() {
        for row in &conn.query_with(CALLS, &[]).await? {
            let text = |column: &str| -> Result<Option<String>, pbps_db::DbError> {
                Ok(row.try_get::<&str>(column)?.map(str::to_owned))
            };
            bodies.push(Call {
                what: text("what")?,
                caller: text("caller")?,
                node: text("via")?.unwrap_or_default(),
                kind: text("kind")?.unwrap_or_default(),
                name: text("name")?.unwrap_or_default(),
                body: text("src")?.unwrap_or_default(),
            });
        }
    }
    let bodies = &bodies;
    for subject in subjects {
        let mut members = BTreeSet::from([subject.clone()]);
        while let Some(user) = standing
            .iter()
            .filter(|(_, object)| matches!(object, SearchObject::Dictionary { .. }))
            .find(|(name, object)| {
                !members.contains(*name)
                    && object.dictionaries().iter().any(|d| members.contains(d))
            })
            .map(|(name, _)| name.clone())
        {
            members.insert(user);
        }
        let mut lines = BTreeSet::new();
        // What a function's text may spell to reach the object: its own name and, for a
        // dictionary, the name of each configuration that maps it, read from the catalog
        // and not from the held model: a configuration with a parser the project does not
        // hold, or one it never declared, maps it all the same (a thesaurus's
        // sub-dictionary is a member already).
        let mut names = BTreeSet::new();
        for member in &members {
            names.insert(member.name.name().to_owned());
            if member.kind == SearchKind::Dictionary {
                let rows = conn
                    .query_with(
                        MAPPING_CONFIGS,
                        &[
                            pbps_db::Param::Str(member.name.schema()),
                            pbps_db::Param::Str(member.name.name()),
                        ],
                    )
                    .await?;
                for row in &rows {
                    names.insert(row.try_get::<&str>("name")?.unwrap_or_default().to_owned());
                }
            }
        }
        let mut listed = BTreeSet::new();
        let mut touched = BTreeSet::new();
        for member in &members {
            let kind = match member.kind {
                SearchKind::Config => "config",
                SearchKind::Dictionary => "dictionary",
            };
            let rows = conn
                .query_with(
                    STALE,
                    &[
                        pbps_db::Param::Str(kind),
                        pbps_db::Param::Str(member.name.schema()),
                        pbps_db::Param::Str(member.name.name()),
                    ],
                )
                .await?;
            for row in &rows {
                if let Some(what) = row.try_get::<&str>("what")? {
                    // A stored materialized view is a node a body can read as well.
                    if let Some(view) = what.strip_prefix("materialized view ") {
                        touched.insert(view.to_owned());
                    }
                    listed.insert(what.to_owned());
                }
                if let Some(via) = row.try_get::<&str>("via")? {
                    touched.insert(via.to_owned());
                }
            }
        }
        lines.extend(listed.iter().cloned());
        lines.extend(named_in_bodies(bodies, &names, &touched, &listed));
        out.insert(subject.clone(), lines.into_iter().collect());
    }
    Ok(out)
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
    // The pull never reads these schemas (`catalog::a_projects_schema`, the one test types,
    // tables and modules share), so the object would be created and no plan could see it
    // again: its recorded identity would go wrong.
    if !crate::catalog::a_projects_schema(name.name.schema()) {
        found.push(invalid(format!(
            "{} is declared in `{}`, which this dialect's pull never reads: `pg_catalog`, \
             `information_schema` and every schema whose name begins with `pg_` are excluded \
             from the managed set. Declare it in a schema of the project's own",
            name.shown(),
            pbps_model::escape::Escaped(name.name.schema())
        )));
    }
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
        pbps_model::Change::Search(pbps_model::SearchChange::CreateSearch {
            uid: "s_aaaaaa".parse().unwrap(),
            name: name.parse().unwrap(),
            definition: Box::new(definition),
            unresolved: Vec::new(),
        })
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

    fn alter_change(name: &str, before: SearchObject, after: SearchObject) -> pbps_model::Change {
        let (uid, name) = ("s_aaaaaa".parse().unwrap(), name.parse().unwrap());
        let (before, after) = (Box::new(before), Box::new(after));
        pbps_model::Change::Search(if matches!(after.as_ref(), SearchObject::Config { .. }) {
            pbps_model::SearchChange::AlterSearchMapping {
                uid,
                name,
                before,
                after,
            }
        } else {
            pbps_model::SearchChange::AlterSearchDictionary {
                uid,
                name,
                before,
                after,
            }
        })
    }

    fn mapped(entries: &[(&str, &[&str])]) -> SearchObject {
        SearchObject::Config {
            parser: t("pg_catalog.\"default\""),
            mappings: entries
                .iter()
                .map(|(alias, used)| ((*alias).to_owned(), used.iter().map(|d| n(d)).collect()))
                .collect(),
        }
    }

    /// A configuration is brought to its new mappings with the one verb each token type
    /// needs: `ADD` for a new one, `ALTER` for a changed list (the engine's `ALTER` on an
    /// unmapped token type does nothing, and `ADD` on a mapped one fails), `DROP` for a given
    /// up one; token types that share a list share a statement; an unchanged one is not
    /// mentioned. No statement carries an identity, since none is created or dropped.
    /// Negative: two configurations that are the same emit nothing.
    #[test]
    fn a_configuration_alter_uses_the_verb_each_token_type_needs() {
        let before = mapped(&[
            ("asciiword", &["app.a", "pg_catalog.simple"]),
            ("word", &["app.a"]),
            ("email", &["pg_catalog.simple"]),
            ("url", &["pg_catalog.simple"]),
        ]);
        let after = mapped(&[
            ("asciiword", &["pg_catalog.simple", "app.a"]),
            ("word", &["pg_catalog.simple", "app.a"]),
            ("email", &["pg_catalog.simple"]),
            ("int", &["pg_catalog.simple"]),
            ("uint", &["pg_catalog.simple"]),
        ]);
        let sql = emitted(&alter_change(
            "config app.docs",
            before.clone(),
            after.clone(),
        ));
        assert_eq!(
            sql.iter().map(|s| s.sql.as_str()).collect::<Vec<_>>(),
            [
                "ALTER TEXT SEARCH CONFIGURATION \"app\".\"docs\" DROP MAPPING FOR \"url\";",
                "ALTER TEXT SEARCH CONFIGURATION \"app\".\"docs\" ALTER MAPPING FOR \"asciiword\", \"word\" WITH \"pg_catalog\".\"simple\", \"app\".\"a\";",
                "ALTER TEXT SEARCH CONFIGURATION \"app\".\"docs\" ADD MAPPING FOR \"int\", \"uint\" WITH \"pg_catalog\".\"simple\";",
            ]
        );
        assert!(sql.iter().all(|s| s.search.is_none()));
        assert!(emitted(&alter_change("config app.docs", after.clone(), after)).is_empty());
    }

    /// A dictionary is one `ALTER` that writes each option that is new or changed with a quoted
    /// value and names, without one, each option given up (the engine's removal). An option
    /// that is the same is not written. Negative: an alteration into another kind of object is
    /// an error and not a statement.
    #[test]
    fn a_dictionary_alter_writes_what_changed_and_removes_what_went() {
        let object = |options: &[(&str, &str)]| SearchObject::Dictionary {
            template: t("pg_catalog.simple"),
            options: options
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        };
        let sql = emitted(&alter_change(
            "dictionary app.words",
            object(&[("accept", "false"), ("stopwords", "english"), ("same", "x")]),
            object(&[("stopwords", "it's"), ("same", "x"), ("synonyms", "s")]),
        ));
        assert_eq!(
            sql.iter().map(|s| s.sql.as_str()).collect::<Vec<_>>(),
            [
                "ALTER TEXT SEARCH DICTIONARY \"app\".\"words\" (\"stopwords\" = 'it''s', \"synonyms\" = 's', \"accept\");"
            ]
        );
        let error = super::alter(
            &"dictionary app.words".parse().unwrap(),
            &object(&[]),
            &mapped(&[]),
        );
        assert!(error.is_err());
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
            let sql = emitted(&pbps_model::Change::Search(
                pbps_model::SearchChange::DropSearch {
                    uid: "s_aaaaaa".parse().unwrap(),
                    name: name.parse().unwrap(),
                    definition: Box::new(object),
                },
            ));
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

    /// An object declared in a schema the pull never reads is refused by name: it would be
    /// created and then never seen, so its recorded identity would go wrong. The test is the
    /// one types, tables and modules use. Negatives: a project schema, and names that only
    /// contain or resemble the prefix (`pgx`, `my_pg_x`), are held.
    #[test]
    fn an_object_in_a_schema_the_pull_never_reads_is_refused_by_name() {
        let config = SearchObject::Config {
            parser: t("pg_catalog.\"default\""),
            mappings: Default::default(),
        };
        let dictionary = SearchObject::Dictionary {
            template: t("pg_catalog.simple"),
            options: Default::default(),
        };
        for schema in [
            "pg_catalog",
            "information_schema",
            "pg_temp",
            "pg_toast",
            "pg_foo",
        ] {
            for (kind, object) in [("config", &config), ("dictionary", &dictionary)] {
                let name: SearchRef = format!("{kind} {schema}.x").parse().unwrap();
                let said = found(&name, object);
                assert_eq!(said.len(), 1, "{schema} {kind}: {said:?}");
                assert!(
                    said[0].contains(&format!("{schema}.x")) && said[0].contains("never reads"),
                    "{said:?}"
                );
            }
        }
        for schema in ["app", "pgx", "my_pg_x", "public"] {
            for (kind, object) in [("config", &config), ("dictionary", &dictionary)] {
                let name: SearchRef = format!("{kind} {schema}.x").parse().unwrap();
                assert!(found(&name, object).is_empty(), "{schema} {kind}");
            }
        }
    }

    /// A thesaurus whose `dictionary` option is spelled without a schema is left out of the
    /// pull with its configuration, named and with the remedy, instead of held as a file the
    /// loader refuses. Negative: the qualified spelling is held, and so is the configuration
    /// over it.
    #[test]
    fn a_thesaurus_with_an_unqualified_sub_dictionary_is_left_out_with_what_maps_it() {
        let rows = vec![
            row(
                "dictionary",
                "bare",
                ("pg_catalog", "thesaurus"),
                "dictfile = 'mythes', dictionary = 'english_stem'",
                json!([]),
            ),
            row(
                "dictionary",
                "full",
                ("pg_catalog", "thesaurus"),
                "dictfile = 'mythes', dictionary = 'pg_catalog.english_stem'",
                json!([]),
            ),
            row(
                "config",
                "over_bare",
                ("pg_catalog", "default"),
                "",
                json!([["word", "app", "bare"]]),
            ),
            row(
                "config",
                "over_full",
                ("pg_catalog", "default"),
                "",
                json!([["word", "app", "full"]]),
            ),
        ];
        let (held, left_out) = decode(&rows).unwrap();
        let held_names: Vec<String> = held.keys().map(|k| k.raw().to_string()).collect();
        assert_eq!(
            held_names,
            ["config app.over_full", "dictionary app.full"],
            "{held_names:?}"
        );
        let said: Vec<&str> = left_out.iter().map(|l| l.detail.as_str()).collect();
        assert!(
            said.iter().any(|d| d.contains("app.bare")
                && d.contains("english_stem")
                && d.contains("schema.name")),
            "{said:?}"
        );
        assert!(
            said.iter()
                .any(|d| d.contains("app.over_bare") && d.contains("is left out")),
            "{said:?}"
        );
        assert_eq!(left_out.len(), 2, "{said:?}");
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

    fn call(from: (Option<&str>, Option<&str>), function: &str, body: &str) -> Call {
        Call {
            what: from.0.map(str::to_owned),
            caller: from.1.map(str::to_owned),
            node: format!("app.{function}(text)"),
            kind: "function".to_owned(),
            name: function.to_owned(),
            body: body.to_owned(),
        }
    }

    fn spelled(rows: &[Call], names: &BTreeSet<String>) -> BTreeSet<String> {
        named_in_bodies(rows, names, &BTreeSet::new(), &BTreeSet::new())
    }

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// A function's text names an object the way the engine would read it (DEC-2728.3): the
    /// bare name case-insensitively, the same inside a literal (`'app.docs'::regconfig`), and a
    /// quoted name exactly. Negatives: another name, a quoted name of another case, a name
    /// inside a comment, a name that is only part of a longer identifier, and no names at all.
    #[test]
    fn a_function_body_names_an_object_by_whole_identifier_and_exact_quoted_case() {
        let row = |body: &str| vec![call((Some("index app.ix"), None), "f", body)];
        let said = "index app.ix (reached through function app.f(text), named in its body)";
        for body in [
            "SELECT to_tsvector('app.docs', t)",
            "SELECT to_tsvector('docs'::regconfig, t)",
            "SELECT to_tsvector(app.DOCS::regconfig, t)",
            "SELECT to_tsvector(\"docs\", t)",
            "BEGIN RETURN to_tsvector('app.docs'::regconfig, t); END",
        ] {
            assert_eq!(
                spelled(&row(body), &names(&["docs"])),
                [said.to_owned()].into(),
                "{body}"
            );
        }
        for body in [
            "SELECT to_tsvector('app.other', t)",
            "SELECT to_tsvector(\"DOCS\", t)",
            "SELECT 1 -- docs",
            "SELECT /* docs */ 1",
            "SELECT to_tsvector('app.docs_v2', t)",
        ] {
            assert!(spelled(&row(body), &names(&["docs"])).is_empty(), "{body}");
        }
        // A quoted name of mixed case is matched as it was written, and a bare spelling of it
        // is not its name.
        assert!(spelled(&row("SELECT 'app.\"Docs\"'"), &names(&["docs"])).is_empty());
        assert!(!spelled(&row("SELECT 'app.\"Docs\"'"), &names(&["Docs"])).is_empty());
        assert!(spelled(&row("SELECT docs"), &names(&["Docs"])).is_empty());
        assert!(!spelled(&row("SELECT \"Docs\""), &names(&["Docs"])).is_empty());
        assert!(spelled(&row("SELECT docs"), &BTreeSet::new()).is_empty());
    }

    /// The functions a dependent reaches are the fixpoint of the catalog's edges and of the
    /// names in the text (DEC-2728.3): a wrapper of a wrapper of a function that spells the
    /// object is reached though no edge joins them, a `BEGIN ATOMIC` hop (a catalog edge)
    /// continues the walk, and a pair of functions that name each other ends it. Negatives: a
    /// chain that ends in a function naming another object, a callee spelled by a run-time
    /// built name (the documented limit), and a name that is only a longer identifier.
    #[test]
    fn the_functions_a_dependent_reaches_are_the_fixpoint_of_edges_and_called_names() {
        let stored = (Some("index app.ix"), None);
        let said = |function: &str| -> BTreeSet<String> {
            [format!(
                "index app.ix (reached through function app.{function}(text), named in its body)"
            )]
            .into()
        };
        let named = "SELECT to_tsvector('app.docs', t)";
        // A function reaches itself in the rows, as the query returns it.
        let own = |function: &str, body: &str| {
            call(
                (None, Some(&format!("app.{function}(text)"))),
                function,
                body,
            )
        };
        // ix -> w1 in the catalog; w1's text calls w2; w2's text calls leaf, which names it.
        let chain = |leaf_body: &str, w2_body: &str| {
            vec![
                call(stored, "w1", "SELECT app.w2(t)"),
                own("w1", "SELECT app.w2(t)"),
                own("w2", w2_body),
                own("leaf", leaf_body),
            ]
        };
        assert_eq!(
            spelled(&chain(named, "SELECT app.leaf(t)"), &names(&["docs"])),
            said("leaf")
        );
        // A catalog hop (`BEGIN ATOMIC`) from the dependent's function to a wrapper with no
        // text, whose text-called function then names the object.
        let mut atomic = vec![
            call(stored, "hop", ""),
            call((None, Some("app.hop(text)")), "w1", "SELECT app.leaf(t)"),
            own("leaf", named),
        ];
        atomic.push(own("hop", ""));
        assert_eq!(spelled(&atomic, &names(&["docs"])), said("leaf"));
        // A cycle ends and still finds what one of its members names.
        let cycle = vec![
            call(stored, "a", "SELECT app.b(t)"),
            call(stored, "b", "SELECT app.a(t), 'docs'"),
        ];
        assert_eq!(spelled(&cycle, &names(&["docs"])), said("b"));
        assert!(spelled(&cycle, &names(&["other"])).is_empty());
        // Negatives.
        assert!(
            spelled(
                &chain("SELECT to_tsvector('app.other', t)", "SELECT app.leaf(t)"),
                &names(&["docs"])
            )
            .is_empty()
        );
        assert!(
            spelled(
                &chain(named, "SELECT app.run('le' || 'af', t)"),
                &names(&["docs"])
            )
            .is_empty()
        );
        assert!(spelled(&chain(named, "SELECT app.leaf_v2(t)"), &names(&["docs"])).is_empty());
    }

    /// A body that reads a view reaches the view although the catalog records no edge to it
    /// (DEC-2728.3): the view counts when the catalog links it to the object (`touched`), or
    /// when the catalog leads from it to a function whose text names the object. A view the
    /// catalog already lists for the dependent adds no second line. Negatives: a view no
    /// catalog edge links, a longer identifier that only starts with the view's name, and a
    /// dependent the catalog listed already.
    #[test]
    fn a_view_a_body_reads_is_a_node_of_the_walk() {
        let view = |from: (Option<&str>, Option<&str>), name: &str, kind: &str| Call {
            what: from.0.map(str::to_owned),
            caller: from.1.map(str::to_owned),
            node: format!("app.{name}"),
            kind: kind.to_owned(),
            name: name.to_owned(),
            body: String::new(),
        };
        let stored = (Some("materialized view app.mv"), None);
        let rows = |body: &str| {
            vec![
                call(stored, "w", body),
                call((None, Some("app.w(text)")), "w", body),
                view((None, Some("app.v1")), "v1", "view"),
                // The view reads a function whose text names the object.
                call((None, Some("app.v1")), "leaf", "SELECT 'app.docs'"),
            ]
        };
        let touched = |nodes: &[&str]| -> BTreeSet<String> {
            nodes.iter().map(|n| (*n).to_owned()).collect()
        };
        let none = BTreeSet::new();
        let said = "materialized view app.mv (reached through view app.v1, which a body names)";
        let found = named_in_bodies(
            &rows("SELECT * FROM app.v1"),
            &none,
            &touched(&["app.v1"]),
            &none,
        );
        assert_eq!(found, [said.to_owned()].into());
        // The view's own functions are walked from it, whose text names the object.
        assert_eq!(
            named_in_bodies(&rows("SELECT * FROM app.v1"), &names(&["docs"]), &none, &none),
            [
                "materialized view app.mv (reached through function app.leaf(text), named in its body)"
                    .to_owned()
            ]
            .into()
        );
        // Negatives.
        assert!(
            named_in_bodies(
                &rows("SELECT * FROM app.v1"),
                &none,
                &touched(&["app.other"]),
                &none
            )
            .is_empty()
        );
        assert!(
            named_in_bodies(
                &rows("SELECT * FROM app.v10"),
                &names(&["docs"]),
                &touched(&["app.v1"]),
                &none
            )
            .is_empty()
        );
        // A view over a view the catalog links is one line, about the view the body names.
        let mut nested = rows("SELECT * FROM app.v2");
        nested.push(view((None, Some("app.v2")), "v2", "view"));
        nested.push(view((None, Some("app.v2")), "v1", "view"));
        let both = touched(&["app.v1", "app.v2"]);
        assert_eq!(
            named_in_bodies(&nested, &none, &both, &none),
            [said.replace("app.v1", "app.v2")].into()
        );
        let listed = touched(&["materialized view app.mv"]);
        assert!(
            named_in_bodies(
                &rows("SELECT * FROM app.v1"),
                &none,
                &touched(&["app.v1"]),
                &listed
            )
            .is_empty()
        );
    }
}
