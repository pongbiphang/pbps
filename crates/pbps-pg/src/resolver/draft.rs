//! A draft of a resolver's externals file, read from the target (#1778,
//! SPEC §9.3.2): `pbps resolver externals`.
//!
//! The draft lists the objects outside the managed set that the managed
//! objects the target holds depend on, as the target records it in
//! `pg_depend`, with the shapes those objects need in turn: a column's type,
//! a domain's base type, the opclass of a compared key. `--schema` adds every
//! unmanaged object of a schema, for references only declarations that are
//! not deployed yet make.
//!
//! It writes SQL for tables (columns, types, order, keys), views as shape
//! views of their output columns, enums, domains, composite types and
//! extensions with their version: the classes the comparison reads, in the
//! shapes it compares. A routine is listed by signature and never written,
//! since pbps never sends routine source to scratch. Every other class
//! (casts, operators, collations, operator classes, a partitioned table) is
//! listed for the operator to write by hand. An object whose shape names a
//! managed object is a chain no externals file can stage; it is listed with
//! the two remedies and written as nothing.
//!
//! The draft is for review: whatever the file ends up holding is compared
//! with the target the same way, whoever wrote it (DEC-1673.1).

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::transport::QueryConnection;
use pbps_db::{DbError, Row};

/// An object in a catalog: the catalog's name and the object's OID.
type Key = (String, u32);

/// The drafted file, and one line for each object it lists rather than
/// writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub sql: String,
    pub listed: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("a declared routine's signature: {0}")]
    Declaration(String),
    #[error(
        "--schema {0}: the target has no schema of that name; an empty schema and an absent \
         one draft differently, so a misspelt name is refused rather than read as empty"
    )]
    AbsentSchema(String),
    #[error("{what}: {source}")]
    Read {
        what: &'static str,
        #[source]
        source: DbError,
    },
}

/// Drafts the externals file for `schema`'s managed objects from the
/// connected target, plus every unmanaged object in `schemas`.
pub async fn draft(
    conn: &mut impl QueryConnection,
    schema: &pbps_model::Schema,
    schemas: &[String],
) -> Result<Draft, Error> {
    let managed = super::capture::Managed::from_schema(schema);
    // The CLI's PostgreSQL dialect configures no write-path extras, as the
    // resolver's own lookup (`resolution.rs`).
    let dialect = crate::Postgres::with_write_path_extras(Vec::new());
    let mut signatures = Vec::new();
    for (id, module) in &schema.modules {
        if let Some(signature) = super::reconstruct::dropped_signature(&dialect, id, module.kind)
            .map_err(|error| Error::Declaration(error.to_string()))?
        {
            signatures.push(signature);
        }
    }
    let (routines, facts) = read(conn, &signatures)
        .await
        .map_err(|source| Error::Read {
            what: "the target's catalog",
            source,
        })?;
    if let Some(absent) = schemas
        .iter()
        .find(|schema| !facts.schemas.contains(*schema))
    {
        return Err(Error::AbsentSchema(absent.clone()));
    }
    let is_managed = |key: &Key| match key.0.as_str() {
        "pg_class" => facts
            .relations
            .get(&key.1)
            .is_some_and(|relation| managed.relation(&relation.schema, &relation.name)),
        "pg_proc" => routines.contains(&key.1),
        _ => false,
    };
    Ok(compose(&facts, &is_managed, schemas))
}

// ---- What the target holds -------------------------------------------------

/// The target's catalog, as far as a draft reads it: every object initdb did
/// not create, their recorded dependencies, and the shapes of the classes a
/// draft writes.
#[derive(Debug, Default)]
struct Facts {
    /// Every schema's name, initdb's too.
    schemas: BTreeSet<String>,
    objects: BTreeMap<Key, Object>,
    depends: Vec<Depend>,
    relations: BTreeMap<u32, Relation>,
    columns: BTreeMap<u32, Vec<Column>>,
    /// Primary-key, unique and exclusion constraints by relation.
    keys: BTreeMap<u32, Vec<Keyed>>,
    /// NOT NULL constraints (18): by relation and column, or by domain and 0.
    not_nulls: BTreeMap<(u32, i32), NotNull>,
    /// Unique indexes and the indexes of keys, by relation.
    indexes: BTreeMap<u32, Vec<Index>>,
    sequences: BTreeMap<u32, Sequence>,
    types: BTreeMap<u32, Type>,
    labels: BTreeMap<u32, Vec<String>>,
    extensions: BTreeMap<u32, Extension>,
}

#[derive(Debug, Clone)]
struct Object {
    /// The schema, quoted, or empty for an object in none.
    schema: String,
    described: String,
}

/// One `pg_depend` row. `shape` says whether the dependent is a part the
/// comparison reads: not a view's query, a default, a trigger, a policy, a
/// foreign key, a CHECK or a plain index (SPEC §9.3.2, #1783); of a
/// routine only the types its signature names, never what its body reads;
/// of a domain only its base type and collation, never its default.
#[derive(Debug, Clone)]
struct Depend {
    dependent: Key,
    referenced: Key,
    deptype: char,
    shape: bool,
}

#[derive(Debug, Clone, Default)]
struct Relation {
    kind: char,
    schema: String,
    name: String,
    qualified: String,
    unlogged: bool,
    options: String,
    replica_identity: char,
    row_security: bool,
    forced_row_security: bool,
    populated: bool,
    /// Why the draft writes no SQL for it, if a structure it does not
    /// write: a partition, inheritance, a typed table.
    structure: String,
    tablespace: bool,
    /// The access method, quoted, unless it is `heap`.
    method: String,
}

#[derive(Debug, Clone, Default)]
struct Column {
    number: i32,
    name: String,
    /// `format_type`'s spelling, qualified under the empty path.
    spelled: String,
    dimensions: i32,
    not_null: bool,
    identity: Option<char>,
    generated: bool,
    /// The column's collation, quoted, where it is not its type's.
    collation: Option<String>,
    storage: Option<char>,
    compression: Option<char>,
    options: String,
    inherited: bool,
}

#[derive(Debug, Clone)]
struct Keyed {
    name: String,
    definition: String,
}

#[derive(Debug, Clone)]
struct NotNull {
    name: String,
    plain: bool,
}

#[derive(Debug, Clone)]
struct Index {
    name: String,
    definition: String,
    keyed: bool,
    clustered: bool,
    replica_identity: bool,
    /// False for an index a failed concurrent build left: a statement
    /// builds a valid one, and the comparison reads which it is.
    valid: bool,
    /// In a tablespace of its own, which scratch need not have.
    tablespace: bool,
}

#[derive(Debug, Clone, Default)]
struct Sequence {
    qualified: String,
    spelled: String,
    start: String,
    increment: String,
    min: String,
    max: String,
    cache: String,
    cycle: bool,
    unlogged: bool,
    /// The relation and column it belongs to, and how: `a` for `OWNED BY`,
    /// `i` for an identity.
    owner: Option<(u32, i32, char)>,
}

#[derive(Debug, Clone, Default)]
struct Type {
    kind: char,
    qualified: String,
    relation: u32,
    base: String,
    /// The base's declared array dimensions, which `format_type` does not
    /// spell.
    dimensions: i32,
    not_null: bool,
    collation: Option<String>,
}

#[derive(Debug, Clone)]
struct Extension {
    name: String,
    schema: String,
    version: String,
}

/// The managed routines' OIDs and [`Facts`], read in one `REPEATABLE READ
/// READ ONLY` transaction, so every part describes one catalog state: a
/// routine recreated between two reads would otherwise read as unmanaged
/// (review of #2024).
async fn read(
    conn: &mut impl QueryConnection,
    signatures: &[super::capture::DroppedSignature],
) -> Result<(BTreeSet<u32>, Facts), DbError> {
    conn.query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await?;
    let read = async {
        let routines = super::externals::routine_oids(conn, signatures).await?;
        // Each lookup sets a transaction-local path, which inside this
        // transaction outlives its statement; the reads below spell every
        // name under the empty path every pbps session pins (DEC-1564.1).
        conn.query("SELECT pg_catalog.set_config('search_path', '', true)")
            .await?;
        Ok::<_, DbError>((routines, read_in(conn).await?))
    }
    .await;
    let ended = conn
        .query(if read.is_ok() { "COMMIT" } else { "ROLLBACK" })
        .await;
    let read = read?;
    ended?;
    Ok(read)
}

async fn read_in(conn: &mut impl QueryConnection) -> Result<Facts, DbError> {
    let mut facts = Facts::default();
    for row in conn
        .query("SELECT n.nspname::text AS name FROM pg_catalog.pg_namespace n")
        .await?
    {
        facts.schemas.insert(text(&row, "name")?);
    }
    for row in conn
        .query(&format!(
            "SELECT (SELECT c.relname::text FROM pg_catalog.pg_class c WHERE c.oid = f.classid) \
                      AS catalog, f.objid::text AS oid, \
                    coalesce(pg_catalog.quote_ident(i.schema), '') AS schema, \
                    pg_catalog.pg_describe_object(f.classid, f.objid, 0) AS described \
               FROM ({}) f \
               CROSS JOIN LATERAL pg_catalog.pg_identify_object(f.classid, f.objid, 0) i",
            super::scratch::user_objects()
        ))
        .await?
    {
        facts.objects.insert(
            (text(&row, "catalog")?, number(&row, "oid")?),
            Object {
                schema: text(&row, "schema")?,
                described: text(&row, "described")?,
            },
        );
    }
    for row in conn.query(DEPENDS).await? {
        facts.depends.push(Depend {
            dependent: (text(&row, "catalog")?, number(&row, "oid")?),
            referenced: (text(&row, "ref_catalog")?, number(&row, "ref_oid")?),
            deptype: character(&row, "deptype")?.unwrap_or('n'),
            shape: flag(&row, "shape")?,
        });
    }
    for row in conn.query(RELATIONS).await? {
        facts.relations.insert(
            number(&row, "oid")?,
            Relation {
                kind: character(&row, "kind")?.unwrap_or(' '),
                schema: text(&row, "schema")?,
                name: text(&row, "name")?,
                qualified: text(&row, "qualified")?,
                unlogged: text(&row, "persistence")? == "u",
                options: text(&row, "options")?,
                replica_identity: character(&row, "replica_identity")?.unwrap_or('d'),
                row_security: flag(&row, "row_security")?,
                forced_row_security: flag(&row, "forced_row_security")?,
                populated: flag(&row, "populated")?,
                structure: text(&row, "structure")?,
                tablespace: flag(&row, "tablespace")?,
                method: text(&row, "method")?,
            },
        );
    }
    for row in conn.query(COLUMNS).await? {
        facts
            .columns
            .entry(number(&row, "relation")?)
            .or_default()
            .push(Column {
                number: signed(&row, "number")?,
                name: text(&row, "name")?,
                spelled: text(&row, "spelled")?,
                dimensions: signed(&row, "dimensions")?,
                not_null: flag(&row, "not_null")?,
                identity: character(&row, "identity")?,
                generated: character(&row, "generated")?.is_some(),
                collation: optional(&row, "collation")?,
                storage: character(&row, "storage")?,
                compression: character(&row, "compression")?,
                options: text(&row, "options")?,
                inherited: flag(&row, "inherited")?,
            });
    }
    for row in conn.query(CONSTRAINTS).await? {
        let name = text(&row, "name")?;
        match character(&row, "kind")? {
            Some('n') => {
                let owner = match number(&row, "relation")? {
                    0 => (number(&row, "type")?, 0),
                    relation => (relation, signed(&row, "column")?),
                };
                facts.not_nulls.insert(
                    owner,
                    NotNull {
                        name,
                        plain: flag(&row, "plain")?,
                    },
                );
            }
            _ => facts
                .keys
                .entry(number(&row, "relation")?)
                .or_default()
                .push(Keyed {
                    name,
                    definition: text(&row, "definition")?,
                }),
        }
    }
    for row in conn.query(INDEXES).await? {
        facts
            .indexes
            .entry(number(&row, "relation")?)
            .or_default()
            .push(Index {
                name: text(&row, "name")?,
                definition: text(&row, "definition")?,
                keyed: flag(&row, "keyed")?,
                clustered: flag(&row, "clustered")?,
                replica_identity: flag(&row, "replica_identity")?,
                valid: flag(&row, "valid")?,
                tablespace: flag(&row, "tablespace")?,
            });
    }
    for row in conn.query(SEQUENCES).await? {
        let owner = match optional(&row, "owner")? {
            Some(owner) => Some((
                owner
                    .parse()
                    .map_err(|_| DbError::BadRow("a sequence's owner was not a number".into()))?,
                signed(&row, "owner_column")?,
                character(&row, "owned")?.unwrap_or('a'),
            )),
            None => None,
        };
        facts.sequences.insert(
            number(&row, "oid")?,
            Sequence {
                qualified: text(&row, "qualified")?,
                spelled: text(&row, "spelled")?,
                start: text(&row, "start")?,
                increment: text(&row, "increment")?,
                min: text(&row, "min")?,
                max: text(&row, "max")?,
                cache: text(&row, "cache")?,
                cycle: flag(&row, "cycle")?,
                unlogged: text(&row, "persistence")? == "u",
                owner,
            },
        );
    }
    for row in conn.query(TYPES).await? {
        facts.types.insert(
            number(&row, "oid")?,
            Type {
                kind: character(&row, "kind")?.unwrap_or(' '),
                qualified: text(&row, "qualified")?,
                relation: number(&row, "relation")?,
                base: optional(&row, "base")?.unwrap_or_default(),
                dimensions: signed(&row, "dimensions")?,
                not_null: flag(&row, "not_null")?,
                collation: optional(&row, "collation")?,
            },
        );
    }
    for row in conn.query(LABELS).await? {
        facts
            .labels
            .entry(number(&row, "type")?)
            .or_default()
            .push(text(&row, "label")?);
    }
    for row in conn.query(EXTENSIONS).await? {
        facts.extensions.insert(
            number(&row, "oid")?,
            Extension {
                name: text(&row, "name")?,
                schema: text(&row, "schema")?,
                version: text(&row, "version")?,
            },
        );
    }
    Ok(facts)
}

const DEPENDS: &str = "\
SELECT (SELECT c.relname::text FROM pg_catalog.pg_class c WHERE c.oid = d.classid) AS catalog, \
       d.objid::text AS oid, \
       (SELECT c.relname::text FROM pg_catalog.pg_class c WHERE c.oid = d.refclassid) AS ref_catalog, \
       d.refobjid::text AS ref_oid, d.deptype::text AS deptype, \
       (CASE d.classid \
          WHEN 'pg_catalog.pg_rewrite'::pg_catalog.regclass THEN false \
          WHEN 'pg_catalog.pg_attrdef'::pg_catalog.regclass THEN false \
          WHEN 'pg_catalog.pg_trigger'::pg_catalog.regclass THEN false \
          WHEN 'pg_catalog.pg_policy'::pg_catalog.regclass THEN false \
          WHEN 'pg_catalog.pg_statistic_ext'::pg_catalog.regclass THEN false \
          WHEN 'pg_catalog.pg_constraint'::pg_catalog.regclass THEN \
            coalesce((SELECT k.contype IN ('p', 'u', 'x', 'n') FROM pg_catalog.pg_constraint k \
                       WHERE k.oid = d.objid), false) \
          WHEN 'pg_catalog.pg_proc'::pg_catalog.regclass THEN \
            d.refclassid = 'pg_catalog.pg_type'::pg_catalog.regclass \
            AND EXISTS (SELECT FROM pg_catalog.pg_proc p WHERE p.oid = d.objid \
                          AND (d.refobjid = p.prorettype \
                               OR d.refobjid = ANY (p.proargtypes::pg_catalog.oid[]) \
                               OR d.refobjid = ANY (coalesce(p.proallargtypes, '{}')))) \
          WHEN 'pg_catalog.pg_type'::pg_catalog.regclass THEN \
            coalesce((SELECT t.typtype <> 'd' \
                             OR (d.refclassid = 'pg_catalog.pg_type'::pg_catalog.regclass \
                                 AND d.refobjid = t.typbasetype) \
                             OR (d.refclassid = 'pg_catalog.pg_collation'::pg_catalog.regclass \
                                 AND d.refobjid = t.typcollation) \
                        FROM pg_catalog.pg_type t WHERE t.oid = d.objid), true) \
          WHEN 'pg_catalog.pg_class'::pg_catalog.regclass THEN \
            NOT EXISTS (SELECT FROM pg_catalog.pg_index i \
                         WHERE i.indexrelid = d.objid AND NOT i.indisunique \
                           AND NOT EXISTS (SELECT FROM pg_catalog.pg_constraint k \
                                            WHERE k.conindid = i.indexrelid \
                                              AND k.conrelid = i.indrelid AND k.contype = 'x')) \
          ELSE true END)::text AS shape \
  FROM pg_catalog.pg_depend d \
 WHERE d.deptype IN ('n', 'a', 'i', 'e') AND (d.objid >= 16384 OR d.refobjid >= 16384)";

const RELATIONS: &str = "\
SELECT c.oid::text AS oid, c.relkind::text AS kind, n.nspname::text AS schema, \
       c.relname::text AS name, \
       pg_catalog.quote_ident(n.nspname) || '.' || pg_catalog.quote_ident(c.relname) AS qualified, \
       c.relpersistence::text AS persistence, \
       coalesce(pg_catalog.array_to_string(c.reloptions, ', '), '') AS options, \
       c.relreplident::text AS replica_identity, c.relrowsecurity::text AS row_security, \
       c.relforcerowsecurity::text AS forced_row_security, c.relispopulated::text AS populated, \
       CASE WHEN c.relkind = 'p' THEN 'a partitioned table' \
            WHEN c.relispartition THEN 'a partition' \
            WHEN c.reloftype <> 0 THEN 'a typed table' \
            WHEN EXISTS (SELECT FROM pg_catalog.pg_inherits i \
                          WHERE i.inhrelid = c.oid OR i.inhparent = c.oid) THEN 'inheritance' \
            ELSE '' END AS structure, \
       (c.reltablespace <> 0)::text AS tablespace, \
       coalesce((SELECT pg_catalog.quote_ident(a.amname) FROM pg_catalog.pg_am a \
                  WHERE a.oid = c.relam AND a.amname <> 'heap'), '') AS method \
  FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
 WHERE c.oid >= 16384 AND c.relkind IN ('r', 'p', 'f', 'v', 'm', 'c', 'S')";

const COLUMNS: &str = "\
SELECT a.attrelid::text AS relation, a.attnum::text AS number, \
       pg_catalog.quote_ident(a.attname) AS name, \
       pg_catalog.format_type(a.atttypid, a.atttypmod) AS spelled, \
       a.attndims::text AS dimensions, a.attnotnull::text AS not_null, \
       a.attidentity::text AS identity, a.attgenerated::text AS generated, \
       CASE WHEN a.attcollation <> t.typcollation AND a.attcollation <> 0 THEN \
         (SELECT pg_catalog.quote_ident(cn.nspname) || '.' || pg_catalog.quote_ident(co.collname) \
            FROM pg_catalog.pg_collation co \
            JOIN pg_catalog.pg_namespace cn ON cn.oid = co.collnamespace \
           WHERE co.oid = a.attcollation) END AS collation, \
       CASE WHEN a.attstorage <> t.typstorage THEN a.attstorage::text ELSE '' END AS storage, \
       a.attcompression::text AS compression, \
       coalesce(pg_catalog.array_to_string(a.attoptions, ', '), '') AS options, \
       (NOT a.attislocal OR a.attinhcount > 0)::text AS inherited \
  FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
 WHERE a.attrelid >= 16384 AND a.attnum > 0 AND NOT a.attisdropped \
 ORDER BY a.attrelid, a.attnum";

// A NOT NULL constraint is a row of its own on 18 and none on 16; its name
// is compared there, so the draft writes it.
const CONSTRAINTS: &str = "\
SELECT k.conrelid::text AS relation, k.contypid::text AS type, k.contype::text AS kind, \
       pg_catalog.quote_ident(k.conname) AS name, \
       pg_catalog.pg_get_constraintdef(k.oid) AS definition, \
       coalesce(k.conkey[1], 0)::text AS column, \
       (k.convalidated AND NOT k.connoinherit)::text AS plain \
  FROM pg_catalog.pg_constraint k \
 WHERE k.oid >= 16384 AND k.contype IN ('p', 'u', 'x', 'n') \
 ORDER BY k.conname";

const INDEXES: &str = "\
SELECT i.indrelid::text AS relation, pg_catalog.quote_ident(c.relname) AS name, \
       pg_catalog.pg_get_indexdef(i.indexrelid) AS definition, \
       EXISTS (SELECT FROM pg_catalog.pg_constraint k \
                WHERE k.conindid = i.indexrelid AND k.conrelid = i.indrelid \
                  AND k.contype IN ('p', 'u', 'x'))::text AS keyed, \
       i.indisclustered::text AS clustered, i.indisreplident::text AS replica_identity, \
       i.indisvalid::text AS valid, (c.reltablespace <> 0)::text AS tablespace \
  FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid \
 WHERE i.indrelid >= 16384 \
   AND (i.indisunique OR EXISTS (SELECT FROM pg_catalog.pg_constraint k \
                                  WHERE k.conindid = i.indexrelid AND k.conrelid = i.indrelid \
                                    AND k.contype = 'x')) \
 ORDER BY c.relname";

const SEQUENCES: &str = "\
SELECT s.seqrelid::text AS oid, \
       pg_catalog.quote_ident(n.nspname) || '.' || pg_catalog.quote_ident(c.relname) AS qualified, \
       pg_catalog.format_type(s.seqtypid, NULL) AS spelled, s.seqstart::text AS start, \
       s.seqincrement::text AS increment, s.seqmin::text AS min, s.seqmax::text AS max, \
       s.seqcache::text AS cache, s.seqcycle::text AS cycle, \
       c.relpersistence::text AS persistence, \
       o.refobjid::text AS owner, coalesce(o.refobjsubid, 0)::text AS owner_column, \
       o.deptype::text AS owned \
  FROM pg_catalog.pg_sequence s \
  JOIN pg_catalog.pg_class c ON c.oid = s.seqrelid \
  JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
  LEFT JOIN LATERAL ( \
    SELECT d.refobjid, d.refobjsubid, d.deptype FROM pg_catalog.pg_depend d \
     WHERE d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass AND d.objid = s.seqrelid \
       AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass \
       AND d.deptype IN ('a', 'i') \
     ORDER BY d.deptype LIMIT 1) o ON true \
 WHERE s.seqrelid >= 16384";

const TYPES: &str = "\
SELECT t.oid::text AS oid, t.typtype::text AS kind, \
       pg_catalog.quote_ident(n.nspname) || '.' || pg_catalog.quote_ident(t.typname) AS qualified, \
       t.typrelid::text AS relation, \
       CASE WHEN t.typtype = 'd' THEN pg_catalog.format_type(t.typbasetype, t.typtypmod) END \
         AS base, \
       t.typnotnull::text AS not_null, t.typndims::text AS dimensions, \
       CASE WHEN t.typtype = 'd' AND t.typcollation <> 0 AND t.typcollation <> \
                 (SELECT b.typcollation FROM pg_catalog.pg_type b WHERE b.oid = t.typbasetype) THEN \
         (SELECT pg_catalog.quote_ident(cn.nspname) || '.' || pg_catalog.quote_ident(co.collname) \
            FROM pg_catalog.pg_collation co \
            JOIN pg_catalog.pg_namespace cn ON cn.oid = co.collnamespace \
           WHERE co.oid = t.typcollation) END AS collation \
  FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
 WHERE t.oid >= 16384";

const LABELS: &str = "\
SELECT e.enumtypid::text AS type, pg_catalog.quote_literal(e.enumlabel) AS label \
  FROM pg_catalog.pg_enum e WHERE e.enumtypid >= 16384 \
 ORDER BY e.enumtypid, e.enumsortorder";

const EXTENSIONS: &str = "\
SELECT e.oid::text AS oid, pg_catalog.quote_ident(e.extname) AS name, \
       pg_catalog.quote_ident(n.nspname) AS schema, \
       pg_catalog.quote_literal(e.extversion) AS version \
  FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
 WHERE e.oid >= 16384";

// ---- What the draft writes -------------------------------------------------

/// What a draft or a fill stages: every object outside the managed set
/// that the managed objects reach, with the shapes those need in turn, and
/// which of them form a chain.
struct Survey<'a> {
    folded: Folded<'a>,
    drafted: BTreeSet<Key>,
    chained: BTreeMap<Key, Chain>,
}

/// The survey of `facts`, with `managed` saying which objects the
/// declarations manage and `schemas` the schemas whose every unmanaged
/// object is staged too.
fn survey<'a>(facts: &'a Facts, managed: &dyn Fn(&Key) -> bool, schemas: &[String]) -> Survey<'a> {
    let folded = Folded::new(facts);
    let user =
        |key: &Key| key.0 != "pg_namespace" && facts.objects.contains_key(key) && !managed(key);
    // What the managed objects depend on, through any part: the compile
    // creates them whole on scratch.
    let mut queue: Vec<Key> = Vec::new();
    for (dependent, referenced) in folded.edges.iter().flat_map(|(dependent, edges)| {
        edges
            .iter()
            .map(move |(referenced, _)| (dependent, referenced))
    }) {
        if managed(dependent) && user(referenced) {
            queue.push(referenced.clone());
        }
    }
    for key in facts.objects.keys() {
        let object = &facts.objects[key];
        if schemas
            .iter()
            .any(|schema| unquoted(&object.schema) == *schema)
        {
            let top = folded.top(key);
            if user(&top) {
                queue.push(top);
            }
        }
    }
    // The shapes they need in turn: an external object is staged as its
    // shape, so only what the comparison reads of it matters.
    let mut drafted = BTreeSet::new();
    let mut chained: BTreeMap<Key, Chain> = BTreeMap::new();
    while let Some(key) = queue.pop() {
        if !drafted.insert(key.clone()) {
            continue;
        }
        for referenced in folded.shapes(&key) {
            if managed(referenced) {
                chained
                    .entry(key.clone())
                    .or_insert_with(|| Chain::Names(referenced.clone()));
            } else if user(referenced) {
                queue.push(referenced.clone());
            }
        }
    }
    // A foreign table cannot be staged at all; an object whose shape names
    // one that cannot be staged cannot be either.
    for key in &drafted {
        if key.0 == "pg_class"
            && facts
                .relations
                .get(&key.1)
                .is_some_and(|relation| relation.kind == 'f')
        {
            chained.insert(key.clone(), Chain::Foreign);
        }
    }
    loop {
        let through: Vec<(Key, Key)> = drafted
            .iter()
            .filter(|key| !chained.contains_key(*key))
            .filter_map(|key| {
                folded
                    .shapes(key)
                    .find(|referenced| chained.contains_key(*referenced))
                    .map(|referenced| (key.clone(), referenced.clone()))
            })
            .collect();
        if through.is_empty() {
            break;
        }
        for (key, referenced) in through {
            chained.insert(key, Chain::Through(referenced));
        }
    }
    Survey {
        folded,
        drafted,
        chained,
    }
}

/// The draft for `facts`, with `managed` saying which objects the
/// declarations manage.
fn compose(facts: &Facts, managed: &dyn Fn(&Key) -> bool, schemas: &[String]) -> Draft {
    let Survey {
        folded,
        drafted,
        chained,
    } = survey(facts, managed, schemas);
    let described = |key: &Key| {
        facts
            .objects
            .get(key)
            .map_or_else(|| format!("{} {}", key.0, key.1), |o| o.described.clone())
    };
    let items: BTreeMap<Key, Item> = drafted
        .iter()
        .map(|key| {
            let item = match chained.get(key) {
                Some(Chain::Names(named)) => Item::listed(
                    key,
                    format!(
                        "{} cannot be staged: its shape names the managed {}, which no \
                         externals file can stage before the managed objects exist; adopt {} \
                         into the managed set too, or select no resolver and keep ADR-0013's \
                         conservative rebuild",
                        described(key),
                        described(named),
                        described(key)
                    ),
                ),
                Some(Chain::Foreign) => Item::listed(
                    key,
                    format!(
                        "{} cannot be staged: the comparison does not qualify a foreign \
                         table's properties",
                        described(key)
                    ),
                ),
                Some(Chain::Through(other)) => Item::listed(
                    key,
                    format!(
                        "{} cannot be staged: its shape names {}, which cannot be staged \
                         either",
                        described(key),
                        described(other)
                    ),
                ),
                None => item(facts, key, &described(key)),
            };
            (key.clone(), item)
        })
        .collect();
    let order = order(&folded, &items, &described);
    let mut schemas_written = BTreeSet::new();
    for key in &order {
        if let Some(object) = facts.objects.get(key)
            && !object.schema.is_empty()
            && object.schema != "pg_catalog"
        {
            schemas_written.insert(object.schema.clone());
        }
        if let Some(extension) = (key.0 == "pg_extension")
            .then(|| facts.extensions.get(&key.1))
            .flatten()
        {
            schemas_written.insert(extension.schema.clone());
        }
    }
    let mut sql = String::from(HEADER);
    if !schemas_written.is_empty() {
        sql.push('\n');
        for schema in &schemas_written {
            sql.push_str(&format!("CREATE SCHEMA IF NOT EXISTS {schema};\n"));
        }
    }
    let mut listed = Vec::new();
    for key in &order {
        let item = &items[key];
        sql.push('\n');
        match &item.body {
            Body::Sql(statements) => sql.push_str(statements),
            Body::Listed(line) => {
                sql.push_str(&comment(line));
                listed.push(line.clone());
            }
        }
    }
    if order.is_empty() {
        sql.push_str("\n-- The managed objects depend on no object outside the managed set.\n");
    }
    Draft { sql, listed }
}

// ---- What fill stages -------------------------------------------------------

/// The shapes a resolver run fills on scratch (#1778, DEC-1778.2): every
/// object a draft would stage, read from the target at plan time, in the
/// order that creates what a shape names before the shape. What the
/// externals file created is left to it ([`Fill::staging`]).
#[derive(Debug, Clone, Default)]
pub struct Fill {
    objects: Vec<Filled>,
}

#[derive(Debug, Clone)]
struct Filled {
    catalog: String,
    described: String,
    /// Its carried parts (an owned sequence, an index), by catalog and
    /// description: an externals file that created one of them holds the
    /// object, and fill does not create it again.
    parts: Vec<(String, String)>,
    /// The schemas its SQL writes into, quoted.
    schemas: Vec<String>,
    how: How,
    /// What its shape names among the objects before it, by index.
    names: Vec<usize>,
    /// The managed relations its shape names, by schema and name: a chained
    /// shape is created once the last of them exists.
    after: BTreeSet<(String, String)>,
}

#[derive(Debug, Clone)]
enum How {
    /// Created after the externals file, before any managed object.
    Ahead(String),
    /// Its shape names a managed object, so it is created once the managed
    /// tables exist and before anything else managed compiles.
    Chained(String),
    /// Not filled, and why.
    Not(String),
}

/// What fill sends, once the externals file has run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Staging {
    /// Run after the externals file, in the session that ran it: each
    /// schema it writes into, then every object no managed object must
    /// precede.
    pub ahead: String,
    /// Run once the managed relations in `chained_after` exist (empty for
    /// none).
    pub chained: String,
    /// The managed relations, by schema and name, the chained shapes name.
    pub chained_after: BTreeSet<(String, String)>,
    /// What each chained object is, as `pg_describe_object` names it.
    pub chained_objects: BTreeSet<String>,
    /// What each filled object is, as `pg_describe_object` names it.
    pub filled: BTreeSet<String>,
    /// One finding for each object fill cannot stage, for a compile that
    /// fails for want of one.
    pub unfillable: Vec<String>,
    /// Each chained object, in order: its description, its SQL, and the
    /// managed relations it waits for, its own and those of the chained
    /// objects it names.
    links: Vec<Link>,
}

/// A chained object: its description, its SQL, and the managed relations
/// it waits for.
type Link = (String, String, BTreeSet<(String, String)>);

impl Staging {
    /// Keeps the chained objects whose managed relations are all
    /// `available` (the scratch compile creates each). One that waits for a
    /// relation it does not create, such as a base table the plan renames,
    /// is not filled and is named in the findings; what names it waits for
    /// that relation too, so it goes with it. The rest still are (#1778).
    pub fn keep_chained(&mut self, available: impl Fn(&BTreeSet<(String, String)>) -> bool) {
        let (kept, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut self.links)
            .into_iter()
            .partition(|(_, _, after)| available(after));
        for (described, _, _) in gone {
            self.filled.remove(&described);
            self.chained_objects.remove(&described);
            self.unfillable.push(format!(
                "{described} is not filled: its shape names a managed relation the scratch \
                 compile does not create"
            ));
        }
        self.chained = kept.iter().map(|(_, sql, _)| sql.as_str()).collect();
        self.chained_after = kept
            .iter()
            .flat_map(|(_, _, after)| after.iter().cloned())
            .collect();
        self.links = kept;
    }
}

impl Fill {
    /// What fill sends once the externals file created `created`, each
    /// object by catalog and `pg_describe_object` description, which names
    /// an object alike on the target and on scratch under the empty path
    /// both read with. The externals file wins on overlap; an object whose
    /// shape names one fill cannot stage is not filled either.
    pub fn staging(&self, created: &BTreeSet<(String, String)>) -> Staging {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum State {
            Created,
            Ahead,
            Chained,
            Missing,
        }
        let mut states = Vec::with_capacity(self.objects.len());
        let mut staging = Staging::default();
        let mut ahead = Vec::new();
        let mut chained = Vec::new();
        let mut schemas = BTreeSet::new();
        let mut afters: Vec<BTreeSet<(String, String)>> = Vec::with_capacity(self.objects.len());
        for object in &self.objects {
            let mut after = object.after.clone();
            for &index in &object.names {
                after.extend(afters[index].iter().cloned());
            }
            let part = object.parts.iter().find(|part| created.contains(*part));
            let state = if created.contains(&(object.catalog.clone(), object.described.clone())) {
                State::Created
            } else if let Some((_, part)) = part {
                // Neither the file's nor fill's: what names it cannot be
                // filled either.
                staging.unfillable.push(format!(
                    "{} is not filled: the externals file created its {part}; write the whole \
                     of it in the externals file if a binding needs it",
                    object.described
                ));
                State::Missing
            } else if let Some(&missing) = object
                .names
                .iter()
                .find(|&&index| states[index] == State::Missing)
            {
                staging.unfillable.push(format!(
                    "{} cannot be filled: its shape names {}, which cannot be filled either",
                    object.described, self.objects[missing].described
                ));
                State::Missing
            } else {
                match &object.how {
                    How::Not(why) => {
                        staging.unfillable.push(format!(
                            "{} cannot be filled: {why}; write it in the externals file, or \
                             select no resolver and keep ADR-0013's conservative rebuild",
                            object.described
                        ));
                        State::Missing
                    }
                    How::Chained(sql) => {
                        chained.push(sql.as_str());
                        State::Chained
                    }
                    How::Ahead(sql)
                        if object
                            .names
                            .iter()
                            .any(|&index| states[index] == State::Chained) =>
                    {
                        chained.push(sql.as_str());
                        State::Chained
                    }
                    How::Ahead(sql) => {
                        ahead.push(sql.as_str());
                        State::Ahead
                    }
                }
            };
            if matches!(state, State::Ahead | State::Chained) {
                staging.filled.insert(object.described.clone());
                schemas.extend(object.schemas.iter().cloned());
            }
            if state == State::Chained {
                staging.chained_objects.insert(object.described.clone());
                staging.chained_after.extend(after.iter().cloned());
                if let How::Ahead(sql) | How::Chained(sql) = &object.how {
                    staging
                        .links
                        .push((object.described.clone(), sql.clone(), after.clone()));
                }
            }
            states.push(state);
            afters.push(after);
        }
        staging.ahead = schemas
            .iter()
            .map(|schema| format!("CREATE SCHEMA IF NOT EXISTS {schema};\n"))
            .chain(ahead.into_iter().map(str::to_owned))
            .collect();
        staging.chained = chained.concat();
        debug_assert_eq!(
            staging.chained,
            staging
                .links
                .iter()
                .map(|(_, sql, _)| sql.as_str())
                .collect::<String>()
        );
        staging
    }
}

/// What fill stages for the managed set of `desired` and `base`, read from
/// the connected target: the objects a draft stages without `--schema`.
/// A declared routine's signature is looked up on the write path with
/// `extras`, as the run's other lookups of the managed routines are.
pub async fn fill(
    conn: &mut impl QueryConnection,
    desired: &pbps_model::Schema,
    base: &pbps_model::Schema,
    extras: &[String],
) -> Result<Fill, Error> {
    let managed = [
        super::capture::Managed::from_schema(desired),
        super::capture::Managed::from_schema(base),
    ];
    let dialect = crate::Postgres::with_write_path_extras(extras.to_vec());
    let mut signatures = Vec::new();
    for schema in [desired, base] {
        for (id, module) in &schema.modules {
            if let Some(signature) =
                super::reconstruct::dropped_signature(&dialect, id, module.kind)
                    .map_err(|error| Error::Declaration(error.to_string()))?
            {
                signatures.push(signature);
            }
        }
    }
    let (routines, facts) = read(conn, &signatures)
        .await
        .map_err(|source| Error::Read {
            what: "the target's catalog",
            source,
        })?;
    let is_managed = |key: &Key| match key.0.as_str() {
        "pg_class" => facts.relations.get(&key.1).is_some_and(|relation| {
            managed
                .iter()
                .any(|managed| managed.relation(&relation.schema, &relation.name))
        }),
        "pg_proc" => routines.contains(&key.1),
        _ => false,
    };
    Ok(survey_fill(&facts, &is_managed))
}

fn survey_fill(facts: &Facts, managed: &dyn Fn(&Key) -> bool) -> Fill {
    let Survey {
        folded,
        drafted,
        chained,
    } = survey(facts, managed, &[]);
    let described = |key: &Key| {
        facts
            .objects
            .get(key)
            .map_or_else(|| format!("{} {}", key.0, key.1), |o| o.described.clone())
    };
    let items: BTreeMap<Key, Item> = drafted
        .iter()
        .map(|key| (key.clone(), item(facts, key, &described(key))))
        .collect();
    let order = order(&folded, &items, &described);
    let index: BTreeMap<&Key, usize> = order.iter().enumerate().map(|(i, k)| (k, i)).collect();
    let objects = order
        .iter()
        .map(|key| {
            let item = &items[key];
            let mut after = BTreeSet::new();
            let how = match (chained.get(key), &item.body) {
                (Some(Chain::Foreign), _) => {
                    How::Not("the comparison does not qualify a foreign table's properties".into())
                }
                (_, Body::Listed(line)) => {
                    How::Not(item.why.clone().unwrap_or_else(|| line.clone()))
                }
                // The chain keeps one managed object it names; the split
                // waits for every one.
                (Some(Chain::Names(_)), Body::Sql(sql)) => {
                    let mut not = None;
                    for named in folded.shapes(key).filter(|referenced| managed(referenced)) {
                        match facts.relations.get(&named.1) {
                            Some(relation) if named.0 == "pg_class" => {
                                after.insert((relation.schema.clone(), relation.name.clone()));
                            }
                            _ => {
                                not.get_or_insert_with(|| {
                                    How::Not(format!(
                                        "its shape names the managed {}, and fill orders a \
                                         shape only after the managed relations it names",
                                        described(named)
                                    ))
                                });
                            }
                        }
                    }
                    not.unwrap_or_else(|| How::Chained(sql.clone()))
                }
                (Some(Chain::Through(_)) | None, Body::Sql(sql)) => How::Ahead(sql.clone()),
            };
            // An extension's members are its parts too.
            let parts = folded
                .carriers
                .keys()
                .chain(folded.members.keys())
                .filter(|part| folded.top(part) == *key)
                .filter_map(|part| {
                    facts
                        .objects
                        .get(*part)
                        .map(|object| (part.0.clone(), object.described.clone()))
                })
                .collect();
            let mut schemas = Vec::new();
            if let Some(object) = facts.objects.get(key)
                && !object.schema.is_empty()
                && object.schema != "pg_catalog"
            {
                schemas.push(object.schema.clone());
            }
            if let Some(extension) = (key.0 == "pg_extension")
                .then(|| facts.extensions.get(&key.1))
                .flatten()
            {
                schemas.push(extension.schema.clone());
            }
            Filled {
                catalog: key.0.clone(),
                described: described(key),
                parts,
                schemas,
                how,
                after,
                names: folded
                    .shapes(key)
                    .filter_map(|referenced| index.get(referenced).copied())
                    .filter(|&i| i < index[key])
                    .collect(),
            }
        })
        .collect();
    Fill { objects }
}

const HEADER: &str = "\
-- An externals file drafted by `pbps resolver externals` from the target
-- (SPEC section 9.3.2). It stages the objects outside the managed set that
-- the managed objects read, as their shapes: a view as a shape view of its
-- output columns, a table without what guards it. Every object it creates
-- is compared with the target. Review it, write what it lists, and name it
-- in a resolver entry's `externals:`.
";

enum Chain {
    /// Its shape names this managed object.
    Names(Key),
    /// Its shape names this object, which cannot be staged.
    Through(Key),
    /// A foreign table, whose properties the comparison does not qualify.
    Foreign,
}

struct Item {
    rank: u8,
    body: Body,
    /// Why it is listed rather than written, as a fill finding says it.
    why: Option<String>,
}

enum Body {
    Sql(String),
    Listed(String),
}

impl Item {
    fn listed(key: &Key, line: String) -> Self {
        Self {
            rank: rank(key),
            body: Body::Listed(line),
            why: None,
        }
    }

    fn because(mut self, why: String) -> Self {
        self.why = Some(why);
        self
    }
}

/// Where an object goes among those no dependency orders: extensions
/// first, then types, sequences, tables and views, then what is listed.
fn rank(key: &Key) -> u8 {
    match key.0.as_str() {
        "pg_extension" => 0,
        "pg_type" => 1,
        "pg_class" => 2,
        "pg_proc" => 4,
        _ => 5,
    }
}

/// Each object in an order that creates what a shape names before the
/// shape; ties by rank and description.
fn order(
    folded: &Folded<'_>,
    items: &BTreeMap<Key, Item>,
    described: &dyn Fn(&Key) -> String,
) -> Vec<Key> {
    let before: BTreeMap<&Key, BTreeSet<&Key>> = items
        .keys()
        .map(|key| {
            (
                key,
                folded
                    .shapes(key)
                    .filter(|referenced| *referenced != key && items.contains_key(*referenced))
                    .collect(),
            )
        })
        .collect();
    let sort_key = |key: &Key| (items[key].rank, described(key), key.clone());
    let mut placed: BTreeSet<&Key> = BTreeSet::new();
    let mut order = Vec::new();
    while placed.len() < items.len() {
        let next = items
            .keys()
            .filter(|key| !placed.contains(key))
            .filter(|key| before[key].iter().all(|other| placed.contains(other)))
            .min_by_key(|key| sort_key(key))
            // A cycle among shapes: the engine allows none, so this is
            // only a guard against looping forever.
            .or_else(|| {
                items
                    .keys()
                    .filter(|key| !placed.contains(key))
                    .min_by_key(|key| sort_key(key))
            })
            .expect("an unplaced item remains");
        placed.insert(next);
        order.push(next.clone());
    }
    order
}

/// The recorded dependencies, each end folded to the object it belongs to:
/// a part to what carries it (`i` and `a`), a member to its extension.
struct Folded<'a> {
    carriers: BTreeMap<&'a Key, &'a Key>,
    members: BTreeMap<&'a Key, &'a Key>,
    /// What each object depends on, with whether through a part the
    /// comparison reads.
    edges: BTreeMap<Key, Vec<(Key, bool)>>,
}

impl<'a> Folded<'a> {
    fn new(facts: &'a Facts) -> Self {
        let mut folded = Self {
            carriers: BTreeMap::new(),
            members: BTreeMap::new(),
            edges: BTreeMap::new(),
        };
        for depend in &facts.depends {
            match depend.deptype {
                'e' => {
                    folded
                        .members
                        .entry(&depend.dependent)
                        .or_insert(&depend.referenced);
                }
                'i' | 'a' => {
                    folded
                        .carriers
                        .entry(&depend.dependent)
                        .or_insert(&depend.referenced);
                }
                _ => {}
            }
        }
        let mut edges: BTreeMap<Key, BTreeMap<Key, bool>> = BTreeMap::new();
        for depend in facts.depends.iter().filter(|d| d.deptype == 'n') {
            let dependent = folded.top(&depend.dependent);
            let referenced = folded.top(&depend.referenced);
            if dependent != referenced {
                *edges
                    .entry(dependent)
                    .or_default()
                    .entry(referenced)
                    .or_default() |= depend.shape;
            }
        }
        folded.edges = edges
            .into_iter()
            .map(|(key, edges)| (key, edges.into_iter().collect()))
            .collect();
        folded
    }

    /// The object `key` belongs to. A bounded walk: a carrier chain is a
    /// few links long (an index, its constraint, its table).
    fn top(&self, key: &Key) -> Key {
        let mut key = key;
        for _ in 0..16 {
            if let Some(extension) = self.members.get(key) {
                return (*extension).clone();
            }
            match self.carriers.get(key) {
                Some(carrier) => key = carrier,
                None => break,
            }
        }
        key.clone()
    }

    /// What `key`'s shape depends on.
    fn shapes(&self, key: &Key) -> impl Iterator<Item = &Key> {
        self.edges
            .get(key)
            .into_iter()
            .flatten()
            .filter(|(_, shape)| *shape)
            .map(|(referenced, _)| referenced)
    }
}

/// What the draft writes for one object it stages.
fn item(facts: &Facts, key: &Key, described: &str) -> Item {
    let by_hand = |what: &str| {
        Item::listed(
            key,
            format!(
                "{described}: write it here by hand if a binding needs it; the draft writes no {what}"
            ),
        )
        .because(format!("pbps writes no {what}"))
    };
    let body = match key.0.as_str() {
        "pg_extension" => facts.extensions.get(&key.1).map(|e| {
            format!(
                "CREATE EXTENSION {} WITH SCHEMA {} VERSION {};\n",
                e.name, e.schema, e.version
            )
        }),
        "pg_proc" => {
            return Item::listed(
                key,
                format!(
                    "{described}: listed, never written; pbps sends no routine source to \
                     scratch, so add its definition here if a binding needs it"
                ),
            )
            .because("pbps sends no routine source to scratch".into());
        }
        "pg_type" => match facts.types.get(&key.1) {
            Some(t) if t.kind == 'e' => Some(format!(
                "CREATE TYPE {} AS ENUM ({});\n",
                t.qualified,
                facts
                    .labels
                    .get(&key.1)
                    .map_or_else(String::new, |l| l.join(", "))
            )),
            Some(t) if t.kind == 'd' => {
                let not_null = match facts.not_nulls.get(&(key.1, 0)) {
                    Some(constraint) => format!(" CONSTRAINT {} NOT NULL", constraint.name),
                    None if t.not_null => " NOT NULL".into(),
                    None => String::new(),
                };
                Some(format!(
                    "CREATE DOMAIN {} AS {}{}{not_null};\n",
                    t.qualified,
                    dimensioned(&t.base, t.dimensions),
                    collate(t.collation.as_deref())
                ))
            }
            Some(t) if t.kind == 'c' => {
                let columns = facts
                    .columns
                    .get(&t.relation)
                    .map_or(&[][..], Vec::as_slice);
                Some(format!(
                    "CREATE TYPE {} AS ({});\n",
                    t.qualified,
                    columns
                        .iter()
                        .map(|c| format!(
                            "{} {}{}",
                            c.name,
                            dimensioned(&c.spelled, c.dimensions),
                            collate(c.collation.as_deref())
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
            Some(t) => {
                return by_hand(match t.kind {
                    'r' => "range type",
                    'm' => "multirange type",
                    _ => "base type",
                });
            }
            None => None,
        },
        "pg_class" => match facts.relations.get(&key.1) {
            Some(relation) => match relation.kind {
                'r' => match table(facts, key.1, relation) {
                    Ok(sql) => Some(sql),
                    Err(what) => return by_hand(what),
                },
                'v' | 'm' => match shape_view(facts, key.1, relation) {
                    Ok(sql) => Some(sql),
                    Err(what) => return by_hand(what),
                },
                'S' => facts
                    .sequences
                    .get(&key.1)
                    .map(|sequence| sequence_sql(sequence, None)),
                'p' => return by_hand("partitioned table"),
                _ => None,
            },
            None => None,
        },
        "pg_cast" => return by_hand("cast"),
        "pg_operator" => return by_hand("operator"),
        "pg_collation" => return by_hand("collation"),
        "pg_opclass" | "pg_opfamily" => return by_hand("operator class"),
        _ => None,
    };
    match body {
        Some(sql) => Item {
            rank: match facts.relations.get(&key.1).map(|r| r.kind) {
                Some('v' | 'm') if key.0 == "pg_class" => 3,
                _ => rank(key),
            },
            body: Body::Sql(sql),
            why: None,
        },
        None => by_hand("object of this class"),
    }
}

/// A table as the comparison reads it: its columns in order with their
/// types, collations and NOT NULLs, its keys and unique indexes, its owned
/// sequences, and its own options. Nothing that guards it (#1783). `Err`
/// names a structure the draft does not write.
fn table(facts: &Facts, oid: u32, relation: &Relation) -> Result<String, &'static str> {
    if !relation.structure.is_empty() {
        return Err(match relation.structure.as_str() {
            "a partition" => "partition",
            "a typed table" => "typed table",
            "inheritance" => "inheritance",
            _ => "partitioned table",
        });
    }
    if relation.tablespace {
        return Err("tablespace");
    }
    if let Some(what) = unreproducible_index(facts, oid) {
        return Err(what);
    }
    let columns = facts.columns.get(&oid).map_or(&[][..], Vec::as_slice);
    let owned: Vec<&Sequence> = facts
        .sequences
        .values()
        .filter(|s| s.owner.is_some_and(|(owner, _, _)| owner == oid))
        .collect();
    let mut definitions = Vec::new();
    let mut after = Vec::new();
    for column in columns {
        if column.generated {
            return Err("generated column");
        }
        if column.inherited {
            return Err("inheritance");
        }
        let mut definition = format!(
            "    {} {}{}",
            column.name,
            dimensioned(&column.spelled, column.dimensions),
            collate(column.collation.as_deref())
        );
        if let Some(how) = column.identity {
            let sequence = owned
                .iter()
                .find(|s| s.owner == Some((oid, column.number, 'i')))
                .ok_or("identity column without its sequence")?;
            let always = if how == 'a' { "ALWAYS" } else { "BY DEFAULT" };
            if sequence.spelled == column.spelled {
                definition.push_str(&format!(
                    " GENERATED {always} AS IDENTITY (SEQUENCE NAME {} {})",
                    sequence.qualified,
                    sequence_options(sequence)
                ));
            } else {
                // Its own type, set apart from the column's (`ALTER
                // SEQUENCE ... AS`): identity options refuse `AS`, and its
                // bounds may not fit the column's type until it has its own
                // (measured on 16 and 18).
                definition.push_str(&format!(
                    " GENERATED {always} AS IDENTITY (SEQUENCE NAME {})",
                    sequence.qualified
                ));
                after.push(format!(
                    "ALTER SEQUENCE {} AS {} {};\n",
                    sequence.qualified,
                    sequence.spelled,
                    sequence_options(sequence)
                ));
            }
            // An identity sequence takes its table's persistence unless it
            // was set apart (`ALTER SEQUENCE ... SET UNLOGGED`, measured on
            // 16 and 18).
            if sequence.unlogged != relation.unlogged {
                after.push(format!(
                    "ALTER SEQUENCE {} SET {}LOGGED;\n",
                    sequence.qualified,
                    if sequence.unlogged { "UN" } else { "" }
                ));
            }
        }
        match facts.not_nulls.get(&(oid, column.number)) {
            Some(constraint) if !constraint.plain => {
                return Err("NOT NULL constraint marked NOT VALID or NO INHERIT");
            }
            Some(constraint) => {
                definition.push_str(&format!(" CONSTRAINT {} NOT NULL", constraint.name));
            }
            None if column.not_null => definition.push_str(" NOT NULL"),
            None => {}
        }
        definitions.push(definition);
        after.push(column_settings(relation, column));
    }
    let mut sql = format!(
        "CREATE {}TABLE {} (\n{}\n){}{};\n",
        if relation.unlogged { "UNLOGGED " } else { "" },
        relation.qualified,
        definitions.join(",\n"),
        using(&relation.method),
        with(&relation.options)
    );
    for keyed in facts.keys.get(&oid).into_iter().flatten() {
        sql.push_str(&format!(
            "ALTER TABLE {} ADD CONSTRAINT {} {};\n",
            relation.qualified, keyed.name, keyed.definition
        ));
    }
    sql.push_str(&indexes(facts, oid, relation));
    for sequence in owned
        .iter()
        .filter(|s| s.owner.is_some_and(|(_, _, how)| how == 'a'))
    {
        let column = columns
            .iter()
            .find(|c| {
                sequence
                    .owner
                    .is_some_and(|(_, number, _)| number == c.number)
            })
            .map(|c| format!("{}.{}", relation.qualified, c.name));
        sql.push_str(&sequence_sql(sequence, column.as_deref()));
    }
    for statement in after {
        sql.push_str(&statement);
    }
    match relation.replica_identity {
        'f' => sql.push_str(&format!(
            "ALTER TABLE {} REPLICA IDENTITY FULL;\n",
            relation.qualified
        )),
        'n' => sql.push_str(&format!(
            "ALTER TABLE {} REPLICA IDENTITY NOTHING;\n",
            relation.qualified
        )),
        _ => {}
    }
    if relation.row_security {
        sql.push_str(&format!(
            "ALTER TABLE {} ENABLE ROW LEVEL SECURITY;\n",
            relation.qualified
        ));
    }
    if relation.forced_row_security {
        sql.push_str(&format!(
            "ALTER TABLE {} FORCE ROW LEVEL SECURITY;\n",
            relation.qualified
        ));
    }
    Ok(sql)
}

/// A view or materialized view as a shape view: its output columns over
/// typed NULLs, returning no row (SPEC §9.3.2). Its query is not compared.
fn shape_view(facts: &Facts, oid: u32, relation: &Relation) -> Result<String, &'static str> {
    if relation.options.contains("check_option") {
        return Err("view WITH CHECK OPTION, which a shape view cannot carry,");
    }
    if relation.tablespace {
        return Err("tablespace");
    }
    if let Some(what) = unreproducible_index(facts, oid) {
        return Err(what);
    }
    let columns = facts.columns.get(&oid).map_or(&[][..], Vec::as_slice);
    let select = columns
        .iter()
        .map(|c| {
            format!(
                "NULL::{}{} AS {}",
                c.spelled,
                collate(c.collation.as_deref()),
                c.name
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let select = if select.is_empty() {
        "SELECT WHERE false".to_owned()
    } else {
        format!("SELECT {select} WHERE false")
    };
    Ok(if relation.kind == 'm' {
        format!(
            "CREATE MATERIALIZED VIEW {}{}{} AS {select} WITH {}DATA;\n{}{}",
            relation.qualified,
            using(&relation.method),
            with(&relation.options),
            if relation.populated { "" } else { "NO " },
            indexes(facts, oid, relation),
            columns
                .iter()
                .map(|column| column_settings(relation, column))
                .collect::<String>()
        )
    } else {
        format!(
            "CREATE VIEW {}{} AS {select};\n",
            relation.qualified,
            with(&relation.options)
        )
    })
}

/// What of a compared index of the relation no statement the draft writes
/// reproduces: an invalid one, which a statement builds valid, or one in a
/// tablespace of its own, which scratch need not have.
fn unreproducible_index(facts: &Facts, oid: u32) -> Option<&'static str> {
    let indexes = facts.indexes.get(&oid).map_or(&[][..], Vec::as_slice);
    if indexes.iter().any(|index| !index.valid) {
        Some("invalid unique index")
    } else if indexes.iter().any(|index| index.tablespace) {
        Some("tablespace")
    } else {
        None
    }
}

/// The unique indexes no key owns, and what the relation says of any
/// compared index: clustered on it, or its replica identity.
fn indexes(facts: &Facts, oid: u32, relation: &Relation) -> String {
    let mut sql = String::new();
    for index in facts.indexes.get(&oid).into_iter().flatten() {
        if !index.keyed {
            sql.push_str(&format!("{};\n", index.definition));
        }
        if index.clustered {
            sql.push_str(&format!(
                "ALTER {} {} CLUSTER ON {};\n",
                altered(relation),
                relation.qualified,
                index.name
            ));
        }
        if index.replica_identity {
            sql.push_str(&format!(
                "ALTER TABLE {} REPLICA IDENTITY USING INDEX {};\n",
                relation.qualified, index.name
            ));
        }
    }
    sql
}

/// What a table's or materialized view's column says beyond its type: its
/// storage, compression and options, each compared (#1783).
fn column_settings(relation: &Relation, column: &Column) -> String {
    let alter = format!(
        "ALTER {} {} ALTER COLUMN {}",
        altered(relation),
        relation.qualified,
        column.name
    );
    let mut sql = String::new();
    if let Some(storage) = column.storage {
        sql.push_str(&format!(
            "{alter} SET STORAGE {};\n",
            match storage {
                'p' => "PLAIN",
                'e' => "EXTERNAL",
                'm' => "MAIN",
                _ => "EXTENDED",
            }
        ));
    }
    if let Some(compression) = column.compression {
        sql.push_str(&format!(
            "{alter} SET COMPRESSION {};\n",
            if compression == 'l' { "lz4" } else { "pglz" }
        ));
    }
    if !column.options.is_empty() {
        sql.push_str(&format!("{alter} SET ({});\n", column.options));
    }
    sql
}

/// The `ALTER` a relation of this kind takes: `ALTER TABLE` refuses a
/// materialized view.
fn altered(relation: &Relation) -> &'static str {
    if relation.kind == 'm' {
        "MATERIALIZED VIEW"
    } else {
        "TABLE"
    }
}

/// A type as `format_type` spells it, with the array dimensions it was
/// declared with: `format_type` writes one `[]` however many there are, and
/// the comparison reads the declared count.
fn dimensioned(spelled: &str, dimensions: i32) -> String {
    format!(
        "{spelled}{}",
        "[]".repeat(usize::try_from(dimensions - 1).unwrap_or(0))
    )
}

fn sequence_options(sequence: &Sequence) -> String {
    format!(
        "START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} CACHE {} {}CYCLE",
        sequence.start,
        sequence.increment,
        sequence.min,
        sequence.max,
        sequence.cache,
        if sequence.cycle { "" } else { "NO " }
    )
}

fn sequence_sql(sequence: &Sequence, owned_by: Option<&str>) -> String {
    format!(
        "CREATE {}SEQUENCE {} AS {} {}{};\n",
        if sequence.unlogged { "UNLOGGED " } else { "" },
        sequence.qualified,
        sequence.spelled,
        sequence_options(sequence),
        owned_by.map_or_else(String::new, |column| format!(" OWNED BY {column}"))
    )
}

fn collate(collation: Option<&str>) -> String {
    collation.map_or_else(String::new, |c| format!(" COLLATE {c}"))
}

fn using(method: &str) -> String {
    if method.is_empty() {
        String::new()
    } else {
        format!(" USING {method}")
    }
}

fn with(options: &str) -> String {
    if options.is_empty() {
        String::new()
    } else {
        format!(" WITH ({options})")
    }
}

/// `line` as SQL comment lines of at most 78 characters.
fn comment(line: &str) -> String {
    let mut out = String::new();
    let mut current = String::from("--");
    for word in line.split_whitespace() {
        if current.len() + 1 + word.len() > 78 && current != "--" {
            out.push_str(&current);
            out.push('\n');
            current = String::from("--");
        }
        current.push(' ');
        current.push_str(word);
    }
    out.push_str(&current);
    out.push('\n');
    out
}

/// A schema name as `quote_ident` spells it, unquoted again for comparing
/// with a name the operator typed.
fn unquoted(quoted: &str) -> String {
    quoted
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .map_or_else(|| quoted.to_owned(), |inner| inner.replace("\"\"", "\""))
}

fn text(row: &Row, field: &str) -> Result<String, DbError> {
    row.try_get::<&str>(field)?
        .map(str::to_owned)
        .ok_or_else(|| DbError::BadRow(format!("{field} was NULL")))
}

fn optional(row: &Row, field: &str) -> Result<Option<String>, DbError> {
    Ok(row.try_get::<&str>(field)?.map(str::to_owned))
}

fn number(row: &Row, field: &str) -> Result<u32, DbError> {
    text(row, field)?
        .parse()
        .map_err(|_| DbError::BadRow(format!("{field} was not an OID")))
}

fn signed(row: &Row, field: &str) -> Result<i32, DbError> {
    text(row, field)?
        .parse()
        .map_err(|_| DbError::BadRow(format!("{field} was not a number")))
}

fn flag(row: &Row, field: &str) -> Result<bool, DbError> {
    Ok(text(row, field)? == "true")
}

/// A `"char"` column, `None` when it holds the empty value.
fn character(row: &Row, field: &str) -> Result<Option<char>, DbError> {
    Ok(optional(row, field)?.and_then(|value| value.chars().next()))
}

#[cfg(test)]
mod tests;
