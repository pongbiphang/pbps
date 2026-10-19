//! The live facts a plan over generated columns needs and the model does not
//! hold (DEC-1168.1): which server version this is, and which columns a
//! generated column is computed from.
//!
//! Neither is a property of a declaration. `SET EXPRESSION` exists from
//! PostgreSQL 17, and which columns an expression reads is the engine's own
//! record in `pg_depend` — asked of the catalog rather than parsed out of the
//! expression's text, which would be a second, weaker SQL parser.

use pbps_db::{Conn, DbError};
use pbps_model::TableName;

/// The first release with `ALTER COLUMN ... SET EXPRESSION`. Measured: a
/// syntax error on 16.15, accepted on 17.11 and 18.6.
pub const SET_EXPRESSION_ARRIVED_IN: i64 = 170_000;

/// One generated column's dependence on another column of its table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependence {
    /// The column the expression reads.
    pub base: pbps_model::ColumnName,
    /// The generated column computed from it.
    pub generated: pbps_model::ColumnName,
    /// The input is `NOT NULL` with no default and no identity, and no
    /// enabled row-level `BEFORE INSERT` trigger on the table could fill it:
    /// while it stands, a row inserted without it is refused (#1397). Where a
    /// trigger may supply it, whether it does is the engine's to say.
    pub base_required: bool,
    /// The input is `NOT NULL` with no default and no identity, and an
    /// enabled row-level `BEFORE INSERT` trigger on the table may fill it:
    /// required all the same where the plan drops or rewrites a trigger on
    /// the table, since the one that would fill it may be gone when the row
    /// is written (#2178).
    pub filled_by_trigger: bool,
}

/// Every column of `table` that a generated column of the same table is
/// computed from, as the engine recorded it when the expression was created:
/// `pg_depend` from the expression's `pg_attrdef` row to each column it reads.
/// Measured on all three supported releases, the engine refuses to retype
/// such a column ("cannot alter type of a column used by a generated
/// column") and drops it only with `CASCADE`.
///
/// Not the expression's edge to the column it belongs to: measured on 16.15,
/// 17.11 and 18.6, `pg_attrdef` also depends on its own column (`deptype`
/// `i`), and that column retypes freely. A generated column never reads
/// another generated column, let alone itself, so no input is lost.
///
/// Empty where the table has no generated column or does not exist yet, which
/// is the same answer here: nothing live depends on anything.
pub async fn dependences(conn: &mut Conn, table: &TableName) -> Result<Vec<Dependence>, DbError> {
    let sql = format!(
        "SELECT base.attname AS base, gen.attname AS generated,
                base.attnotnull AND NOT base.atthasdef AND base.attidentity = ''
                  AS strict,
                EXISTS (
                    SELECT 1 FROM pg_catalog.pg_trigger tg
                     WHERE tg.tgrelid = c.oid AND NOT tg.tgisinternal
                       AND tg.tgenabled <> 'D'
                       AND (tg.tgtype::integer & 7) = 7)
                  AS triggered
           FROM pg_catalog.pg_attrdef ad
           JOIN pg_catalog.pg_attribute gen
             ON gen.attrelid = ad.adrelid AND gen.attnum = ad.adnum AND gen.attgenerated <> ''
           JOIN pg_catalog.pg_depend d
             ON d.classid = 'pg_catalog.pg_attrdef'::pg_catalog.regclass AND d.objid = ad.oid
            AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass
            AND d.refobjid = ad.adrelid AND d.refobjsubid > 0
            AND d.refobjsubid <> ad.adnum
           JOIN pg_catalog.pg_attribute base
             ON base.attrelid = d.refobjid AND base.attnum = d.refobjsubid
           JOIN pg_catalog.pg_class c ON c.oid = ad.adrelid
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = {} AND c.relname = {}
          ORDER BY base.attname, gen.attname",
        literal(&table.schema),
        literal(&table.name)
    );
    let mut out = Vec::new();
    for row in conn.query(&sql).await? {
        let text = |column: &str| -> Result<String, DbError> {
            row.try_get::<&str>(column)?
                .map(str::to_owned)
                .ok_or_else(|| DbError::BadRow(format!("catalog column `{column}` is null")))
        };
        let flag = |column: &str| -> Result<bool, DbError> {
            row.try_get::<bool>(column)?
                .ok_or_else(|| DbError::BadRow(format!("catalog column `{column}` is null")))
        };
        let (strict, triggered) = (flag("strict")?, flag("triggered")?);
        out.push(Dependence {
            base: text("base")?.into(),
            generated: text("generated")?.into(),
            base_required: strict && !triggered,
            filled_by_trigger: strict && triggered,
        });
    }
    Ok(out)
}

fn literal(value: &str) -> String {
    format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

/// Whether an expression's text may read `column`: it holds an identifier
/// naming it, outside literals and comments, by the engine's quoting and
/// folding rules. An over-approximation. A function or a field of the same
/// name counts too, so a "yes" means *may* and a "no" means it does not
/// (DEC-1316.1).
///
/// A Unicode-escaped identifier is matched as the name it spells: the lexer
/// decodes `U&"\0061"`, and `UESCAPE` with it, to `"a"` before the scan.
#[must_use]
pub fn may_read(expression: &str, column: &str) -> bool {
    crate::impact::mentions(expression, column)
}

/// Whether a text may call or read anything named `name`: the scan
/// [`may_read`] runs, asked of a function's or a table's bare name. The
/// schema is not compared, so a name in another schema counts too; a "yes"
/// means *may* (DEC-1364.1).
#[must_use]
pub fn may_name(text: &str, name: &str) -> bool {
    crate::impact::mentions(text, name)
}

/// Whether an expression may call the function named `name`: [`may_name`],
/// or the name read the same way inside a string literal. Measured on 18.6, a
/// literal an OID-alias type reads (`'app.f(integer)'::regprocedure`,
/// `'app.f'::regproc`, `regprocedure('app.f(integer)')`) names the function
/// to the engine, which refuses it before the function exists and records a
/// dependency on it after. The literal may also be read so without a cast, as
/// a default of such a column or compared with one, so every literal counts.
/// Its contents are read as code, so a quoted name in it is one name, and as
/// the OID-alias input reads them, so a reserved word bare is a name too; a
/// comment is no literal (DEC-1364.1).
#[must_use]
pub fn may_call(expression: &str, name: &str) -> bool {
    may_name(expression, name)
        || crate::LEXICON
            .string_literals(expression)
            .iter()
            .any(|contents| crate::impact::mentions_in_literal(contents, name))
}

mod parts;
pub(crate) use parts::parts;

/// A consumer binds the full row shape even when it mentions no suffix
/// column explicitly. Reuse the identifier/literal scans, rather than trying
/// to recognize every SQL whole-row construct (DEC-1503.4).
pub(crate) fn names_table(text: &str, table: &pbps_model::TableName) -> bool {
    if may_call(text, &table.name) {
        return true;
    }
    let array = format!("_{}", table.name);
    // The catalog chooses collision suffixes. Consider every number present
    // in the text instead of guessing which suffix this plan will allocate.
    let mut regions = crate::LEXICON.string_literals(text);
    regions.push(crate::LEXICON.code_only(text));
    let numbers: std::collections::BTreeSet<_> = regions
        .iter()
        .flat_map(|region| region.split(|c: char| !c.is_ascii_digit()))
        .filter(|s| !s.is_empty())
        .collect();
    // Long names may be truncated for an array name. All character-boundary
    // prefixes over-approximate those spellings without predicting one.
    let ends: Vec<_> = if array.len() > 62 {
        array
            .char_indices()
            .map(|(i, _)| i)
            .filter(|i| *i > 1)
            .chain([array.len()])
            .collect()
    } else {
        vec![array.len()]
    };
    ends.into_iter().any(|end| {
        may_call(text, &array[..end])
            || numbers
                .iter()
                .any(|n| may_call(text, &format!("{}_{n}", &array[..end])))
    })
}

/// Prove the narrow literal-regclass form, including parentheses and built-in
/// casts used by the existing name fixtures. The existing lexer decodes the
/// literal and masks comments. No other reference is assumed name-only.
pub(crate) fn table_name_only(text: &str, table: &pbps_model::TableName) -> bool {
    let literals = crate::LEXICON.string_literals(text);
    if literals.len() != 1 || !crate::impact::mentions_in_literal(&literals[0], &table.name) {
        return false;
    }
    let code = crate::LEXICON.code_only(text);
    if names_table(&code, table) {
        return false;
    }
    let casts: String = code
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && !matches!(c, '(' | ')'))
        .collect();
    let mut casts = casts.split("::");
    casts.next() == Some("")
        && matches!(casts.next(), Some("regclass" | "pg_catalog.regclass"))
        && casts.all(|cast| {
            matches!(
                cast,
                "oid"
                    | "integer"
                    | "bigint"
                    | "text"
                    | "pg_catalog.oid"
                    | "pg_catalog.int4"
                    | "pg_catalog.int8"
                    | "pg_catalog.text"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{may_call, may_name, may_read};

    #[test]
    fn only_proven_relation_name_literals_use_the_prefix() {
        let table = pbps_model::TableName::new("app", "a");
        for text in [
            "'app.a'::regclass",
            "('app.a'::regclass)::oid::integer",
            "E'app.a'::pg_catalog.regclass",
        ] {
            assert!(super::names_table(text, &table));
            assert!(super::table_name_only(text, &table), "{text}");
        }
        for text in [
            "TABLE app.a",
            "INSERT INTO app.a VALUES(1,2,3)",
            "ROW(1,2,3)::app.a",
            "'app.a'::regtype",
            "'app._a'::regtype",
            "'app._a_1'::regtype",
            "'app.a'::regclass::text || 'app.a'::regtype::text",
            "SELECT a.id FROM app.a",
            r#"SELECT NULL::app.U&"_a_\0031""#,
        ] {
            assert!(super::names_table(text, &table), "{text}");
            assert!(!super::table_name_only(text, &table), "{text}");
        }
        assert!(!super::names_table("x * 2 /* app.a */", &table));
    }

    /// A function an OID-alias literal names is a call to the engine, so a
    /// name inside a literal counts; a longer name still does not
    /// (DEC-1364.1).
    #[test]
    fn a_function_named_inside_a_literal_may_be_called() {
        assert!(may_call("('app.f(integer)'::regprocedure)::text", "f"));
        assert!(may_call("'app.f'::regproc", "f"));
        assert!(may_call("regprocedure(E'app.f(integer)')", "f"));
        assert!(may_call("app.f(a)", "f"));
        // A quoted name is one name, delimiters and all.
        assert!(may_call(
            "'app.\"my func\"(integer)'::regprocedure",
            "my func"
        ));
        assert!(!may_call("'app.\"my func\"(integer)'::regprocedure", "my"));
        // A reserved word bare in a literal is a name to the OID-alias input,
        // and in code it is not.
        assert!(may_call("'select(integer)'::regprocedure", "select"));
        assert!(!may_call("a > (SELECT 0)", "select"));
        // Negative: a longer name, a comment, and no name at all.
        assert!(!may_call("'app.ff(integer)'::regprocedure", "f"));
        assert!(!may_call("0 /* f */", "f"));
        assert!(!may_call("0 -- 'f'", "f"));
        assert!(!may_call("a * 2", "f"));
    }

    /// A call is read by the function's bare name, qualified or not, in a
    /// routine's body as in an expression; a literal or a comment is no call
    /// (DEC-1364.1).
    #[test]
    fn a_function_is_named_by_its_bare_name_wherever_it_is_code() {
        assert!(may_name("app.f(a) + 1", "f"));
        assert!(may_name("f(a)", "f"));
        assert!(may_name(
            "(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT app.f(x) $$",
            "f"
        ));
        assert!(!may_name("app.ff(a)", "f"));
        assert!(!may_name("'f(a)' || b", "f"));
        assert!(!may_name("b -- f(a)", "f"));
    }

    /// The scan reads a name however it is spelled, a Unicode escape and its
    /// `UESCAPE` included, and data in a literal is no name (DEC-1316.1).
    #[test]
    fn an_escaped_identifier_is_read_as_the_name_it_spells() {
        assert!(may_read("a * 2", "a"));
        assert!(!may_read("b * 2", "a"));
        // A literal is data, not a name.
        assert!(!may_read("b || 'a'", "a"));
        assert!(may_read("U&\"\\0061\" * 2", "a"));
        assert!(may_read("u&\"\\0061\" * 2", "a"));
        assert!(may_read("U&\"!0061\" UESCAPE '!' * 2", "a"));
        assert!(!may_read("U&\"\\0062\" * 2", "a"));
        // Inside a literal it is data again.
        assert!(!may_read("b || 'U&\"x\"'", "a"));
    }
}
