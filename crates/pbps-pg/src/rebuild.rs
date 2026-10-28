//! Connected rebuild observations and the lock-and-recertify window.
use crate::{
    Postgres,
    emit::{self, literal, qualified},
};
use pbps_db::Conn;
use pbps_diff::{
    Side,
    order::{catalog::graph::Root, rebuild::*},
};
use pbps_model::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Database(#[from] pbps_db::DbError),
    #[error(transparent)]
    Dialect(#[from] pbps_dialect::DialectError),
    #[error("{0}")]
    Refused(String),
}
impl From<String> for Error {
    fn from(s: String) -> Self {
        Self::Refused(s)
    }
}

/// Retained only in memory: the final observer needs the same explicit
/// addresses and catalog occupancy as the expansion it is checking.
pub struct Verification {
    root: RebuildPartitionedTable,
    prepared: Prepared,
    catalog: pbps_diff::order::catalog::graph::Catalog,
    naming: pbps_diff::order::catalog::graph::Naming,
}
impl Verification {
    pub fn verify(
        &self,
        before: Side<'_>,
        desired: Side<'_>,
        hints: &Hints,
        changes: &ChangeSet,
        dialect: &dyn pbps_dialect::Dialect,
    ) -> Result<(), String> {
        let input = Input {
            root: &self.root,
            before,
            desired,
            hints,
            catalog: &self.catalog,
            naming: &self.naming,
            mode: PlanMode::Transactional,
        };
        verify_plan(&input, &self.prepared, changes, dialect)
    }
}

/// Complete catalog reads and typed recipes, in the caller's transaction.
pub async fn plan<S: pbps_diff::order::RouteState<D>, D: pbps_dialect::Dialect>(
    conn: &mut Conn,
    changes: pbps_diff::order::PlanningPlan<S>,
    before: Side<'_>,
    desired: Side<'_>,
    hints: &Hints,
    dialect: &D,
) -> Result<
    (
        pbps_diff::order::PlanningPlan<S::Output>,
        i64,
        i64,
        Verification,
    ),
    Error,
> {
    let [planned] = changes.changes().changes.as_slice() else {
        return Err("a rebuild must be planned on its own".to_owned().into());
    };
    let Change::RebuildPartitionedTable(root) = &planned.change else {
        return Err("missing qualified rebuild root".to_owned().into());
    };
    let root = (**root).clone();
    let root = &root;
    let catalog = crate::modules::dependencies::collect(
        conn,
        &Root::Table(before.ids.table_uid(&root.table).unwrap().clone()),
        before,
    )
    .await?;
    let naming = crate::partition_index::collect_names(
        conn,
        &root
            .before
            .keys()
            .chain(root.after.keys())
            .map(|t| t.schema.clone())
            .collect(),
    )
    .await?;
    let input = Input {
        root,
        before,
        desired,
        catalog: &catalog,
        naming: &naming,
        hints,
        mode: PlanMode::Transactional,
    };
    let prepared =
        pbps_diff::order::rebuild::prepare(&input, dialect, &mut |slots, target, released| {
            let mut allocation = crate::partition_index::Allocation::new(
                root, &catalog, &naming, slots, target, released,
            );
            index_definitions(&root.after[&root.table])
                .into_iter()
                .map(|(part, mut definition)| {
                    let name = match &mut definition {
                        PartitionIndexDefinition::PrimaryKey(key) => {
                            let name = key.name.clone().unwrap_or_else(|| {
                                crate::partition_index::clone_name(
                                    &root.table,
                                    &part,
                                    &pbps_diff::order::catalog::graph::Fact::Absent,
                                    &Default::default(),
                                )
                                .unwrap()
                            });
                            key.name = Some(name.clone());
                            name
                        }
                        PartitionIndexDefinition::Index(_)
                        | PartitionIndexDefinition::Unique(_) => match &part {
                            IndexPart::Index(n) | IndexPart::Unique(n) => n.clone(),
                            IndexPart::PrimaryKey => unreachable!(),
                        },
                    };
                    Ok(BuildPartitionedIndex {
                        parent: RebuildRelation {
                            uid: target.table_uid(&root.table).unwrap().clone(),
                            name: root.table.clone(),
                        },
                        name,
                        clones: allocation.clones(&part, &definition)?,
                        definition,
                    })
                })
                .collect()
        })?;
    let parts = ChangeSet {
        changes: prepared.parts().iter().map(|p| p.change.clone()).collect(),
    };
    let copy = parts
        .changes
        .iter()
        .find_map(|p| {
            if let Change::CopyTableRows(c) = &p.change {
                Some(c)
            } else {
                None
            }
        })
        .ok_or_else(|| "prepared rebuild has no copy".to_owned())?;
    let (rows, bytes) = observe(conn, &parts, copy).await?;
    let verification = Verification {
        root: root.clone(),
        prepared: prepared.clone(),
        catalog: catalog.clone(),
        naming: naming.clone(),
    };
    Ok((
        continue_plan(changes, &input, prepared, dialect)?,
        rows,
        bytes,
        verification,
    ))
}

/// The database, rather than a second range comparator, evaluates the new
/// bounds over the exact typed copy projection. No rows are copied here.
pub async fn observe(
    conn: &mut Conn,
    changes: &ChangeSet,
    copy: &CopyTableRows,
) -> Result<(i64, i64), Error> {
    conn.execute("SAVEPOINT pbps_rebuild_probe").await?;
    let result = async {
        for c in &copy.columns {
            let mut from = c.source.as_ref().map(|s| &s.ty);
            for stage in &c.stages {
                if let (Some(ty), Some(using)) = (from, &stage.using) {
                    crate::input_path::vet_copy_conversion(conn, &stage.input, ty, using).await?;
                }
                from = Some(&stage.cast);
            }
        }
        let relations: Vec<_> = std::iter::once(&copy.target).chain(&copy.target_partitions).collect();
        let temporary: std::collections::BTreeMap<_, _> = relations.iter().map(|r| (r.name.clone(), TableName::new("pg_temp", format!("probe_{}", Uid::generate(UidKind::Table))))).collect();
        let pg = Postgres::new();
        let mut required = Vec::new();
        for relation in &relations {
            let spec = changes.changes.iter().find_map(|p| if let Change::CreateTable { uid, table, .. } = &p.change && uid == &relation.uid { Some(table) } else { None }).ok_or_else(|| "rebuild probe has no typed replacement table".to_owned())?;
            if relation.uid == copy.target.uid {
                required = spec.columns.iter().filter(|(_, c)| !c.nullable).map(|(n, _)| crate::quote(n.raw()).map(|n| format!("{n} IS NOT NULL"))).collect::<Result<Vec<_>, _>>()?;
            }
            let mut table = (**spec).clone();
            if let Some(of) = &mut table.partition_of { of.parent = temporary.get(&of.parent).cloned().ok_or_else(|| "rebuild probe has a foreign parent".to_owned())?; }
            let change = Change::CreateTable { uid: relation.uid.clone(), name: temporary[&relation.name].clone(), table: Box::new(table), beside_default: None, moves_at_most: None };
            for statement in emit::emit(&pg, &change, Strategy::default())? { conn.execute(&statement.sql).await?; }
        }
        let mut bounds = Vec::new();
        for leaf in &copy.target_partitions {
            let name = qualified(&temporary[&leaf.name])?;
            let rows = conn.query(&format!("SELECT pg_catalog.pg_get_partition_constraintdef({}::pg_catalog.regclass) AS predicate", literal(&name))).await?;
            bounds.push(rows.first().ok_or_else(|| "rebuild bound was not read".to_owned())?.try_get::<&str>("predicate")?.ok_or_else(|| "rebuild bound is unreadable".to_owned())?.to_owned());
        }
        let projection = copy.columns.iter().map(|c| Ok(format!("{} AS {}", emit::rebuild::copy_value(c)?, crate::quote(c.target.name.raw())?))).collect::<Result<Vec<_>, pbps_dialect::DialectError>>()?.join(", ");
        let mut predicate = if bounds.is_empty() { "FALSE".into() } else { bounds.iter().map(|s| format!("({s})")).collect::<Vec<_>>().join(" OR ") };
        if !required.is_empty() { predicate = format!("({predicate}) AND {}", required.join(" AND ")); }
        let sql = format!("SELECT count(*) AS rows, count(*) FILTER (WHERE ({predicate}) IS NOT TRUE) AS outside, coalesce(sum(pg_column_size(r)),0)::bigint AS evaluated FROM (SELECT {projection} FROM {} AS src) AS r", qualified(&copy.source.name)?);
        let rows = conn.query(&sql).await?;
        let row = rows.first().ok_or_else(|| "rebuild rows were not read".to_owned())?;
        let count = row.try_get::<i64>("rows")?.ok_or_else(|| "rebuild row count is unreadable".to_owned())?;
        let outside = row.try_get::<i64>("outside")?.ok_or_else(|| "rebuild bound count is unreadable".to_owned())?;
        if count as u64 > copy.max_rows || outside != 0 { return Err(Error::Refused(format!("{}: rebuild observes {count} rows, approved max_rows {}; {outside} rows fall outside the new bounds", copy.source.name.shown(), copy.max_rows))); }
        let sizes = std::iter::once(&copy.source).chain(&copy.source_partitions).map(|r| Ok(format!("pg_catalog.pg_total_relation_size({}::pg_catalog.regclass)", literal(&qualified(&r.name)?)))).collect::<Result<Vec<_>, pbps_dialect::DialectError>>()?.join(" + ");
        let bytes = conn.query(&format!("SELECT ({sizes})::bigint AS bytes")).await?;
        let bytes = bytes.first().ok_or_else(|| "rebuild size was not read".to_owned())?.try_get::<i64>("bytes")?.ok_or_else(|| "rebuild size is unreadable".to_owned())?;
        Ok((count, bytes))
    }.await;
    let closed = conn
        .execute("ROLLBACK TO SAVEPOINT pbps_rebuild_probe; RELEASE SAVEPOINT pbps_rebuild_probe")
        .await;
    result.and_then(|value| {
        closed?;
        Ok(value)
    })
}

/// Lock the recursive source tree before reading the dependency closure again.
pub async fn window(conn: &mut Conn, before: Side<'_>, changes: &ChangeSet) -> Result<(), Error> {
    for copy in changes.changes.iter().filter_map(|p| {
        if let Change::CopyTableRows(c) = &p.change {
            Some(c)
        } else {
            None
        }
    }) {
        conn.execute(&format!(
            "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
            qualified(&copy.source.name)?
        ))
        .await?;
        let observed = crate::catalog::introspect_within_transaction(conn).await?;
        let pg = Postgres::new();
        if !pbps_diff::rebuild::same_tree(before.schema, &observed.schema, &copy.source.name, &pg) {
            return Err(format!(
                "{}: source shape changed before the rebuild window; plan again",
                copy.source.name.shown()
            )
            .into());
        }
        let catalog = crate::modules::dependencies::collect(
            conn,
            &Root::Table(copy.source.uid.clone()),
            before,
        )
        .await?;
        pbps_diff::order::rebuild::recertify(&catalog, copy, before, changes)?;
        observe(conn, changes, copy).await?;
    }
    Ok(())
}
