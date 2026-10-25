//! Complete catalog-local evidence, without scheduling or granting rebuild admission.
use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError, Param};
use pbps_diff::{
    Side,
    order::catalog::{Holds, Part, graph::*},
};
use pbps_model::operation::{MembershipCatalog, Snapshot, StoredColumn};
use pbps_model::{ModuleId, ObjectName, TableName, Uid, escape::Escaped};
use serde::Deserialize;

fn object(catalog: &str, id: u64, attribute: i32) -> Object {
    Object {
        catalog: catalog.into(),
        id,
        attribute,
    }
}
fn relation(id: u64) -> Object {
    object("pg_class", id, 0)
}
fn ty(id: u64) -> Object {
    object("pg_type", id, 0)
}

#[derive(Deserialize)]
struct Meta {
    relkind: Option<String>,
    constraint_kind: Option<String>,
    described: String,
    kind: String,
    schema: String,
    name: String,
    part: String,
    owner_class: String,
    owner: u64,
    owner_attribute: i32,
    args: Vec<String>,
    argument_types: Vec<u64>,
    result: u64,
    row_type: u64,
    array_type: u64,
    attributes: Vec<(i32, String, u64)>,
    valid: bool,
    table_id: u64,
    index: u64,
    columns: Vec<i32>,
    referenced: u64,
    referenced_columns: Vec<i32>,
}

/// Reads must share the caller's transaction. Missing roots, failed reads and
/// unknown catalog classes are never successful empty dependency evidence.
/// Publication facts are separate from pg_depend: its rows do not cover all modes.
pub async fn collect(conn: &mut Conn, root: &Root, source: Side<'_>) -> Result<Catalog, DbError> {
    let label = match root {
        Root::Module(id) => id.shown().to_string(),
        Root::Table(uid) => source
            .ids
            .tables
            .get(uid)
            .map_or_else(|| uid.to_string(), |name| name.shown().to_string()),
    };
    collect_in(conn, root, source)
        .await
        .map_err(|error| match &error {
            DbError::Driver { code, .. } => {
                let message = format!(
                    "{label}: dependency catalog is unreadable; SQLSTATE {}",
                    code.as_deref().unwrap_or("unavailable")
                );
                error.context(message)
            }
            DbError::BadConnectionString(_)
            | DbError::Connect { .. }
            | DbError::ConnectTimeout { .. }
            | DbError::Context { .. }
            | DbError::Refused(_)
            | DbError::WrongSession { .. }
            | DbError::UnsupportedServer { .. } => error,
            DbError::BadRow(_) => error.context(format!(
                "{label}: dependency catalog metadata is unreadable"
            )),
        })
}

async fn collect_in(conn: &mut Conn, root: &Root, source: Side<'_>) -> Result<Catalog, DbError> {
    super::require_the_callers_transaction(conn).await?;
    conn.query(super::CANONICAL_PATH).await?;
    let start = match root {
        Root::Module(id) => {
            let module = source
                .schema
                .modules
                .get(id)
                .ok_or_else(|| super::not_in_the_catalog(id))?;
            let oid = super::module_oid(conn, id, module.kind)
                .await?
                .ok_or_else(|| super::not_in_the_catalog(id))?;
            object(
                match id {
                    ModuleId::Named(_) => "pg_class",
                    ModuleId::Routine(_) => "pg_proc",
                    ModuleId::Trigger { .. } => "pg_trigger",
                },
                oid as u64,
                0,
            )
        }
        Root::Table(uid) => {
            let name = source
                .ids
                .tables
                .get(uid)
                .filter(|_| uid.kind() == pbps_model::UidKind::Table)
                .ok_or_else(|| DbError::Refused(format!("{uid}: no source table identity")))?;
            let rows = conn.query_with("SELECT c.oid::int8 AS id FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname=$2 AND c.relkind IN ('r','p')", &[Param::Str(&name.schema), Param::Str(&name.name)]).await?;
            let id = rows
                .first()
                .map(|r| r.try_get::<i64>("id"))
                .transpose()?
                .flatten()
                .ok_or_else(|| {
                    DbError::Refused(format!("{}: source tree is absent", name.shown()))
                })?;
            relation(id as u64)
        }
    };
    read(conn, root, &start, source).await
}

async fn read(
    conn: &mut Conn,
    root: &Root,
    start: &Object,
    source: Side<'_>,
) -> Result<Catalog, DbError> {
    let mut out = Catalog::default();
    out.graph
        .roots
        .insert(root.clone(), Fact::Present(start.clone()));
    let mut pending = vec![(start.clone(), true)];
    let mut complete = BTreeSet::new();
    let mut metadata = BTreeMap::new();
    let mut spellings = BTreeMap::new();
    while let Some((id, walk)) = pending.pop() {
        if complete.contains(&id) || (!walk && metadata.contains_key(&id)) {
            continue;
        }
        let rows = conn
            .query_with(
                METADATA,
                &[
                    Param::Str(&id.catalog),
                    Param::I64(id.id as i64),
                    Param::I32(id.attribute),
                ],
            )
            .await?;
        if rows.len() != 1 {
            return Err(DbError::Refused(format!(
                "{id:?}: missing or ambiguous catalog identity"
            )));
        }
        let json = rows
            .first()
            .and_then(|r| r.try_get::<&str>("meta").transpose())
            .transpose()?
            .ok_or_else(|| super::vanished(id.id as i64))?;
        let m: Meta = serde_json::from_str(json)
            .map_err(|e| DbError::BadRow(format!("dependency metadata: {e}")))?;
        if !m.valid {
            return Err(super::vanished(id.id as i64));
        }
        let table = TableName::new(&m.schema, &m.name);
        let held = if &id == start {
            Held::Root(root.clone())
        } else if m.owner != 0 {
            let kind = match m.kind.as_str() {
                "attribute" => Ownership::Attribute,
                "row_type" => Ownership::Composite,
                "array_type" => Ownership::Array,
                "return_rule" => Ownership::ReturnRule,
                "key_index" => Ownership::KeyIndex,
                "constraint_trigger" => Ownership::ConstraintTrigger,
                "not_null" => Ownership::Attribute,
                "storage" => Ownership::Storage,
                "clone" => Ownership::PartitionClone,
                _ => {
                    return Err(DbError::BadRow(format!(
                        "{}: unknown ownership {}",
                        Escaped(&m.described),
                        Escaped(&m.kind)
                    )));
                }
            };
            let by = object(&m.owner_class, m.owner, m.owner_attribute);
            pending.push((by.clone(), walk));
            Held::Owned { by, kind }
        } else {
            let holds = match m.kind.as_str() {
                "view" => Holds::Module(ModuleId::Named(ObjectName::new(&m.schema, &m.name))),
                "routine" => match m
                    .args
                    .iter()
                    .map(|arg| arg.parse())
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(args) => Holds::Module(ModuleId::Routine(pbps_model::RoutineId::new(
                        ObjectName::new(&m.schema, &m.name),
                        args,
                    ))),
                    Err(_) => Holds::Unrepresentable(
                        "routine identity argument is not representable".into(),
                    ),
                },
                "trigger" => Holds::Module(ModuleId::Trigger {
                    on: ObjectName::new(&m.schema, &m.name),
                    name: m.part.clone(),
                }),
                // Binding-only keys retain their physical metadata in `out.keys`,
                // but supply no teardown definition (DEC-2298.5). A later walk
                // of this node must still require its source declaration.
                "primary" if !walk => {
                    Holds::Unrepresentable("referenced primary key is binding evidence only".into())
                }
                "primary" => {
                    let key = source
                        .schema
                        .tables
                        .get(&table)
                        .and_then(|t| t.primary_key.clone())
                        .ok_or_else(|| {
                            DbError::Refused(format!(
                                "{}: source primary key is unmanaged",
                                Escaped(&m.described)
                            ))
                        })?;
                    Holds::TablePart {
                        table: table.clone(),
                        part: Part::PrimaryKey(pbps_model::PrimaryKey {
                            name: Some(m.part.clone().into()),
                            ..key
                        }),
                    }
                }
                "unique" | "foreign" | "check" | "index" | "default" | "generated" | "policy" => {
                    Holds::TablePart {
                        table: table.clone(),
                        part: match m.kind.as_str() {
                            "unique" => Part::Unique(m.part.clone().into()),
                            "foreign" => Part::ForeignKey(m.part.clone().into()),
                            "check" => Part::Check(m.part.clone().into()),
                            "index" => Part::Index(m.part.clone().into()),
                            "default" => Part::Default(m.part.clone().into()),
                            "generated" => Part::Generated(m.part.clone().into()),
                            "policy" => Part::Policy(m.part.clone()),
                            _ => unreachable!("enumerated table parts"),
                        },
                    }
                }
                _ => {
                    Holds::Unrepresentable(format!("unsupported catalog kind {}", Escaped(&m.kind)))
                }
            };
            Held::Dependent(holds)
        };
        let index = m.relkind.as_deref().is_some_and(|k| matches!(k, "i" | "I"));
        if (id.catalog == "pg_class" && id.attribute == 0 && index)
            || (id.catalog == "pg_constraint"
                && m.constraint_kind
                    .as_deref()
                    .is_some_and(|k| matches!(k, "p" | "u" | "f" | "c")))
        {
            spellings.insert(
                id.clone(),
                PartSpelling {
                    table: table.clone(),
                    name: m.part.clone().into(),
                    kind: if index {
                        pbps_model::operation::NameKind::Index
                    } else {
                        pbps_model::operation::NameKind::Constraint
                    },
                    index_attributes: index.then(|| {
                        m.attributes
                            .iter()
                            .map(|(_, n, _)| n.clone().into())
                            .collect()
                    }),
                },
            );
        }
        if id.catalog == "pg_class" && id.attribute == 0 && m.row_type != 0 {
            if walk && m.kind == "clone" && !source.schema.tables.contains_key(&table) {
                return Err(DbError::Refused(format!(
                    "{}: unmanaged partition",
                    Escaped(&m.described)
                )));
            }
            out.relations.insert(
                id.clone(),
                Relation {
                    name: table,
                    row_type: ty(m.row_type),
                    array_type: ty(m.array_type),
                    attributes: m
                        .attributes
                        .iter()
                        .map(|(n, name, t)| (*n, (name.clone().into(), ty(*t))))
                        .collect(),
                },
            );
        }
        if id.catalog == "pg_constraint" && m.index != 0 {
            out.keys.insert(
                id.clone(),
                Key {
                    index: relation(m.index),
                    columns: m
                        .columns
                        .iter()
                        .map(|n| object("pg_class", m.table_id, *n))
                        .collect(),
                    referenced_columns: m
                        .referenced_columns
                        .iter()
                        .map(|n| object("pg_class", m.referenced, *n))
                        .collect(),
                },
            );
            // An external candidate key is evidence, not a reason to traverse all its users.
            if m.referenced != 0 {
                pending.push((relation(m.index), false));
            }
        }
        if id.catalog == "pg_proc" {
            out.routines.insert(
                id.clone(),
                (
                    m.argument_types.iter().copied().map(ty).collect(),
                    ty(m.result),
                ),
            );
        }
        let mut links = BTreeSet::new();
        for row in if walk {
            conn.query_with(
                EDGES,
                &[
                    Param::Str(&id.catalog),
                    Param::I64(id.id as i64),
                    Param::I32(id.attribute),
                ],
            )
            .await?
        } else {
            Vec::new()
        } {
            let catalog = row
                .try_get::<&str>("catalog")?
                .ok_or_else(|| DbError::BadRow("missing dependent catalog".into()))?;
            let dep = object(
                catalog,
                row.try_get::<i64>("id")?
                    .ok_or_else(|| DbError::BadRow("missing dependent OID".into()))?
                    as u64,
                row.try_get::<i32>("attribute")?
                    .ok_or_else(|| DbError::BadRow("missing dependent attribute".into()))?,
            );
            let kind = row
                .try_get::<&str>("kind")?
                .ok_or_else(|| DbError::BadRow("missing dependency category".into()))?;
            pending.push((dep.clone(), true));
            links.insert(Link {
                dependent: dep,
                kind: kind.into(),
                binding: if id.attribute == 0 {
                    Binding::Object
                } else {
                    Binding::Column
                },
            });
        }
        // Attribute identity is not an ownership row in pg_depend.
        for (n, _, _) in &m.attributes {
            if walk && id.catalog == "pg_class" && id.attribute == 0 {
                let dep = object("pg_class", id.id, *n);
                pending.push((dep.clone(), true));
                links.insert(Link {
                    dependent: dep,
                    kind: "attribute".into(),
                    binding: Binding::Ownership(Ownership::Attribute),
                });
            }
        }
        out.graph.nodes.insert(
            id.clone(),
            Fact::Present(Node {
                described: m.described.clone(),
                held,
                dependents: if walk {
                    Fact::Present(links)
                } else {
                    Fact::Absent
                },
            }),
        );
        if walk {
            complete.insert(id.clone());
        }
        metadata.insert(id, m);
    }
    // Annotate physical edges only after every endpoint's ownership/signature is known.
    let ownership: BTreeMap<_, _> = out
        .graph
        .nodes
        .iter()
        .filter_map(|(id, fact)| match fact {
            Fact::Present(Node {
                held: Held::Owned { kind, .. },
                ..
            }) => Some((id.clone(), kind.clone())),
            Fact::Absent | Fact::Present(_) | Fact::Unreadable(_) => None,
        })
        .collect();
    for (from, fact) in &mut out.graph.nodes {
        let Fact::Present(node) = fact else {
            unreachable!("only successful node reads are inserted")
        };
        let Fact::Present(links) = &node.dependents else {
            continue;
        };
        let mut annotated = BTreeSet::new();
        for link in links {
            let m = &metadata[&link.dependent];
            let mut bindings = vec![link.binding.clone()];
            if m.owner != 0 && matches!(link.kind.as_str(), "i" | "a" | "P" | "S") {
                if let Some(kind) = ownership.get(&link.dependent) {
                    bindings = vec![Binding::Ownership(kind.clone())];
                }
            } else if m.referenced != 0 {
                bindings = vec![Binding::ForeignKey {
                    key: relation(m.index),
                }];
            } else if link.dependent.catalog == "pg_proc" && from.catalog == "pg_type" {
                bindings = m
                    .argument_types
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| **t == from.id)
                    .map(|(n, _)| Binding::Argument {
                        position: n as u32,
                        array: metadata[from].kind == "array_type",
                    })
                    .collect();
                if m.result == from.id {
                    bindings.push(Binding::Result {
                        array: metadata[from].kind == "array_type",
                    });
                }
                if bindings.is_empty() {
                    bindings.push(Binding::Object);
                }
            }
            for binding in bindings {
                annotated.insert(Link {
                    binding,
                    ..link.clone()
                });
            }
        }
        node.dependents = Fact::Present(annotated);
    }
    if matches!(root, Root::Table(_)) {
        let root_name = &out
            .relations
            .get(start)
            .ok_or_else(|| super::vanished(start.id as i64))?
            .name;
        for name in source.schema.tables.keys() {
            let mut ancestor = name;
            let mut seen = BTreeSet::new();
            while let Some(of) = source
                .schema
                .tables
                .get(ancestor)
                .and_then(|t| t.partition_of.as_ref())
            {
                if !seen.insert(ancestor) {
                    return Err(DbError::Refused(format!(
                        "{}: source partition cycle",
                        name.shown()
                    )));
                }
                ancestor = &of.parent;
            }
            if ancestor == root_name && !out.relations.values().any(|r| &r.name == name) {
                return Err(DbError::Refused(format!(
                    "{}: source tree member is absent",
                    name.shown()
                )));
            }
        }
    }
    let publications = if matches!(root, Root::Table(_)) {
        let mut memberships = BTreeSet::new();
        for row in conn
            .query_with(PUBLICATIONS, &[Param::I64(start.id as i64)])
            .await?
        {
            let field = |key| {
                row.try_get::<&str>(key)?
                    .ok_or_else(|| DbError::BadRow(format!("missing publication {key}")))
            };
            memberships.insert((
                field("publication")?.into(),
                TableName::new(field("schema")?, field("name")?),
            ));
        }
        memberships
    } else {
        BTreeSet::new()
    };
    out.spellings = Fact::Present(spellings);
    out.publications = Fact::Present(publications);
    Ok(out)
}

/// What the stored DEFAULT of each column of `tables` binds, and whether a
/// column holds a NOT NULL of its own: the references of the nodes `collect`
/// reaches as dependents, read in the opposite direction and from the same
/// catalog classes. This is the connected half of a membership boundary's
/// facts; it executes no input expression and grants no admission. A column
/// whose default binds anything the facts cannot represent says so
/// (`unrepresented`), which is never an empty binding set.
pub async fn references(
    conn: &mut Conn,
    before: Snapshot<'_>,
    tables: &BTreeSet<Uid>,
) -> Result<MembershipCatalog, DbError> {
    super::require_the_callers_transaction(conn).await?;
    conn.query(super::CANONICAL_PATH).await?;
    let mut found = MembershipCatalog::default();
    for uid in tables {
        let table = before.ids.tables.get(uid).ok_or_else(|| {
            DbError::Refused(format!(
                "{uid}: membership catalog table is absent from the baseline"
            ))
        })?;
        let rows = conn
            .query_with(
                STORED_COLUMNS,
                &[Param::Str(&table.schema), Param::Str(&table.name)],
            )
            .await?;
        for row in rows {
            let missing = |what: &str| DbError::BadRow(format!("membership column {what}"));
            let name = row
                .try_get::<&str>("attname")?
                .ok_or_else(|| missing("name"))?;
            let default = row
                .try_get::<i64>("default_id")?
                .ok_or_else(|| missing("default"))?;
            let mut column = StoredColumn {
                default: row.try_get::<&str>("definition")?.map(str::to_owned),
                local_not_null: row
                    .try_get::<bool>("local_not_null")?
                    .ok_or_else(|| missing("local NOT NULL"))?,
                ..Default::default()
            };
            if default != 0 {
                let own = row
                    .try_get::<i64>("table_id")?
                    .ok_or_else(|| missing("table"))?;
                bound_by(
                    conn,
                    &mut column,
                    object("pg_attrdef", default as u64, 0),
                    own as u64,
                )
                .await?;
            }
            found.columns.insert(table.column(name), column);
        }
    }
    Ok(found)
}

/// Sorts the references of one default into what the facts represent: a
/// routine, an owned sequence, a column of the same table; anything else
/// through a normal dependency, of any catalog, is unrepresented.
async fn bound_by(
    conn: &mut Conn,
    column: &mut StoredColumn,
    default: Object,
    table: u64,
) -> Result<(), DbError> {
    for edge in references_of(conn, &default).await? {
        let (catalog, id, attribute, normal) = edge;
        match (catalog.as_str(), attribute) {
            ("pg_proc", _) => {
                let m = meta(conn, &object("pg_proc", id, 0)).await?;
                let args: Result<Vec<_>, _> = m.args.iter().map(|a| a.parse()).collect();
                match args {
                    Ok(args) if m.kind == "routine" => {
                        column
                            .routines
                            .insert(ModuleId::Routine(pbps_model::RoutineId::new(
                                ObjectName::new(&m.schema, &m.name),
                                args,
                            )));
                    }
                    Ok(_) | Err(_) => column.unrepresented |= normal,
                }
            }
            ("pg_class", 0) => {
                let owner = conn
                    .query_with(SEQUENCE_OWNER, &[Param::I64(id as i64)])
                    .await?;
                let owner = owner.first().ok_or_else(|| super::vanished(id as i64))?;
                let field = |key| owner.try_get::<&str>(key);
                match (
                    field("kind")?,
                    field("owner_schema")?,
                    field("owner_table")?,
                    field("owner_column")?,
                ) {
                    (Some("S"), Some(schema), Some(table), Some(name)) => {
                        column
                            .sequences
                            .insert(TableName::new(schema, table).column(name));
                    }
                    (..) => column.unrepresented |= normal,
                }
            }
            ("pg_class", _) => column.unrepresented |= normal && id != table,
            // An operator, type, collation or anything else a default
            // depends on normally is bound by something the facts do not
            // hold: dropping it would read as a smaller binding set.
            _ => column.unrepresented |= normal,
        }
    }
    Ok(())
}

/// The mirror of `EDGES`: what `from` depends on. Each row is the referenced
/// catalog, OID, attribute, and whether the dependency is a normal one.
async fn references_of(
    conn: &mut Conn,
    from: &Object,
) -> Result<Vec<(String, u64, i32, bool)>, DbError> {
    let rows = conn
        .query_with(
            REFERENCES,
            &[
                Param::Str(&from.catalog),
                Param::I64(from.id as i64),
                Param::I32(from.attribute),
            ],
        )
        .await?;
    rows.iter()
        .map(|row| {
            let field = |what: &str| DbError::BadRow(format!("missing referenced {what}"));
            Ok((
                row.try_get::<&str>("catalog")?
                    .ok_or_else(|| field("catalog"))?
                    .to_owned(),
                row.try_get::<i64>("id")?.ok_or_else(|| field("OID"))? as u64,
                row.try_get::<i32>("attribute")?
                    .ok_or_else(|| field("attribute"))?,
                row.try_get::<&str>("kind")?
                    .ok_or_else(|| field("category"))?
                    == "n",
            ))
        })
        .collect()
}

async fn meta(conn: &mut Conn, of: &Object) -> Result<Meta, DbError> {
    let rows = conn
        .query_with(
            METADATA,
            &[
                Param::Str(&of.catalog),
                Param::I64(of.id as i64),
                Param::I32(of.attribute),
            ],
        )
        .await?;
    let json = rows
        .first()
        .and_then(|r| r.try_get::<&str>("meta").transpose())
        .transpose()?
        .ok_or_else(|| super::vanished(of.id as i64))?;
    serde_json::from_str(json).map_err(|e| DbError::BadRow(format!("dependency metadata: {e}")))
}

const REFERENCES: &str = "SELECT c.relname::text AS catalog, d.refobjid::int8 AS id, d.refobjsubid AS attribute, d.deptype::text AS kind FROM pg_catalog.pg_depend d JOIN pg_catalog.pg_class c ON c.oid=d.refclassid WHERE d.classid=pg_catalog.to_regclass('pg_catalog.' || $1) AND d.objid=$2::int8::oid AND d.objsubid=$3";

// A sequence is owned when an automatic or internal dependency ties it to a
// column; a relation of another kind, or an unowned sequence, has no owner row.
const SEQUENCE_OWNER: &str = "SELECT s.relkind::text AS kind, n.nspname::text AS owner_schema, o.relname::text AS owner_table, a.attname::text AS owner_column FROM pg_catalog.pg_class s LEFT JOIN pg_catalog.pg_depend own ON own.classid='pg_catalog.pg_class'::regclass AND own.objid=s.oid AND own.refclassid='pg_catalog.pg_class'::regclass AND own.deptype IN ('a','i') AND own.refobjsubid>0 LEFT JOIN pg_catalog.pg_class o ON o.oid=own.refobjid LEFT JOIN pg_catalog.pg_namespace n ON n.oid=o.relnamespace LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid=o.oid AND a.attnum=own.refobjsubid WHERE s.oid=$1::int8::oid";

// The convenience views omit what is not a plain column; read the base catalogs.
// conislocal exists on every version; contype 'n' rows exist from PostgreSQL 18.
const STORED_COLUMNS: &str = "SELECT a.attname::text AS attname, pg_catalog.pg_get_expr(ad.adbin,ad.adrelid) AS definition, COALESCE(ad.oid,0)::int8 AS default_id, t.oid::int8 AS table_id, EXISTS(SELECT 1 FROM pg_catalog.pg_constraint k WHERE k.conrelid=t.oid AND k.contype='n' AND a.attnum=ANY(k.conkey) AND k.conislocal) AS local_not_null FROM pg_catalog.pg_class t JOIN pg_catalog.pg_namespace n ON n.oid=t.relnamespace JOIN pg_catalog.pg_attribute a ON a.attrelid=t.oid AND a.attnum>0 AND NOT a.attisdropped LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid=t.oid AND ad.adnum=a.attnum WHERE n.nspname=$1 AND t.relname=$2 ORDER BY a.attnum";

const EDGES: &str = "SELECT c.relname::text AS catalog, d.objid::int8 AS id, d.objsubid AS attribute, d.deptype::text AS kind FROM pg_catalog.pg_depend d JOIN pg_catalog.pg_class c ON c.oid=d.classid WHERE d.refclassid=pg_catalog.to_regclass('pg_catalog.' || $1) AND d.refobjid=$2::int8::oid AND d.refobjsubid=$3";

// The convenience view omits an empty partitioned parent; its raw membership
// still covers future leaves. Include ancestors for publish-via-root as well.
const PUBLICATIONS: &str = "WITH RECURSIVE tree AS (SELECT relid FROM pg_catalog.pg_partition_tree($1::int8::oid) UNION SELECT $1::int8::oid), ancestors AS (SELECT relid FROM tree UNION SELECT i.inhparent FROM ancestors a JOIN pg_catalog.pg_inherits i ON i.inhrelid=a.relid) SELECT DISTINCT p.pubname AS publication, n.nspname AS schema, c.relname AS name FROM ancestors a JOIN pg_catalog.pg_class c ON c.oid=a.relid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_publication p ON p.puballtables OR EXISTS(SELECT FROM pg_catalog.pg_publication_rel r WHERE r.prpubid=p.oid AND r.prrelid=c.oid) OR EXISTS(SELECT FROM pg_catalog.pg_publication_namespace s WHERE s.pnpubid=p.oid AND s.pnnspid=c.relnamespace)";

// LEFT JOINs keep every class total. Unsupported kinds remain named evidence;
// a disappearing row in a known catalog is a failed read, not an unknown kind.
const METADATA: &str = "WITH input AS (SELECT $1::text AS catalog, $2::int8::oid AS id, $3::int AS att), facts AS (
 SELECT x.*, c.relkind, con.contype::text AS constraint_kind, c.reltype, c.reltoastrelid, t.typtype,
 CASE
 WHEN x.catalog='pg_class' AND x.att<>0 THEN 'attribute'
 WHEN storage.oid IS NOT NULL OR owner.relkind='t' THEN 'storage'
 WHEN inherited_check.oid IS NOT NULL THEN 'clone'
 WHEN c.relkind IN ('i','I') AND backing.oid IS NOT NULL THEN 'key_index'
 WHEN inherited.inhparent IS NOT NULL OR con.conparentid<>0 OR tg.tgparentid<>0 THEN 'clone'
 WHEN t.typrelid<>0 THEN 'row_type'
 WHEN element.oid IS NOT NULL THEN 'array_type'
 WHEN rw.rulename='_RETURN' AND view.relkind='v' THEN 'return_rule'
 WHEN tg.tgisinternal AND tg.tgconstraint<>0 THEN 'constraint_trigger'
 WHEN con.contype='n' THEN 'not_null'
 WHEN c.relkind='v' THEN 'view'
 WHEN proc.prokind IN ('f','p') THEN 'routine'
 WHEN tg.oid IS NOT NULL AND NOT tg.tgisinternal THEN 'trigger'
 WHEN con.conrelid<>0 THEN CASE con.contype WHEN 'p' THEN 'primary' WHEN 'u' THEN 'unique' WHEN 'f' THEN 'foreign' WHEN 'c' THEN 'check' ELSE 'constraint:'||con.contype::text END
 WHEN c.relkind IN ('i','I') THEN 'index'
 WHEN def.oid IS NOT NULL THEN CASE WHEN attr.attgenerated<>'' THEN 'generated' ELSE 'default' END
 WHEN pol.oid IS NOT NULL THEN 'policy'
 ELSE x.catalog||':'||COALESCE(c.relkind::text,proc.prokind::text,con.contype::text,'unknown') END AS kind,
 CASE WHEN x.catalog='pg_class' AND x.att<>0 THEN 'pg_class'
 WHEN storage.oid IS NOT NULL OR owner.relkind='t' THEN 'pg_class'
 WHEN inherited_check.oid IS NOT NULL THEN 'pg_constraint'
 WHEN backing.oid IS NOT NULL THEN 'pg_constraint'
 WHEN inherited.inhparent IS NOT NULL THEN 'pg_class'
 WHEN con.conparentid<>0 THEN 'pg_constraint'
 WHEN tg.tgparentid<>0 THEN 'pg_trigger'
 WHEN t.typrelid<>0 THEN 'pg_class'
 WHEN element.oid IS NOT NULL THEN 'pg_type'
 WHEN rw.rulename='_RETURN' AND view.relkind='v' THEN 'pg_class'
 WHEN tg.tgisinternal AND tg.tgconstraint<>0 THEN 'pg_constraint'
 WHEN con.contype='n' THEN 'pg_class' ELSE '' END AS owner_class,
 CASE WHEN x.catalog='pg_class' AND x.att<>0 THEN c.oid
 WHEN storage.oid IS NOT NULL THEN storage.oid
 WHEN owner.relkind='t' THEN owner.oid
 WHEN inherited_check.oid IS NOT NULL THEN inherited_check.oid
 WHEN backing.oid IS NOT NULL THEN backing.oid
 WHEN inherited.inhparent IS NOT NULL THEN inherited.inhparent
 WHEN con.conparentid<>0 THEN con.conparentid
 WHEN tg.tgparentid<>0 THEN tg.tgparentid
 WHEN t.typrelid<>0 THEN t.typrelid
 WHEN element.oid IS NOT NULL THEN element.oid
 WHEN rw.rulename='_RETURN' AND view.relkind='v' THEN rw.ev_class
 WHEN tg.tgisinternal AND tg.tgconstraint<>0 THEN tg.tgconstraint
 WHEN con.contype='n' THEN con.conrelid ELSE 0 END::int8 AS owner,
 CASE WHEN con.contype='n' THEN con.conkey[1] ELSE 0 END::int AS owner_attribute,
 COALESCE(c.relnamespace,proc.pronamespace,owner.relnamespace,con.connamespace) AS namespace,
 COALESCE(owner.relname,c.relname,proc.proname,'') AS name,
 COALESCE(con.conname,tg.tgname,attr.attname,pol.polname,CASE WHEN c.relkind IN ('i','I') THEN c.relname END,'') AS part,
 COALESCE(con.conrelid,0)::int8 AS table_id,
 COALESCE(con.conindid,0)::int8 AS index,
 COALESCE(con.confrelid,0)::int8 AS referenced,
 COALESCE(con.conkey::int[],ARRAY[]::int[]) AS columns,
 COALESCE(con.confkey::int[],ARRAY[]::int[]) AS referenced_columns,
 COALESCE(proc.prorettype,0)::int8 AS result,
 COALESCE(proc.proallargtypes,proc.proargtypes::oid[],ARRAY[]::oid[])::int8[] AS argument_types,
 ARRAY(SELECT pg_catalog.format_type(a,NULL) FROM pg_catalog.unnest(proc.proargtypes) WITH ORDINALITY u(a,n) ORDER BY n) AS args,
 COALESCE(c.reltype,0)::int8 AS row_type,COALESCE(rowtype.typarray,0)::int8 AS array_type,
 COALESCE((SELECT pg_catalog.jsonb_agg(pg_catalog.jsonb_build_array(a.attnum,a.attname,a.atttypid::int8) ORDER BY a.attnum) FROM pg_catalog.pg_attribute a WHERE a.attrelid=c.oid AND a.attnum<>0 AND NOT a.attisdropped),'[]'::jsonb) AS attributes,
 CASE x.catalog WHEN 'pg_class' THEN c.oid IS NOT NULL AND (x.att=0 OR EXISTS(SELECT FROM pg_catalog.pg_attribute a WHERE a.attrelid=c.oid AND a.attnum=x.att))
 WHEN 'pg_type' THEN t.oid IS NOT NULL WHEN 'pg_proc' THEN proc.oid IS NOT NULL WHEN 'pg_constraint' THEN con.oid IS NOT NULL
 WHEN 'pg_rewrite' THEN rw.oid IS NOT NULL WHEN 'pg_trigger' THEN tg.oid IS NOT NULL WHEN 'pg_attrdef' THEN def.oid IS NOT NULL WHEN 'pg_policy' THEN pol.oid IS NOT NULL ELSE true END AS valid
 FROM input x
 LEFT JOIN pg_catalog.pg_class c ON x.catalog='pg_class' AND c.oid=x.id
 LEFT JOIN pg_catalog.pg_class storage ON c.oid=storage.reltoastrelid
 LEFT JOIN pg_catalog.pg_type rowtype ON rowtype.oid=c.reltype
 LEFT JOIN pg_catalog.pg_type t ON x.catalog='pg_type' AND t.oid=x.id
 LEFT JOIN pg_catalog.pg_type element ON element.typarray=t.oid
 LEFT JOIN pg_catalog.pg_proc proc ON x.catalog='pg_proc' AND proc.oid=x.id
 LEFT JOIN pg_catalog.pg_constraint con ON x.catalog='pg_constraint' AND con.oid=x.id
 LEFT JOIN pg_catalog.pg_inherits check_parent ON con.contype='c' AND NOT con.conislocal AND check_parent.inhrelid=con.conrelid
 LEFT JOIN pg_catalog.pg_constraint inherited_check ON inherited_check.conrelid=check_parent.inhparent AND inherited_check.conname=con.conname AND inherited_check.contype='c'
 LEFT JOIN pg_catalog.pg_constraint backing ON c.relkind IN ('i','I') AND backing.conindid=c.oid AND backing.contype IN ('p','u','x')
 LEFT JOIN pg_catalog.pg_inherits inherited ON c.oid=inherited.inhrelid
 LEFT JOIN pg_catalog.pg_rewrite rw ON x.catalog='pg_rewrite' AND rw.oid=x.id
 LEFT JOIN pg_catalog.pg_class view ON view.oid=rw.ev_class
 LEFT JOIN pg_catalog.pg_trigger tg ON x.catalog='pg_trigger' AND tg.oid=x.id
 LEFT JOIN pg_catalog.pg_attrdef def ON x.catalog='pg_attrdef' AND def.oid=x.id
 LEFT JOIN pg_catalog.pg_attribute attr ON attr.attrelid=def.adrelid AND attr.attnum=def.adnum
 LEFT JOIN pg_catalog.pg_policy pol ON x.catalog='pg_policy' AND pol.oid=x.id
 LEFT JOIN pg_catalog.pg_index ix ON c.oid=ix.indexrelid
 LEFT JOIN pg_catalog.pg_class owner ON owner.oid=COALESCE(NULLIF(con.conrelid,0),tg.tgrelid,def.adrelid,pol.polrelid,ix.indrelid)
) SELECT (pg_catalog.to_jsonb(facts) || pg_catalog.jsonb_build_object('schema',COALESCE(n.nspname,''),'described',pg_catalog.pg_describe_object(pg_catalog.to_regclass('pg_catalog.'||catalog),id,att)))::text AS meta FROM facts LEFT JOIN pg_catalog.pg_namespace n ON n.oid=facts.namespace";
