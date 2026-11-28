//! Which indexes, keys and exclusions the engine takes as a parent's clones
//! (#1671): an attached table's at the attach, read from a database and
//! rehearsed on a scratch (DEC-1671.2). The plan's records of that, and of a
//! standing partition's own under its parent's new ones (DEC-1671.3), are held
//! to the database when the plan is applied.
//!
//! The engine takes, for each index of the parent's in the order they were
//! made, the first of the table's in that order which it holds equal and which
//! nothing has taken. Both the comparison, of bound expressions, and the order
//! are the engine's, so planning copies them onto empty tables and asks it.

use crate::{Postgres, catalog, emit, quote};
use pbps_db::transport::ExecuteConnection;
use pbps_db::{Conn, DbError};
use pbps_dialect::Statement;
use pbps_diff::attach::{AdditionQuestion, Question};
use pbps_model::adoption::{
    AdditionAnswer, AttachRehearsal, HeldIndex, IndexKind, Relation, RelationIndex, Relations,
    Skipped, difference_after, record, standing, standing_own,
};
use pbps_model::{Change, ChangeSet, ObjectLocalName, PartitionChange, Strategy, TableName};
use std::collections::{BTreeMap, BTreeSet};

/// One row for each relation of `wanted` that is a table: its columns as
/// `CREATE TABLE` spells them, and its indexes in the order of their object
/// numbers, which is the order the engine looks through them in. Spelled by the
/// session's path, so it is only ever run under the catalog's canonical one.
pub(crate) fn query<'a>(wanted: impl IntoIterator<Item = &'a TableName>) -> String {
    let named = wanted
        .into_iter()
        .map(|table| {
            let (schema, name) = (emit::literal(&table.schema), emit::literal(&table.name));
            format!("({schema}, {name})")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT n.nspname::pg_catalog.text AS schema_name,
                c.relname::pg_catalog.text AS table_name,
                pg_catalog.pg_get_partkeydef(c.oid) AS partition_key,
                ARRAY(SELECT pg_catalog.format('%I %s%s%s', a.attname,
                             pg_catalog.format_type(a.atttypid, a.atttypmod),
                             (SELECT pg_catalog.format(' COLLATE %I.%I', cn.nspname, co.collname)
                                FROM pg_catalog.pg_collation co
                                JOIN pg_catalog.pg_namespace cn ON cn.oid = co.collnamespace
                               WHERE co.oid = a.attcollation AND a.attcollation <> t.typcollation),
                             CASE WHEN a.attnotnull THEN ' NOT NULL' END)
                        FROM pg_catalog.pg_attribute a
                        JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
                       WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
                       ORDER BY a.attnum) AS columns,
                COALESCE((SELECT pg_catalog.json_agg(pg_catalog.json_build_object(
                                     'name', ic.relname,
                                     'index', pg_catalog.pg_get_indexdef(i.indexrelid),
                                     'constraint', pg_catalog.pg_get_constraintdef(k.oid),
                                     'valid', i.indisvalid,
                                     'inherited', EXISTS (SELECT 1 FROM pg_catalog.pg_inherits h
                                                           WHERE h.inhrelid = i.indexrelid))
                                 ORDER BY i.indexrelid)
                            FROM pg_catalog.pg_index i
                            JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
                            LEFT JOIN pg_catalog.pg_constraint k
                              ON k.conindid = i.indexrelid AND k.conrelid = i.indrelid
                             AND k.contype IN ('p', 'u', 'x')
                           WHERE i.indrelid = c.oid), '[]'::pg_catalog.json) AS indexes
           FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE c.relkind IN ('r', 'p')
            AND (n.nspname::pg_catalog.text, c.relname::pg_catalog.text) IN (VALUES {named})"
    )
}

#[derive(serde::Deserialize)]
struct Row {
    schema_name: String,
    table_name: String,
    partition_key: Option<String>,
    columns: Vec<String>,
    indexes: Vec<Index>,
}

#[derive(serde::Deserialize)]
struct Index {
    name: String,
    index: String,
    constraint: Option<String>,
    valid: bool,
    inherited: bool,
}

/// The rows of [`query`]. A wanted relation with no row is not in the catalog;
/// a row that cannot be read is an error, never that.
pub(crate) fn decode<'a>(
    wanted: impl IntoIterator<Item = &'a TableName>,
    rows: &[serde_json::Value],
) -> Result<Relations, DbError> {
    let mut found: Relations = wanted.into_iter().map(|t| (t.clone(), None)).collect();
    for row in rows {
        let row: Row = serde_json::from_value(row.clone())
            .map_err(|e| DbError::BadRow(format!("invalid attach relation: {e}")))?;
        let slot = found
            .get_mut(&TableName::new(&row.schema_name, &row.table_name))
            .filter(|slot| slot.is_none())
            .ok_or_else(|| DbError::BadRow("an attach relation was not asked for".into()))?;
        *slot = Some(Relation {
            columns: row.columns,
            partition_key: row.partition_key,
            indexes: row
                .indexes
                .into_iter()
                .map(|i| HeldIndex {
                    index: RelationIndex {
                        name: i.name.into(),
                        index: i.index.into(),
                        constraint: i.constraint.map(Into::into),
                    },
                    valid: i.valid,
                    inherited: i.inherited,
                })
                .collect(),
        });
    }
    Ok(found)
}

/// The relations as they stand, spelled as planning read them: under the
/// canonical path, inside a savepoint of the caller's transaction, which leaves
/// the session's settings as they were.
async fn read(conn: &mut Conn, wanted: &[&TableName]) -> Result<Relations, DbError> {
    let path = catalog::canonical_path_of(conn)?;
    let sql = format!(
        "SELECT COALESCE(pg_catalog.jsonb_agg(pg_catalog.to_jsonb(r)), '[]'::pg_catalog.jsonb)\
         ::pg_catalog.text AS payload FROM ({}) r",
        query(wanted.iter().copied())
    );
    conn.execute("SAVEPOINT pbps_adoption").await?;
    let rows = async {
        conn.query(&path).await?;
        conn.query(&sql).await
    }
    .await;
    conn.execute("ROLLBACK TO SAVEPOINT pbps_adoption; RELEASE SAVEPOINT pbps_adoption")
        .await?;
    let rows = rows?;
    let payload = rows
        .first()
        .and_then(|row| row.try_get::<&str>("payload").ok().flatten())
        .ok_or_else(|| DbError::BadRow("the attach relations could not be read".into()))?;
    let payload: Vec<serde_json::Value> = serde_json::from_str(payload)
        .map_err(|e| DbError::BadRow(format!("invalid attach relations: {e}")))?;
    decode(wanted.iter().copied(), &payload)
}

/// The parent's index that took one of the table's.
struct Taken {
    by: ObjectLocalName,
    /// A constraint on the table's index stayed the table's own.
    mixed: bool,
}

/// The statement that makes `index` on the relation spelled `on`.
fn made(on: &str, index: &RelationIndex) -> Result<String, pbps_dialect::DialectError> {
    let Some(constraint) = &index.constraint else {
        return Ok(index.index.raw().to_owned());
    };
    let name = quote(index.name.raw())?;
    let mut add = format!("ALTER TABLE {on} ADD CONSTRAINT {name} ");
    add.push_str(constraint.raw());
    Ok(add)
}

/// The parent and the table made empty, the parent's indexes, `attached` where
/// the table is its partition beforehand, then the table's own indexes in the
/// order the first database made them but for those named in `out`, and
/// `statements` as apply runs them: what took each of the table's that was taken.
async fn replay(
    conn: &mut Conn,
    [(on_parent, parent), (on_table, table)]: [(&TableName, &Relation); 2],
    attached: Option<&str>,
    out: &BTreeSet<ObjectLocalName>,
    statements: &[Statement],
) -> Result<BTreeMap<ObjectLocalName, Taken>, DbError> {
    let refused = |e: pbps_dialect::DialectError| DbError::Refused(e.to_string());
    conn.query(&catalog::canonical_path_of(conn)?).await?;
    let mut made_of = Vec::new();
    for (name, relation) in [(on_parent, parent), (on_table, table)] {
        let mut create = format!(
            "CREATE TABLE {} ({})",
            emit::qualified(name).map_err(refused)?,
            relation.columns.join(", ")
        );
        if let Some(key) = &relation.partition_key {
            create.push_str(" PARTITION BY ");
            create.push_str(key);
        }
        made_of.push(create);
    }
    let on = emit::qualified(on_parent).map_err(refused)?;
    for held in &parent.indexes {
        made_of.push(made(&on, &held.index).map_err(refused)?);
    }
    made_of.extend(attached.map(str::to_owned));
    let on = emit::qualified(on_table).map_err(refused)?;
    for held in &table.indexes {
        // A clone of a parent's is the engine's to make.
        if !held.inherited && !out.contains(&held.index.name) {
            made_of.push(made(&on, &held.index).map_err(refused)?);
        }
    }
    // The definitions are another database's text, so each goes to the engine
    // as one statement and no more (DEC-2354.3).
    for sql in &made_of {
        conn.execute_extended(sql).await?;
    }
    for statement in statements {
        crate::statement::execute(conn, statement)
            .await
            .map_err(|failure| failure.error)?;
    }
    let (schema, name) = (
        emit::literal(&on_table.schema),
        emit::literal(&on_table.name),
    );
    let rows = conn
        .query(&format!(
            "SELECT own.relname::pg_catalog.text AS own, theirs.relname::pg_catalog.text AS taken_by,
                    k.oid IS NOT NULL AND k.conparentid = 0 AS mixed
               FROM pg_catalog.pg_index i
               JOIN pg_catalog.pg_class t ON t.oid = i.indrelid
               JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace
               JOIN pg_catalog.pg_class own ON own.oid = i.indexrelid
               JOIN pg_catalog.pg_inherits h ON h.inhrelid = i.indexrelid
               JOIN pg_catalog.pg_class theirs ON theirs.oid = h.inhparent
               LEFT JOIN pg_catalog.pg_constraint k
                 ON k.conindid = i.indexrelid AND k.conrelid = i.indrelid
                AND k.contype IN ('p', 'u', 'x')
              WHERE n.nspname = {schema} AND t.relname = {name}"
        ))
        .await?;
    rows.iter()
        .map(|row| {
            let name = |column| {
                row.try_get::<&str>(column)
                    .ok()
                    .flatten()
                    .map(ObjectLocalName::from)
                    .ok_or_else(|| DbError::BadRow("an adopted index has no name".into()))
            };
            let mixed = row
                .try_get::<bool>("mixed")?
                .ok_or_else(|| DbError::BadRow("an adopted index has no state".into()))?;
            let by = name("taken_by")?;
            Ok((name("own")?, Taken { by, mixed }))
        })
        .collect()
}

/// Asks the engine of `scratch`, a session that holds what the definitions call
/// and neither relation, which of the question's indexes, keys and exclusions
/// the attach takes. A record that keeps none says the engine takes none.
///
/// One answer can be wrong: a unique index of the parent's takes the index under
/// a key or unique constraint of the table's, which then stays the table's own,
/// and once the plan has dropped that one the same index of the parent's takes
/// another. PostgreSQL 16 also takes one for a parent's of another kind. So the
/// question is asked again without each one that was taken so, until none is,
/// and the answer is what the last round took.
pub async fn rehearse(
    scratch: &mut Conn,
    question: &Question,
    parent: &Relation,
    table: &Relation,
) -> Result<Result<AttachRehearsal, Skipped>, DbError> {
    // An attach never takes an invalid index, and a copy of it made on empty
    // tables is valid: the answer would be about another table.
    if parent
        .indexes
        .iter()
        .chain(&table.indexes)
        .any(|i| !i.valid)
    {
        return Ok(Err(Skipped::InvalidIndex));
    }
    // Any bound takes the same indexes (measured on 16 and 18).
    let (on, of) = (
        emit::qualified(&question.parent),
        emit::qualified(&question.table),
    );
    let (Ok(on), Ok(of)) = (on, of) else {
        return Ok(Err(Skipped::Scratch));
    };
    let attach = [Statement::new(format!(
        "ALTER TABLE {on} ATTACH PARTITION {of} DEFAULT"
    ))];
    let relations = [(&question.parent, parent), (&question.table, table)];
    let recorded = |relation: &Relation| relation.indexes.iter().map(|i| i.index.clone()).collect();
    let mut record = AttachRehearsal {
        parent: recorded(parent),
        table: recorded(table),
        kept: BTreeMap::new(),
    };
    // The table's primary key, where it may be left to the attach. Left local
    // over an index a unique index of the parent's took, it has the engine give
    // the table a second one and refuse the attach, so that state shows only as
    // a refusal.
    let key = record
        .table
        .iter()
        .find(|i| i.kind() == Some(IndexKind::PrimaryKey) && question.owns.contains(&i.name))
        .map(|i| i.name.clone());
    // What the plan drops whatever the engine says is gone by the attach. Each
    // further round leaves out at least one more, so the rounds end.
    let mut out = question.dropped.clone();
    loop {
        scratch.execute("BEGIN").await?;
        let taken = replay(scratch, relations, None, &out, &attach).await;
        scratch.execute("ROLLBACK").await?;
        let taken = match (taken, &key) {
            (Ok(taken), _) => taken,
            (Err(_), Some(key)) if !out.contains(key) => {
                out.insert(key.clone());
                continue;
            }
            (Err(error), _) => return Err(error),
        };
        // Only what this round made: the engine may give a copy it builds the
        // name of one that was left out.
        let (wrong, right): (Vec<_>, Vec<_>) = taken
            .into_iter()
            .filter(|(own, _)| question.owns.contains(own) && !out.contains(own))
            .partition(|(own, taken)| taken.mixed || !record.pairs(own, &taken.by));
        if wrong.is_empty() {
            record.kept = right.into_iter().map(|(own, t)| (own, t.by)).collect();
            return Ok(Ok(record));
        }
        out.extend(wrong.into_iter().map(|(own, _)| own));
    }
}

/// Asks the engine of `scratch` which of a standing partition's own indexes the
/// parent's new ones take, as `pg` would write them (DEC-1671.3).
///
/// A declared own the engine takes must stay the partition's own, and the plan
/// rebuilds it around the addition as without an answer. Rebuilt, it is gone by
/// the addition, which may then take another, so the question is asked again
/// without each declared own that was taken, until none is. What the last round
/// leaves is the answer: a declared own that is not taken stands beside the
/// copy, and a retiring one that is taken becomes it.
pub async fn rehearse_addition(
    scratch: &mut Conn,
    pg: &Postgres,
    question: &AdditionQuestion,
    parent: &Relation,
    partition: &Relation,
) -> Result<Result<AdditionAnswer, Skipped>, DbError> {
    let own: Vec<&HeldIndex> = partition.indexes.iter().filter(|i| !i.inherited).collect();
    // The addition takes an invalid own and marks the parent's index invalid
    // (measured on 16 and 18), and a copy made on an empty table is valid.
    if own.iter().any(|i| !i.valid) {
        return Ok(Err(Skipped::InvalidIndex));
    }
    let (Ok(on), Ok(of)) = (
        emit::qualified(&question.parent),
        emit::qualified(&question.table),
    ) else {
        return Ok(Err(Skipped::Scratch));
    };
    let attach = format!("ALTER TABLE {on} ATTACH PARTITION {of} DEFAULT");
    // The parent's standing indexes and their clones take part in no choice, so
    // none is made (measured on 16 and 18).
    let bare = Relation {
        indexes: Vec::new(),
        ..parent.clone()
    };
    let relations = [(&question.parent, &bare), (&question.table, partition)];
    let mut changes = Vec::new();
    for change in &question.changes {
        let Ok(statements) = emit::emit(pg, change, Strategy::default()) else {
            return Ok(Err(Skipped::Scratch));
        };
        changes.extend(statements);
    }
    // Each further round leaves out at least one more, so the rounds end.
    let mut out = question.dropped.clone();
    loop {
        scratch.execute("BEGIN").await?;
        let taken = replay(scratch, relations, Some(&attach), &out, &changes).await;
        scratch.execute("ROLLBACK").await?;
        // Only what this round made: the engine may give a copy it builds the
        // name of one that was left out.
        let taken: Vec<_> = taken?
            .into_iter()
            .filter(|(name, _)| !out.contains(name) && own.iter().any(|i| i.index.name == *name))
            .collect();
        // One the plan cannot take out of the way, left for a foreign key: the
        // answer would be about a state the plan does not reach.
        if taken
            .iter()
            .any(|(name, _)| !question.declared.contains(name) && !question.retiring.contains(name))
        {
            return Ok(Err(Skipped::Scratch));
        }
        let wrong: Vec<_> = taken
            .iter()
            .filter(|(name, _)| question.declared.contains(name))
            .map(|(name, _)| name.clone())
            .collect();
        if wrong.is_empty() {
            return Ok(Ok(AdditionAnswer {
                parent: question.parent_uid.clone(),
                uid: question.uid.clone(),
                additions: question.additions().map(|(name, _)| name.clone()).collect(),
                own: own.iter().map(|i| i.index.clone()).collect(),
                adopted: taken.into_iter().map(|(name, t)| (name, t.by)).collect(),
                beside: question.declared.difference(&out).cloned().collect(),
            }));
        }
        out.extend(wrong);
    }
}

/// What one relation is held to: its indexes in order, and the names that
/// follow them, whose definitions the engine prints. A partition's list holds its
/// own, a parent's clones left out.
struct Expected<'a> {
    table: &'a TableName,
    indexes: Vec<RelationIndex>,
    added: Vec<ObjectLocalName>,
    partition: bool,
}

/// How `found`, a relation as read now, is not what it is held to, or `None`.
/// One the catalog does not hold is not one with no index.
fn unlike(expected: &Expected<'_>, found: Option<&Relation>) -> Option<String> {
    let Some(relation) = found else {
        return Some("it is not there".to_owned());
    };
    let held: Vec<_> = relation
        .indexes
        .iter()
        .filter(|i| !(expected.partition && i.inherited))
        .cloned()
        .collect();
    difference_after(&expected.indexes, &expected.added, &held)
}

/// Compares each relation with what the record says it holds.
async fn compare(conn: &mut Conn, expected: &[Expected<'_>], what: &str) -> Result<(), DbError> {
    let wanted: Vec<_> = expected.iter().map(|e| e.table).collect();
    let found = read(conn, &wanted).await?;
    for one in expected {
        if let Some(how) = unlike(one, found.get(one.table).and_then(Option::as_ref)) {
            return Err(DbError::Refused(format!(
                "`{name}` is not as the {what}'s rehearsal read it: {how}. Which of its indexes \
                 the engine takes was decided on that reading, so the statement is not run. \
                 Restore the indexes, or plan again with `pbps plan --db`.",
                name = one.table.shown()
            )));
        }
    }
    Ok(())
}

fn attached<'a>(table: &'a TableName, indexes: Vec<RelationIndex>) -> Expected<'a> {
    Expected {
        table,
        indexes,
        added: Vec::new(),
        partition: false,
    }
}

/// Before any statement of an apply: every relation of every recorded
/// rehearsal, against the whole record.
pub async fn check(conn: &mut Conn, changes: &ChangeSet) -> Result<(), DbError> {
    for planned in &changes.changes {
        if let Change::Partition(PartitionChange::AttachPartition {
            table,
            parent,
            rehearsal: Some(rehearsal),
            ..
        }) = &planned.change
        {
            compare(
                conn,
                &[
                    attached(parent, rehearsal.parent.clone()),
                    attached(table, rehearsal.table.clone()),
                ],
                "attach",
            )
            .await?;
        }
        if let Some((_, rehearsal)) = record(&planned.change) {
            let each: Vec<_> = rehearsal
                .partitions
                .iter()
                .map(|of| Expected {
                    table: &of.table,
                    indexes: of.own.clone(),
                    added: Vec::new(),
                    partition: true,
                })
                .collect();
            compare(conn, &each, "parent's addition").await?;
        }
    }
    Ok(())
}

/// The lock apply takes on `parent` ahead of a recorded addition of it, without
/// `ONLY`, which locks its partitions too. The strongest mode any of its
/// recorded additions needs, taken at the first, so that none upgrades a lock it
/// holds: an exclusion's statement takes `ACCESS EXCLUSIVE`, and an index's only
/// `SHARE`, which still lets another index be made or dropped.
fn parents_lock(changes: &ChangeSet, parent: &TableName) -> &'static str {
    let exclusion = changes.changes.iter().any(|p| {
        record(&p.change).is_some_and(|(of, _)| of == parent)
            && matches!(p.change, Change::Constraint(_))
    });
    if exclusion {
        "ACCESS EXCLUSIVE"
    } else {
        "SHARE ROW EXCLUSIVE"
    }
}

/// Just before one recorded statement, in its transaction and under locks held
/// to the commit: the record as the plan's earlier changes leave it.
pub async fn hold(conn: &mut Conn, changes: &ChangeSet, pinned: &Change) -> Result<(), DbError> {
    let refused = |e: pbps_dialect::DialectError| DbError::Refused(e.to_string());
    let not_recorded = || {
        DbError::Refused(
            "the statement's change is not one of the plan's recorded rehearsals".into(),
        )
    };
    let at = changes
        .changes
        .iter()
        .position(|p| p.change == *pinned)
        .ok_or_else(not_recorded)?;
    if let Change::Partition(PartitionChange::AttachPartition {
        table,
        parent,
        rehearsal: Some(rehearsal),
        ..
    }) = pinned
    {
        // The parent's first, as the statement takes them.
        conn.execute(&format!(
            "LOCK TABLE ONLY {} IN SHARE UPDATE EXCLUSIVE MODE; \
             LOCK TABLE ONLY {} IN ACCESS EXCLUSIVE MODE;",
            emit::qualified(parent).map_err(refused)?,
            emit::qualified(table).map_err(refused)?
        ))
        .await?;
        return compare(
            conn,
            &[
                attached(parent, standing(changes, at, parent, &rehearsal.parent)),
                attached(table, standing(changes, at, table, &rehearsal.table)),
            ],
            "attach",
        )
        .await;
    }
    let (parent, rehearsal) = record(pinned).ok_or_else(not_recorded)?;
    conn.execute(&format!(
        "LOCK TABLE {} IN {} MODE;",
        emit::qualified(parent).map_err(refused)?,
        parents_lock(changes, parent)
    ))
    .await?;
    let each: Vec<_> = rehearsal
        .partitions
        .iter()
        .map(|of| {
            let (indexes, added) = standing_own(changes, at, &of.table, &of.own);
            Expected {
                table: &of.table,
                indexes,
                added,
                partition: true,
            }
        })
        .collect();
    compare(conn, &each, "parent's addition").await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str) -> serde_json::Value {
        serde_json::json!({
            "schema_name": "app", "table_name": name, "partition_key": null,
            "columns": ["n integer"],
            "indexes": [{"name": "i", "index": "CREATE INDEX i", "constraint": null, "valid": false,
                         "inherited": true}],
        })
    }

    #[test]
    fn a_relation_the_catalog_lacks_an_empty_one_and_an_unreadable_one_differ() {
        let wanted = [TableName::new("app", "c"), TableName::new("app", "p")];
        let mut empty = row("c");
        empty["indexes"] = serde_json::json!([]);
        let found = decode(&wanted, &[empty]).unwrap();
        assert!(found[&wanted[0]].as_ref().unwrap().indexes.is_empty());
        assert!(found[&wanted[1]].is_none());
        let found = decode(&wanted, &[row("c")]).unwrap();
        assert!(!found[&wanted[0]].as_ref().unwrap().indexes[0].valid);
        for column in ["columns", "indexes", "table_name"] {
            let mut broken = row("c");
            broken.as_object_mut().unwrap().remove(column);
            assert!(decode(&wanted, &[broken]).is_err(), "{column}");
        }
        assert!(decode(&wanted, &[row("other")]).is_err());
        assert!(decode(&wanted, &[row("c"), row("c")]).is_err());
    }

    fn held(name: &str, inherited: bool) -> HeldIndex {
        HeldIndex {
            index: RelationIndex {
                name: name.into(),
                index: format!("CREATE INDEX {name}").into(),
                constraint: None,
            },
            valid: true,
            inherited,
        }
    }

    /// An attach holds a relation's every index; a parent's addition holds a
    /// partition's own, a parent's clones left out, and what earlier changes add
    /// to it by name.
    #[test]
    fn each_compare_holds_its_own_list_and_a_missing_relation_is_not_an_empty_one() {
        let table = TableName::new("app", "c");
        let relation = Relation {
            columns: Vec::new(),
            partition_key: None,
            indexes: vec![held("own", false), held("clone", true)],
        };
        let expected = |names: &[&str], added: &[&str], partition| Expected {
            table: &table,
            indexes: names.iter().map(|n| held(n, false).index).collect(),
            added: added.iter().map(|n| (*n).into()).collect(),
            partition,
        };
        let found = Some(&relation);
        assert_eq!(unlike(&expected(&["own"], &[], true), found), None);
        assert_eq!(
            unlike(&expected(&["own", "clone"], &[], false), found),
            None
        );
        let attach = unlike(&expected(&["own"], &[], false), found).unwrap();
        assert!(attach.contains("has the index `clone`"), "{attach}");
        let both = Relation {
            indexes: vec![held("own", false), held("later", false)],
            ..relation.clone()
        };
        assert_eq!(
            unlike(&expected(&["own"], &["later"], true), Some(&both)),
            None
        );
        let unadded = unlike(&expected(&["own"], &["later"], true), found).unwrap();
        assert!(
            unadded.contains("no longer has the index `later`"),
            "{unadded}"
        );
        let empty = Relation {
            indexes: Vec::new(),
            ..relation.clone()
        };
        assert_eq!(unlike(&expected(&[], &[], true), Some(&empty)), None);
        assert_eq!(
            unlike(&expected(&[], &[], true), None).as_deref(),
            Some("it is not there")
        );
    }

    #[test]
    fn a_parent_is_locked_as_strongly_as_its_recorded_additions_need() {
        use pbps_model::adoption::AdditionRehearsal;
        let parent = TableName::new("app", "p");
        let recorded = || {
            Some(Box::new(AdditionRehearsal {
                partitions: Vec::new(),
            }))
        };
        let index = |rehearsal| {
            pbps_model::PlannedChange::new(Change::Index(pbps_model::IndexChange::AddIndex {
                table: parent.clone(),
                name: "p_n".into(),
                index: Box::new(pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn::column("n")],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                    filegroup: None,
                }),
                clustered: false,
                rehearsal,
            }))
        };
        let exclusion = |table: &TableName, rehearsal| {
            pbps_model::PlannedChange::new(Change::Constraint(
                pbps_model::ConstraintChange::SetExclusion {
                    table: table.clone(),
                    name: "p_x".into(),
                    from: None,
                    to: Some(Box::new(pbps_model::ExclusionConstraint {
                        method: Default::default(),
                        keys: Vec::new(),
                        include: Vec::new(),
                        filter: None,
                        storage_parameters: Default::default(),
                        timing: pbps_model::ExclusionTiming::Immediate,
                    })),
                    rehearsal,
                },
            ))
        };
        let lock = |changes| parents_lock(&ChangeSet { changes }, &parent);
        assert_eq!(lock(vec![index(recorded())]), "SHARE ROW EXCLUSIVE");
        assert_eq!(
            lock(vec![index(recorded()), exclusion(&parent, recorded())]),
            "ACCESS EXCLUSIVE"
        );
        // An unrecorded exclusion, or another parent's, asks nothing of it.
        let other = TableName::new("app", "q");
        assert_eq!(
            lock(vec![
                index(recorded()),
                exclusion(&parent, None),
                exclusion(&other, recorded())
            ]),
            "SHARE ROW EXCLUSIVE"
        );
    }
}
