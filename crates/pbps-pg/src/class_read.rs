//! Reading an operator class a package provides (DEC-2447.14).
//!
//! A class is `(method, schema, name)` to the engine. Whether it is a package's
//! is the class row's own `e` dependency on an extension, never its schema or
//! its name: a lookalike in the package's schema has none (measured on 16.15 and
//! 18.6), and so has a class of a schema's own. The pin's digest covers what the
//! class can do, the operators and support functions of its family, because
//! those rows carry no dependency on the package and an update that adds one
//! changes the class under the plan that approved it (`pg_trgm` 1.3 to 1.6).
//!
//! Read as E reads a type: the whole set twice and compared, so a class that
//! changed between the queries comes back unreadable and never as a definition
//! that never existed; absent, unreadable and refused are three answers.

use pbps_db::{Conn, DbError, Param, Row};
use pbps_model::escape::Escaped;
use sha2::{Digest, Sha256};

use crate::pins::Within;
use crate::user_types::{OWN_TRANSACTION, need};

const SAVEPOINT: &str = "pbps_class_reads";

/// The class a key names, with the access method of the index it is used in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Wanted {
    pub method: String,
    pub schema: String,
    pub name: String,
}

impl Wanted {
    /// The class as a message names it.
    fn shown(&self) -> String {
        format!(
            "{}.{} for access method {}",
            Escaped(&self.schema),
            Escaped(&self.name),
            Escaped(&self.method)
        )
    }
}

/// A class the engine holds, as far as a plan relies on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Class {
    pub wanted: Wanted,
    pub oid: i64,
    /// `opcdefault`: a key naming its method's default class for the type
    /// spells what the catalog cannot print, so the declaration is refused.
    pub is_default: bool,
    /// The type the class is for, as `format_type` writes it.
    pub input_type: String,
    /// The package that owns the class row and, separately, its family: both
    /// must be members of the same one.
    pub extension: Option<(String, String)>,
    pub family_extension: Option<String>,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassRead {
    Present(Box<Class>),
    Absent,
    Unreadable(String),
}

impl ClassRead {
    /// Why this answer is not a class a plan can rely on, or `None` for one
    /// that is: absent, unreadable, and a class that no package owns.
    #[must_use]
    pub fn refusal(&self, wanted: &Wanted) -> Option<String> {
        match self {
            ClassRead::Absent => Some(format!(
                "the operator class {} is not in the target database and no package this \
                 plan creates stands in its schema",
                wanted.shown()
            )),
            ClassRead::Unreadable(why) => Some(format!(
                "the operator class {} cannot be read: {why}",
                wanted.shown()
            )),
            ClassRead::Present(class) => match (&class.extension, &class.family_extension) {
                (Some((a, _)), Some(b)) if a == b => None,
                (Some((a, _)), _) => Some(format!(
                    "the operator class {} belongs to the extension `{}`, but its operator \
                     family does not; this model takes a class only when a package owns both",
                    wanted.shown(),
                    Escaped(a)
                )),
                (None, _) => Some(format!(
                    "the operator class {} is not a member of any extension; a schema's own \
                     class has nothing that vouches for what it does, and a class that only \
                     shares a package's name is not the package's",
                    wanted.shown()
                )),
            },
        }
    }
}

/// Everything about one class a plan relies on. `pg_amop` and `pg_amproc`
/// rows are read by OID: a move renames the schema and replaces nothing, and
/// the digest must survive it. A support function's body is in the digest, so a
/// function replaced under the plan moves it too.
const LOOKUP: &str = "
    SELECT oc.oid::pg_catalog.int8 AS oid,
           oc.opcdefault AS is_default,
           pg_catalog.format_type(oc.opcintype, NULL)::pg_catalog.text AS input_type,
           pg_catalog.has_schema_privilege(n.oid, 'USAGE') AS schema_usage,
           (SELECT e.extname::pg_catalog.text
              FROM pg_catalog.pg_depend d
              JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid
             WHERE d.classid = 'pg_catalog.pg_opclass'::pg_catalog.regclass
               AND d.objid = oc.oid
               AND d.refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass
               AND d.deptype = 'e'
             LIMIT 1) AS extension,
           (SELECT e.extversion::pg_catalog.text
              FROM pg_catalog.pg_depend d
              JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid
             WHERE d.classid = 'pg_catalog.pg_opclass'::pg_catalog.regclass
               AND d.objid = oc.oid
               AND d.refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass
               AND d.deptype = 'e'
             LIMIT 1) AS version,
           (SELECT e.extname::pg_catalog.text
              FROM pg_catalog.pg_depend d
              JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid
             WHERE d.classid = 'pg_catalog.pg_opfamily'::pg_catalog.regclass
               AND d.objid = oc.opcfamily
               AND d.refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass
               AND d.deptype = 'e'
             LIMIT 1) AS family_extension,
           pg_catalog.jsonb_build_object(
             'method', am.amname::pg_catalog.text,
             'input', oc.opcintype::pg_catalog.int8,
             'key', oc.opckeytype::pg_catalog.int8,
             'default', oc.opcdefault,
             'owner', oc.opcowner::pg_catalog.int8,
             'family', oc.opcfamily::pg_catalog.int8,
             'operators', (SELECT COALESCE(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_array(
                             o.amopstrategy, o.amoppurpose::pg_catalog.text,
                             o.amopopr::pg_catalog.int8, o.amoplefttype::pg_catalog.int8,
                             o.amoprighttype::pg_catalog.int8, o.amopsortfamily::pg_catalog.int8)
                           ORDER BY o.amopstrategy, o.amoplefttype, o.amoprighttype), '[]')
                           FROM pg_catalog.pg_amop o WHERE o.amopfamily = oc.opcfamily),
             'support', (SELECT COALESCE(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_array(
                           a.amprocnum, a.amproclefttype::pg_catalog.int8,
                           a.amprocrighttype::pg_catalog.int8, a.amproc::pg_catalog.oid::pg_catalog.int8,
                           p.proowner::pg_catalog.int8, p.prosrc, p.probin)
                         ORDER BY a.amprocnum, a.amproclefttype, a.amprocrighttype), '[]')
                         FROM pg_catalog.pg_amproc a
                         JOIN pg_catalog.pg_proc p ON p.oid = a.amproc::pg_catalog.oid
                        WHERE a.amprocfamily = oc.opcfamily))::pg_catalog.text AS facts
      FROM pg_catalog.pg_opclass oc
      JOIN pg_catalog.pg_am am ON am.oid = oc.opcmethod
      JOIN pg_catalog.pg_namespace n ON n.oid = oc.opcnamespace
     WHERE am.amname::pg_catalog.text = $1
       AND n.nspname::pg_catalog.text = $2
       AND oc.opcname::pg_catalog.text = $3";

async fn read_one(conn: &mut Conn, wanted: &Wanted) -> Result<ClassRead, DbError> {
    let rows = conn
        .query_with(
            LOOKUP,
            &[
                Param::Str(&wanted.method),
                Param::Str(&wanted.schema),
                Param::Str(&wanted.name),
            ],
        )
        .await?;
    let row: &Row = match rows.as_slice() {
        [] => return Ok(ClassRead::Absent),
        [row] => row,
        _ => {
            return Err(DbError::BadRow(
                "a class name matched more than one `pg_opclass` row".to_owned(),
            ));
        }
    };
    if !need::<bool>(row, "schema_usage")? {
        return Ok(ClassRead::Unreadable(format!(
            "the role has no USAGE on schema `{}`",
            Escaped(&wanted.schema)
        )));
    }
    let extension = row.try_get::<&str>("extension")?.map(str::to_owned);
    let version = row.try_get::<&str>("version")?.map(str::to_owned);
    Ok(ClassRead::Present(Box::new(Class {
        wanted: wanted.clone(),
        oid: need::<i64>(row, "oid")?,
        is_default: need::<bool>(row, "is_default")?,
        input_type: need::<&str>(row, "input_type")?.to_owned(),
        extension: extension.zip(version),
        family_extension: row.try_get::<&str>("family_extension")?.map(str::to_owned),
        digest: hex(&Sha256::digest(need::<&str>(row, "facts")?.as_bytes())),
    })))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Reads each class twice and keeps the answer both passes agree on.
async fn read_each(conn: &mut Conn, wanted: &[Wanted]) -> Result<Vec<ClassRead>, DbError> {
    conn.query("SELECT pg_catalog.set_config('search_path', '', true) AS was_set")
        .await?;
    let mut passes = [Vec::new(), Vec::new()];
    for pass in &mut passes {
        for one in wanted {
            pass.push(read_one(conn, one).await?);
        }
    }
    let [first, second] = passes;
    Ok(first
        .into_iter()
        .zip(second)
        .map(|(first, second)| {
            if first == second {
                first
            } else {
                ClassRead::Unreadable("the class changed while it was being read".to_owned())
            }
        })
        .collect())
}

async fn read_all(
    conn: &mut Conn,
    within: Within,
    wanted: &[Wanted],
) -> Result<Vec<ClassRead>, DbError> {
    match within {
        Within::OwnTransaction => {
            conn.begin(&OWN_TRANSACTION).await?;
            let result = read_each(conn, wanted).await;
            let closed = conn.rollback(&OWN_TRANSACTION).await;
            let reads = result?;
            closed?;
            Ok(reads)
        }
        Within::CallersTransaction => {
            conn.execute(&format!("SAVEPOINT {SAVEPOINT}")).await?;
            let result = read_each(conn, wanted).await;
            // Leave the caller's transaction usable if it can be: a failed
            // statement aborts it until the savepoint is rolled back.
            let undone = conn
                .execute(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))
                .await;
            let released = conn
                .execute(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))
                .await;
            let reads = result?;
            undone?;
            released?;
            Ok(reads)
        }
    }
}

/// One answer per class, in order. A failed read is every class unreadable.
pub async fn read(conn: &mut Conn, within: Within, wanted: &[Wanted]) -> Vec<ClassRead> {
    match read_all(conn, within, wanted).await {
        Ok(reads) => reads,
        Err(e) => wanted
            .iter()
            .map(|_| ClassRead::Unreadable(e.to_string()))
            .collect(),
    }
}
