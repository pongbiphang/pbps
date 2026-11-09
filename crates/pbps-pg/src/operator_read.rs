//! Reading an operator a package provides (DEC-2447.14, #2448 C1b).
//!
//! An exclusion constraint names its operator by schema and symbol, and an operator is
//! identified to the engine by that pair **and** its operand types. A key is compared with
//! itself, so the plan's own operand types are the key's, but the declaration does not carry
//! them: the pin therefore covers every overload of the symbol in the schema. Whether they
//! are a package's is each row's own `e` dependency on an extension, never its schema or name
//! (a lookalike in the package's schema has none), and all must be one package's. The digest
//! covers what each does: its function (owner, source, library), its commutator, negator and
//! join properties, and the operator families it is a member of, because an update that
//! changes any of them changes the exclusion under the plan that approved it.
//!
//! Read as a class is: the whole set twice and compared, so an operator that changed between
//! the queries comes back unreadable and never as one that never existed.

use pbps_db::{Conn, DbError, Param};
use pbps_model::escape::Escaped;
use sha2::{Digest, Sha256};

use crate::pins::Within;
use crate::user_types::need;

/// The operator a key names: its schema and symbol.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Wanted {
    pub schema: String,
    pub symbol: String,
}

impl Wanted {
    fn shown(&self) -> String {
        format!("{}.{}", Escaped(&self.schema), Escaped(&self.symbol))
    }
}

/// The operators the engine holds under one name, as far as a plan relies on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operator {
    pub wanted: Wanted,
    /// The lowest OID of the overloads, which the pin records.
    pub oid: i64,
    /// The package every overload belongs to; `None` when one belongs to none.
    pub extension: Option<(String, String)>,
    /// Overloads that belong to different packages, or to a package and to none.
    pub mixed: bool,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorRead {
    Present(Box<Operator>),
    Absent,
    Unreadable(String),
}

impl OperatorRead {
    /// Why this answer is not an operator a plan can rely on, or `None` for one that is.
    #[must_use]
    pub fn refusal(&self, wanted: &Wanted) -> Option<String> {
        match self {
            OperatorRead::Absent => Some(format!(
                "the operator {} is not in the target database and no package this plan \
                 creates stands in its schema",
                wanted.shown()
            )),
            OperatorRead::Unreadable(why) => Some(format!(
                "the operator {} cannot be read: {why}",
                wanted.shown()
            )),
            OperatorRead::Present(op) if op.mixed => Some(format!(
                "the operator {} has overloads that do not all belong to one extension; a \
                 name a package shares with a schema's own operator has nothing that vouches \
                 for which one the engine would pick",
                wanted.shown()
            )),
            OperatorRead::Present(op) if op.extension.is_none() => Some(format!(
                "the operator {} is not a member of any extension; a schema's own operator \
                 has nothing that vouches for what it does, and one that only shares a \
                 package's schema is not the package's",
                wanted.shown()
            )),
            OperatorRead::Present(_) => None,
        }
    }
}

/// One row per overload, with everything the digest covers.
const LOOKUP: &str = "
    SELECT o.oid::pg_catalog.int8 AS oid,
           pg_catalog.has_schema_privilege(n.oid, 'USAGE') AS schema_usage,
           (SELECT e.extname::pg_catalog.text
              FROM pg_catalog.pg_depend d
              JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid
             WHERE d.classid = 'pg_catalog.pg_operator'::pg_catalog.regclass
               AND d.objid = o.oid
               AND d.refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass
               AND d.deptype = 'e' LIMIT 1) AS extension,
           (SELECT e.extversion::pg_catalog.text
              FROM pg_catalog.pg_depend d
              JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid
             WHERE d.classid = 'pg_catalog.pg_operator'::pg_catalog.regclass
               AND d.objid = o.oid
               AND d.refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass
               AND d.deptype = 'e' LIMIT 1) AS version,
           pg_catalog.jsonb_build_object(
             'left', o.oprleft::pg_catalog.int8, 'right', o.oprright::pg_catalog.int8,
             'result', o.oprresult::pg_catalog.int8, 'owner', o.oprowner::pg_catalog.int8,
             'commutator', o.oprcom::pg_catalog.int8, 'negator', o.oprnegate::pg_catalog.int8,
             'merge', o.oprcanmerge, 'hash', o.oprcanhash,
             'function', o.oprcode::pg_catalog.oid::pg_catalog.int8,
             'function_owner', p.proowner::pg_catalog.int8,
             'source', p.prosrc, 'library', p.probin,
             'families', (SELECT COALESCE(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_array(
                            a.amopfamily::pg_catalog.int8, a.amopstrategy,
                            a.amoppurpose::pg_catalog.text, a.amoplefttype::pg_catalog.int8,
                            a.amoprighttype::pg_catalog.int8)
                          ORDER BY a.amopfamily, a.amopstrategy, a.amoplefttype), '[]')
                          FROM pg_catalog.pg_amop a WHERE a.amopopr = o.oid)
           )::pg_catalog.text AS facts
      FROM pg_catalog.pg_operator o
      JOIN pg_catalog.pg_namespace n ON n.oid = o.oprnamespace
      JOIN pg_catalog.pg_proc p ON p.oid = o.oprcode::pg_catalog.oid
     WHERE n.nspname::pg_catalog.text = $1
       AND o.oprname::pg_catalog.text = $2
     ORDER BY o.oid";

async fn read_one(conn: &mut Conn, wanted: &Wanted) -> Result<OperatorRead, DbError> {
    let rows = conn
        .query_with(
            LOOKUP,
            &[Param::Str(&wanted.schema), Param::Str(&wanted.symbol)],
        )
        .await?;
    if rows.is_empty() {
        return Ok(OperatorRead::Absent);
    }
    let mut extensions = Vec::new();
    let mut facts = Sha256::new();
    let mut oid = i64::MAX;
    for row in &rows {
        if !need::<bool>(row, "schema_usage")? {
            return Ok(OperatorRead::Unreadable(format!(
                "the role has no USAGE on schema `{}`",
                Escaped(&wanted.schema)
            )));
        }
        oid = oid.min(need::<i64>(row, "oid")?);
        extensions.push(
            row.try_get::<&str>("extension")?
                .map(str::to_owned)
                .zip(row.try_get::<&str>("version")?.map(str::to_owned)),
        );
        facts.update(need::<&str>(row, "facts")?.as_bytes());
        facts.update([0]);
    }
    let first = extensions[0].clone();
    let mixed = extensions
        .iter()
        .any(|e| e.as_ref().map(|e| &e.0) != first.as_ref().map(|e| &e.0));
    Ok(OperatorRead::Present(Box::new(Operator {
        wanted: wanted.clone(),
        oid,
        extension: first,
        mixed,
        digest: hex(&facts.finalize()),
    })))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// One answer per operator, in order. A failed read is every operator unreadable.
pub async fn read(conn: &mut Conn, within: Within, wanted: &[Wanted]) -> Vec<OperatorRead> {
    crate::settled_read::read::<Operators>(conn, within, wanted).await
}

/// How an operator is read, for the double read of [`crate::settled_read`].
pub(crate) struct Operators;

impl crate::settled_read::Kind for Operators {
    type Wanted = Wanted;
    type Read = OperatorRead;
    const SAVEPOINT: &'static str = "pbps_operator_reads";
    const NOUN: &'static str = "operator";

    fn unreadable(why: String) -> OperatorRead {
        OperatorRead::Unreadable(why)
    }

    async fn read_one(conn: &mut Conn, wanted: &Wanted) -> Result<OperatorRead, DbError> {
        read_one(conn, wanted).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operator(extension: Option<(&str, &str)>, mixed: bool) -> OperatorRead {
        OperatorRead::Present(Box::new(Operator {
            wanted: Wanted {
                schema: "ext".into(),
                symbol: "%".into(),
            },
            oid: 1,
            extension: extension.map(|(a, b)| (a.into(), b.into())),
            mixed,
            digest: "d".into(),
        }))
    }

    #[test]
    fn an_operator_no_package_owns_or_that_a_package_shares_with_another_is_refused() {
        let wanted = Wanted {
            schema: "ext".into(),
            symbol: "%".into(),
        };
        assert_eq!(
            operator(Some(("pg_trgm", "1.6")), false).refusal(&wanted),
            None
        );
        let lookalike = operator(None, false).refusal(&wanted).unwrap();
        assert!(
            lookalike.contains("not a member of any extension"),
            "{lookalike}"
        );
        let shared = operator(Some(("pg_trgm", "1.6")), true)
            .refusal(&wanted)
            .unwrap();
        assert!(
            shared.contains("do not all belong to one extension"),
            "{shared}"
        );
        assert!(
            OperatorRead::Absent
                .refusal(&wanted)
                .unwrap()
                .contains("not in the target")
        );
        assert!(
            OperatorRead::Unreadable("x".into())
                .refusal(&wanted)
                .unwrap()
                .contains("cannot be read")
        );
    }
}
