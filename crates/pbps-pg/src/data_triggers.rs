//! Reference-data writes must not invoke unapproved trigger code as the deployer.
//!
//! The table lock is part of the execution guard, not a post-write drift check:
//! TRIGGER privilege permits CREATE OR REPLACE even for an existing trigger.
//! It cannot change a trigger while the writer holds ROW EXCLUSIVE (DECISIONS 445).
//!
//! The named table is not the only table the write reaches: a referential
//! action writes the referencing side of a foreign key, and a trigger there
//! runs under the deployer just the same. The guard therefore authenticates the
//! whole write-producing foreign-key closure of each row operation (DECISIONS 451).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::admission::{self, Admission, Cause, Judgement};
use crate::existing_expressions::{self, Kind, Site};
use pbps_db::{Conn, DbError};
use pbps_dialect::{ColumnDomain, RewriteScope, RowOperation, RowWrite};
use pbps_model::{ModuleId, ModuleKind, Schema, TableName};

/// `insufficient_privilege`, which `LOCK TABLE` raises for a table the
/// deployment role may read but not write.
const DENIED: &str = "42501";

/// `pg_trigger.tgtype` event bits.
const INSERT: i16 = 4;
const DELETE: i16 = 8;
const UPDATE: i16 = 16;
/// The event of a statement that rewrites or scans rows without writing them: no
/// trigger bit, so nothing in the trigger catalog matches it (DEC-2563.9).
const REWRITE: i16 = 0;

/// Valid only inside the transaction that authenticated and locked the tables.
/// Oids survive a table rename; names alone would also bless a replacement.
#[derive(Default)]
pub struct Guard {
    approved: BTreeMap<i64, i64>,
    /// The roles declared trusted, which `check` judges function owners by.
    trusted: Vec<pbps_model::RoleName>,
}

/// What the plan takes away before its row statements run, in the spelling the
/// catalog still holds.
///
/// Every kind here is ordered ahead of the row changes (`order_key` classes 0,
/// 2 and 6 against 11 and 12), so by the time a row statement runs the trigger
/// is gone, the referential action is gone, or the table carrying it is. A
/// closure that followed them anyway would refuse a plan for a table the write
/// cannot reach — the same fault `gone_keys` exists to avoid in the delete
/// probe (DECISIONS 128). The allowance is `prepare`'s alone: `check` reads the
/// catalog as it is immediately before the write, where what the plan promised
/// to remove has to actually be absent.
#[derive(Default)]
pub struct Dropped {
    pub modules: BTreeSet<ModuleId>,
    /// Referencing table and constraint name, as `pg_constraint` spells them.
    pub foreign_keys: BTreeSet<(TableName, pbps_model::ObjectLocalName)>,
    pub tables: BTreeSet<TableName>,
    /// The expressions of a table the plan removes before its rows are written
    /// (`order_key` classes 0, 2 and 5 against 8 to 12), by table and name as
    /// the catalog still spells them (DEC-2563.9). Like the rest of this
    /// struct they are `prepare`'s allowance alone.
    pub checks: BTreeSet<(TableName, String)>,
    pub indexes: BTreeSet<(TableName, String)>,
    pub policies: BTreeSet<(TableName, String)>,
    /// Columns whose default or generated expression the plan drops or replaces.
    pub values: BTreeSet<(TableName, String)>,
    /// A domain's check the plan drops.
    pub domain_checks: BTreeSet<(pbps_model::TypeRef, String)>,
    /// Columns the plan drops or retypes before its rows are written (`order_key`
    /// classes 5 and 9 against 11): they no longer carry the domain they have
    /// now, so neither its CHECKs nor its DEFAULT run for them (#2748).
    pub columns: BTreeSet<(TableName, String)>,
    /// A domain whose default the plan sets or drops: the one it has now is gone
    /// before the rows (`order_key` classes 2 and 7); the new one is the plan's.
    pub domain_defaults: BTreeSet<pbps_model::TypeRef>,
    /// Partitions the plan detaches (class 6): a write through the parent no
    /// longer reaches them or what they carry.
    pub detached: BTreeSet<TableName>,
}

impl Dropped {
    /// Records the expression `change` removes or replaces, by the table and the
    /// name the catalog still holds: a check, an index, a policy, a default or a
    /// generated expression (a dropped or altered column), and a domain's check.
    /// Every one is ordered ahead of the row changes and the rewrites
    /// (`order_key` classes 0, 2, 5 and 9 against 8, 9, 11 and 12), so by the
    /// time one runs the expression is gone (DEC-2563.9).
    ///
    /// `stored` maps a table, and `stored_column` a column, from the plan's
    /// spelling to the one the catalog has before the plan's renames.
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "every other change removes no expression a row statement would run"
    )]
    pub fn note_removed_expressions(
        &mut self,
        change: &pbps_model::Change,
        stored: &dyn Fn(&TableName) -> Option<TableName>,
        stored_column: &dyn Fn(&pbps_model::ColumnRef) -> String,
    ) {
        use pbps_model::Change;
        match change {
            Change::DropCheck { table, name } => {
                if let Some(old) = stored(table) {
                    self.checks.insert((old, name.raw().to_owned()));
                }
            }
            Change::DropIndex { table, name } => {
                if let Some(old) = stored(table) {
                    self.indexes.insert((old, name.raw().to_owned()));
                }
            }
            Change::DropPolicy { table, name, .. } => {
                if let Some(old) = stored(table) {
                    self.policies.insert((old, name.clone()));
                }
            }
            Change::DropColumn { column, .. }
            | Change::AlterColumnDefault { column, .. }
            | Change::AlterColumnExpression { column, .. } => {
                if let Some(old) = stored(&column.table) {
                    self.values.insert((old.clone(), stored_column(column)));
                    // A dropped column takes its domain with it; a column whose
                    // default or expression changes keeps the domain's checks.
                    if matches!(change, Change::DropColumn { .. }) {
                        self.columns.insert((old, stored_column(column)));
                    }
                }
            }
            // Retyped, a column is no longer the domain it is now; its own
            // default and expressions stay (re-cast), and so are not excused.
            Change::AlterColumnType { column, .. } => {
                if let Some(old) = stored(&column.table) {
                    self.columns.insert((old, stored_column(column)));
                }
            }
            Change::DetachPartition { table, .. } => {
                if let Some(old) = stored(table) {
                    self.detached.insert(old);
                }
            }
            Change::AlterDomain {
                name,
                from,
                to,
                half: pbps_model::DomainHalf::Loosen,
                ..
            } => {
                if let Some(delta) = pbps_model::DomainDelta::of(from, to) {
                    for check in delta.drop_checks {
                        self.domain_checks
                            .insert((name.clone(), check.raw().to_owned()));
                    }
                    // `Loosen` sets or drops the default.
                    if delta.default.is_some() {
                        self.domain_defaults.insert(name.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

/// Authenticate existing triggers before any plan statement changes names.
/// New tables have no baseline trigger allowance. A trigger the plan drops is
/// not allowed: it must actually be gone by the time a row statement runs.
pub async fn prepare(
    conn: &mut Conn,
    admission: &Admission,
    writes: &[RowWrite],
    baseline: &Schema,
    dropped: &Dropped,
) -> Result<Guard, DbError> {
    let mut writes = writes.to_vec();
    writes.sort_by(|a, b| a.table.cmp(&b.table).then(a.operation.cmp(&b.operation)));
    writes.dedup();
    let mut guard = Guard {
        trusted: admission.trusted().to_vec(),
        ..Guard::default()
    };
    let mut sites = Vec::new();
    for write in writes {
        let reached = reachable(conn, &write, dropped, &guard.trusted).await?;
        sites.extend(reached.sites);
        for trigger in reached.triggers {
            let id = ModuleId::Trigger {
                on: trigger.table.clone(),
                name: trigger.name.clone(),
            };
            if dropped.modules.contains(&id) {
                continue;
            }
            let matches_record = baseline.modules.get(&id).is_some_and(|m| {
                m.kind == ModuleKind::Trigger
                    && crate::introspect::trigger_definition(&trigger.definition).as_deref()
                        == Some(m.definition.as_str())
            });
            if !matches_record || !trigger.causes.is_empty() {
                return Err(refused(&trigger, &write.table));
            }
            guard.approved.insert(trigger.oid, trigger.function_oid);
            // A clone is authenticated as its parent's trigger, so that
            // trigger is approved too: a partition this plan creates or
            // attaches before the row statement gets a clone of it under
            // an oid that did not exist here, which `check` accepts by it
            // (#1632 review).
            if let Some(parent) = trigger.parent_oid {
                guard.approved.insert(parent, trigger.function_oid);
            }
        }
    }
    // Every expression the writes run, judged together, so that one refusal
    // names every routine that is not admitted (DEC-2563.9).
    existing_expressions::judge(conn, admission, sites)
        .await
        .map_err(DbError::Refused)?;
    Ok(guard)
}

/// Finish deferred trigger work before recording a transactional apply (473).
/// The caller compares managed state before and after this flush: committing
/// first would let a trigger mutate rows after their success record was read.
/// No later plan write may run after this point. The authentication closure
/// cannot decide whether work is pending: an ordinary trigger's routine can
/// queue a deferred trigger on another table outside that closure (473).
pub async fn settle(conn: &mut Conn) -> Result<(), DbError> {
    conn.execute("SET CONSTRAINTS ALL IMMEDIATE").await?;
    Ok(())
}

/// Recheck immediately before each row statement, including on a newly created
/// or renamed table. The lock stays held through that statement's transaction.
pub async fn check(
    conn: &mut Conn,
    admission: &Admission,
    write: &RowWrite,
    guard: &Guard,
) -> Result<(), DbError> {
    let reached = reachable(conn, write, &Dropped::default(), &guard.trusted).await?;
    for trigger in reached.triggers {
        let approved = |oid: i64| guard.approved.get(&oid) == Some(&trigger.function_oid);
        if !(approved(trigger.oid) || trigger.parent_oid.is_some_and(approved))
            || !trigger.causes.is_empty()
        {
            return Err(refused(&trigger, &write.table));
        }
    }
    // The expressions as they stand now, with no allowance for what the plan
    // meant to remove: if it is still there, the statement runs it.
    existing_expressions::judge(conn, admission, reached.sites)
        .await
        .map_err(DbError::Refused)?;
    Ok(())
}

fn event(operation: &RowOperation) -> i16 {
    match operation {
        RowOperation::Insert { .. } | RowOperation::Copy { .. } => INSERT,
        RowOperation::Update { .. } => UPDATE,
        RowOperation::Delete => DELETE,
        RowOperation::Rewrite { .. } => REWRITE,
    }
}

/// Whether the operation writes through the trigger catalog at all. A rewrite
/// and a copy into a table the plan created fire none the guard answers for.
fn fires_triggers(operation: &RowOperation) -> bool {
    !matches!(
        operation,
        RowOperation::Rewrite { .. } | RowOperation::Copy { .. }
    )
}

/// What one write reaches: the triggers it fires and the expressions it runs.
struct Reached {
    triggers: Vec<Trigger>,
    sites: Vec<Site>,
}

/// What the catalog says of a trigger's function.
struct FunctionFacts {
    owner_oid: i64,
    secdef: bool,
    fixed_search_path: bool,
    language: String,
    /// `app.audit()`: the identity `ALTER FUNCTION` names.
    function: String,
}

struct Trigger {
    oid: i64,
    /// The parent's row trigger this is a clone of, on a partition (#1632).
    parent_oid: Option<i64>,
    table: TableName,
    function_oid: i64,
    name: String,
    definition: String,
    /// What the admission predicate refuses of the trigger's function (DEC-2563.2).
    /// Empty when it is admitted.
    causes: Vec<Cause>,
    facts: FunctionFacts,
    /// False for the table the plan writes by name; true for a table the
    /// engine writes on its own behalf through a referential action.
    through_an_action: bool,
}

/// One statement the engine will run for this write: the relation it names, the
/// trigger event, and, for an UPDATE, the columns it sets.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Statement {
    relation: i64,
    event: i16,
    columns: BTreeSet<String>,
    /// For an INSERT, the columns its column list names: a column it supplies
    /// does not run its DEFAULT, and neither does its domain's (measured on
    /// 16.15 and 18.6, #2748). Empty when the list is not known, which judges
    /// every DEFAULT, the conservative answer.
    supplied: BTreeSet<String>,
    /// For a rewrite, what it re-evaluates besides the indexes (measured on 16
    /// and 18, DEC-2563.9). `Indexes` for every other statement, which never
    /// reads it.
    rewrite: RewriteScope,
    /// Whether `read_actions` made this statement: the engine's own write for a
    /// referential action, or one half of a row movement such a write causes. A
    /// property the statement is given where it is created, never read off which
    /// relation it names: a self-referencing foreign key, or a cycle, queues an
    /// action on the very relation the plan writes (#2748).
    from_action: bool,
    /// A referential action that is `SET DEFAULT`: the only action whose UPDATE
    /// runs the DEFAULT of the columns it sets. `CASCADE` writes the new key and
    /// `SET NULL` a NULL (#2748).
    sets_default: bool,
    /// A referential action carries ONLY, and an INSERT has no descendants to
    /// reach but its partitions. Measured on PostgreSQL 18.6: a cascade updates
    /// and deletes partitions of the referencing table but never a plain
    /// inheritance descendant, while the emitted row statement reaches both.
    partitions_only: bool,
    /// The DELETE and INSERT halves of a cross-partition row movement, which
    /// are row-level events of an UPDATE and not statements of their own.
    rows_only: bool,
}

/// Every trigger the write can fire, with the tables holding them locked.
///
/// The named table is locked under the caller's `search_path` because that is
/// the name the plan will write; everything the closure reaches afterwards is
/// followed by oid, so a rename cannot move the guard onto another relation.
async fn reachable(
    conn: &mut Conn,
    write: &RowWrite,
    dropped: &Dropped,
    trusted: &[pbps_model::RoleName],
) -> Result<Reached, DbError> {
    let table = &write.table;
    // No ONLY: PostgreSQL DML can reach inheritance descendants, and locking
    // the descendants is what keeps their triggers from being replaced too.
    conn.execute(&format!(
        "LOCK TABLE {}.{} IN ROW EXCLUSIVE MODE",
        ident(&table.schema),
        ident(&table.name)
    ))
    .await?;
    let path = conn
        .query("SELECT pg_catalog.current_setting('search_path') AS path")
        .await?[0]
        .try_get::<&str>("path")?
        .ok_or_else(missing)?
        .to_owned();
    conn.query("SELECT pg_catalog.set_config('search_path', '', true)")
        .await?;
    let result = walk(conn, write, dropped, trusted).await;
    // On a query error the caller must roll back the transaction. On success,
    // restore its path before returning; no setting leaks into emitted SQL.
    if result.is_ok() {
        conn.query(&format!(
            "SELECT pg_catalog.set_config('search_path', {}, true)",
            literal(&path)
        ))
        .await?;
    }
    result
}

async fn walk(
    conn: &mut Conn,
    write: &RowWrite,
    dropped: &Dropped,
    trusted: &[pbps_model::RoleName],
) -> Result<Reached, DbError> {
    let table = &write.table;
    let rows = conn
        .query(&format!(
            "SELECT c.oid::int8 AS oid FROM pg_catalog.pg_class c
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = {} AND c.relname = {}",
            literal(&table.schema),
            literal(&table.name)
        ))
        .await?;
    let Some(row) = rows.first() else {
        // The lock above already succeeded, so the name resolved a moment ago;
        // a write to a table that is gone fails on its own statement.
        return Ok(Reached {
            triggers: Vec::new(),
            sites: Vec::new(),
        });
    };
    let named = row.try_get::<i64>("oid")?.ok_or_else(missing)?;
    let mut queue = VecDeque::from([Statement {
        relation: named,
        event: event(&write.operation),
        columns: match &write.operation {
            RowOperation::Update { columns } => {
                columns.iter().map(|c| c.raw().to_owned()).collect()
            }
            // The column a rewrite recomputes or retypes: the CHECKs that
            // mention it, and its domain's, are what the rewrite validates.
            RowOperation::Rewrite {
                scope: RewriteScope::Column { column, .. },
            } => BTreeSet::from([column.raw().to_owned()]),
            RowOperation::Insert { .. }
            | RowOperation::Delete
            | RowOperation::Copy { .. }
            | RowOperation::Rewrite { .. } => BTreeSet::new(),
        },
        rewrite: match &write.operation {
            RowOperation::Rewrite { scope } => scope.clone(),
            RowOperation::Insert { .. }
            | RowOperation::Update { .. }
            | RowOperation::Delete
            | RowOperation::Copy { .. } => RewriteScope::Indexes,
        },
        supplied: match &write.operation {
            RowOperation::Insert { columns } => {
                columns.iter().map(|c| c.raw().to_owned()).collect()
            }
            // `INSERT ... SELECT` lists its columns as well.
            RowOperation::Copy { columns } => columns.iter().map(|c| c.raw().to_owned()).collect(),
            RowOperation::Update { .. } | RowOperation::Delete | RowOperation::Rewrite { .. } => {
                BTreeSet::new()
            }
        },
        from_action: false,
        sets_default: false,
        partitions_only: false,
        rows_only: false,
    }]);
    let mut done = BTreeSet::new();
    let mut locked = BTreeSet::from([named]);
    let mut triggers = Vec::new();
    let mut sites = Vec::new();
    let fires = fires_triggers(&write.operation);
    while let Some(statement) = queue.pop_front() {
        if !done.insert(statement.clone()) {
            continue;
        }
        if locked.insert(statement.relation) {
            lock_by_oid(conn, statement.relation).await?;
        }
        // The expressions this statement runs, on the relation it names and on
        // the partitions and descendants it reaches (DEC-2563.9).
        sites.extend(read_sites(conn, &statement, dropped).await?);
        if !fires {
            continue;
        }
        refuse_rules(conn, &statement).await?;
        triggers.extend(
            read_triggers(conn, &statement, dropped)
                .await?
                .into_iter()
                .map(|mut trigger| {
                    trigger.through_an_action = statement.from_action;
                    trigger
                }),
        );
        // An INSERT fires no referential action: the row it adds is the one a
        // foreign key checks, never one another row already refers to. Neither
        // half of a row movement fires one either: measured on 18.6, a moved
        // row's referencing rows are cascaded as an UPDATE, not deleted.
        if statement.event != INSERT && !statement.rows_only {
            queue.extend(read_actions(conn, &statement, dropped).await?);
        }
        // An UPDATE that can change a partition key moves the row instead of
        // updating it, and a movement is a DELETE on the partition it leaves
        // and an INSERT on the one it lands in (DECISIONS 451).
        if statement.event == UPDATE
            && !statement.rows_only
            && can_move(conn, &statement, dropped).await?
        {
            queue.extend([DELETE, INSERT].map(|event| Statement {
                relation: statement.relation,
                event,
                columns: BTreeSet::new(),
                // The INSERT half of a movement evaluates no default at all.
                supplied: BTreeSet::new(),
                // The halves of a movement a referential action causes are the
                // engine's own as well.
                rewrite: RewriteScope::Indexes,
                from_action: statement.from_action,
                sets_default: false,
                partitions_only: statement.partitions_only,
                rows_only: true,
            }));
        }
    }
    // The one predicate, over each trigger's function, in the same snapshot and
    // under the same empty path as the triggers were read.
    let owners = triggers.iter().map(|t| t.facts.owner_oid).collect();
    let actors = crate::input_path::untrusted_actors(conn, trusted, &owners).await?;
    for trigger in &mut triggers {
        let facts = &trigger.facts;
        let routine = admission::Routine {
            tier1: false,
            secdef: facts.secdef,
            fixed_search_path: facts.fixed_search_path,
            language: &facts.language,
            untrusted_owner: actors.contains_key(&facts.owner_oid),
        };
        if let Judgement::Refuse(causes) = admission::judge(&routine, false) {
            trigger.causes = causes;
        }
    }
    Ok(Reached { triggers, sites })
}

/// Lock a relation the closure found, and prove the lock landed on it.
///
/// The name is read before the lock, so another session can still rename the
/// relation away and give the name to one of its own in between. Re-resolving
/// the name afterwards settles which relation the lock actually holds; once it
/// is the intended one, ROW EXCLUSIVE keeps it that way (DECISIONS 445).
async fn lock_by_oid(conn: &mut Conn, relation: i64) -> Result<(), DbError> {
    let rows = conn
        .query(&format!(
            "SELECT n.nspname AS schema_name, c.relname AS table_name,
                    c.relkind = 'p' AS partitioned
             FROM pg_catalog.pg_class c
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE c.oid = {relation}::pg_catalog.oid"
        ))
        .await?;
    let row = rows.first().ok_or_else(|| moved(relation))?;
    let schema = row.try_get::<&str>("schema_name")?.ok_or_else(missing)?;
    let name = row.try_get::<&str>("table_name")?.ok_or_else(missing)?;
    let (schema, name) = (schema.to_owned(), name.to_owned());
    let table = TableName::new(&schema, &name);
    // Referential actions use ONLY: an ordinary inheritance child is not in
    // their write set. A partition tree cannot contain plain inheritance
    // edges (measured on 16/18), so keep its one recursive LOCK statement;
    // enumerating partitions before locking them would introduce a new gap.
    // A relation cannot become partitioned without replacing its oid (489).
    let only = if row.try_get::<bool>("partitioned")?.ok_or_else(missing)? {
        ""
    } else {
        "ONLY "
    };
    // ROW EXCLUSIVE needs INSERT, UPDATE, DELETE or TRUNCATE on the table;
    // measured on 18.6, SELECT alone is `permission denied`. The engine runs
    // the action as the referencing table's owner and so needs none of them,
    // which makes this the one plan the closure can refuse for a reason that
    // is not a trigger at all: say which table and why (DECISIONS 451).
    conn.execute(&format!(
        "LOCK TABLE {only}{}.{} IN ROW EXCLUSIVE MODE",
        ident(&schema),
        ident(&name)
    ))
    .await
    .map_err(|e| {
        if e.server_error_code().as_deref() == Some(DENIED) {
            return e.context(format!(
                "the data-trigger guard cannot lock `{table}`, which this row operation writes through a foreign-key referential action. The deployment role needs INSERT, UPDATE, DELETE or TRUNCATE on it to hold its triggers still through the write.",
                table = table.shown(),
            ));
        }
        e
    })?;
    let rows = conn
        .query(&format!(
            "SELECT c.oid::int8 AS oid FROM pg_catalog.pg_class c
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = {} AND c.relname = {}",
            literal(&schema),
            literal(&name)
        ))
        .await?;
    let locked = match rows.first() {
        Some(row) => row.try_get::<i64>("oid")?,
        None => None,
    };
    if locked != Some(relation) {
        return Err(moved(relation));
    }
    Ok(())
}

/// The relations one statement reaches, and the columns an UPDATE touches on
/// each of them. Both queries below start from these.
fn reached(statement: &Statement, dropped: &Dropped) -> String {
    let columns = statement
        .columns
        .iter()
        .map(|name| literal(name))
        .collect::<Vec<_>>()
        .join(", ");
    let descends = statement.event != INSERT && !statement.partitions_only;
    // UPDATE OF follows the SET list, including generated columns that depend
    // on it. A BEFORE ROW UPDATE trigger makes PostgreSQL include all generated
    // columns, even if that trigger is disabled; measured on PostgreSQL 18.
    //
    // A trigger this plan drops widens nothing, for the reason the dropped
    // foreign keys are left out: `DropModule` is `order_key` 0, so by the time
    // the row statement runs it is gone, and a closure that widened for it
    // would refuse a plan for a column the write does not touch. `check`
    // passes no removals and so asks the catalog as it stands.
    format!(
        "WITH RECURSIVE relations(oid) AS (
             SELECT {relation}::pg_catalog.oid
             UNION
             SELECT i.inhrelid FROM pg_catalog.pg_inherits i
             JOIN relations r ON r.oid = i.inhparent
             JOIN pg_catalog.pg_class parent ON parent.oid = i.inhparent
             WHERE parent.relkind = 'p' OR {descends}
         ),
         touched(attrelid, attnum) AS (
             SELECT a.attrelid, a.attnum
             FROM relations r
             JOIN pg_catalog.pg_attribute a ON a.attrelid = r.oid
             WHERE a.attname::text = ANY(ARRAY[{columns}]::text[])
                OR (a.attgenerated <> '' AND (EXISTS (
                       SELECT 1 FROM pg_catalog.pg_trigger before_update
                       JOIN pg_catalog.pg_class widener ON widener.oid = before_update.tgrelid
                       JOIN pg_catalog.pg_namespace widenerns ON widenerns.oid = widener.relnamespace
                       WHERE before_update.tgrelid = a.attrelid
                         AND (before_update.tgtype & 19) = 19
                         AND NOT ({widening_gone})
                   ) OR EXISTS (
                       SELECT 1 FROM pg_catalog.pg_attrdef ad
                       JOIN pg_catalog.pg_depend d ON d.classid = 'pg_catalog.pg_attrdef'::pg_catalog.regclass AND d.objid = ad.oid
                       JOIN pg_catalog.pg_attribute source ON source.attrelid = d.refobjid AND source.attnum = d.refobjsubid
                       WHERE ad.adrelid = a.attrelid AND ad.adnum = a.attnum
                         AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass AND d.refobjid = a.attrelid
                         AND source.attname::text = ANY(ARRAY[{columns}]::text[])
                   )))
         )",
        relation = statement.relation,
        widening_gone = gone_triggers(dropped, "widenerns", "widener", "before_update"),
    )
}

/// The trigger modules this plan drops before the row statement, as a filter on
/// a `pg_trigger` row joined to its relation and that relation's namespace.
/// `false` when there are none: an empty `OR` list is not valid SQL, and every
/// caller negates this.
fn gone_triggers(dropped: &Dropped, namespace: &str, relation: &str, trigger: &str) -> String {
    let clauses: Vec<String> = dropped
        .modules
        .iter()
        .filter_map(|id| match id {
            ModuleId::Trigger { on, name } => Some((on, name)),
            // A view or a routine is not a trigger, and neither widens a
            // write; naming the two kinds keeps a third from arriving here
            // silently matched.
            ModuleId::Named(_) | ModuleId::Routine(_) => None,
        })
        // A clone of a partitioned parent's row trigger goes with the
        // parent's, under its name, so the parent's drop removes it too,
        // measured on 16 and 18 (#1632).
        .map(|(on, name)| {
            let (schema, table, name) = (literal(&on.schema), literal(&on.name), literal(name));
            format!(
                "(({namespace}.nspname = {schema} AND {relation}.relname = {table} \
                   AND {trigger}.tgname = {name}) \
                  OR ({trigger}.tgparentid <> 0 AND {trigger}.tgname = {name} AND EXISTS ( \
                      SELECT 1 FROM pg_catalog.pg_trigger gone_parent \
                      JOIN pg_catalog.pg_class gone_rel ON gone_rel.oid = gone_parent.tgrelid \
                      JOIN pg_catalog.pg_namespace gone_ns ON gone_ns.oid = gone_rel.relnamespace \
                      WHERE gone_parent.oid = {trigger}.tgparentid \
                        AND gone_ns.nspname = {schema} AND gone_rel.relname = {table})))"
            )
        })
        .collect();
    if clauses.is_empty() {
        "false".to_owned()
    } else {
        clauses.join(" OR ")
    }
}

/// Refuse a relation this write reaches that carries a rewrite rule.
///
/// A rule decides what a statement does: measured on 18.6, an `ON UPDATE … DO
/// ALSO` rule on the table a cascade writes inserted into a third table and
/// fired its triggers, none of which the closure had locked or authenticated.
/// Following a rule would mean reading its action, and this tool parses no SQL
/// (DECISIONS 174) — so a reached relation carrying one is refused instead,
/// which is also what the model does with such a table: `ORDINARY_TABLE` holds
/// no relation with rules at all.
///
/// Not asked of a row movement's halves: measured, rules on the partitions a
/// row leaves and lands in do not fire, because the movement is one statement's
/// doing and not a statement of its own. Asked only of the relation the
/// statement names, for the same reason a movement is not asked at all.
async fn refuse_rules(conn: &mut Conn, statement: &Statement) -> Result<(), DbError> {
    if statement.rows_only {
        return Ok(());
    }
    let event = match statement.event {
        INSERT => '3',
        DELETE => '4',
        _ => '2',
    };
    // Only the relation the statement names: measured on 18.6, the rewriter
    // runs before partition routing and before inheritance expansion, so a
    // rule on a partition or on an inheritance child does not fire for a
    // statement naming their parent — and refusing for one would refuse a
    // plan whose write cannot reach it.
    let rows = conn
        .query(&format!(
            "SELECT n.nspname AS schema_name, c.relname AS table_name, w.rulename AS name
             FROM pg_catalog.pg_rewrite w
             JOIN pg_catalog.pg_class c ON c.oid = w.ev_class
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE w.ev_class = {relation}::pg_catalog.oid
               AND w.rulename <> '_RETURN' AND w.ev_type = '{event}'
               AND (w.ev_enabled = 'A' OR w.ev_enabled =
                    CASE WHEN pg_catalog.current_setting('session_replication_role') = 'replica'
                         THEN 'R' ELSE 'O' END)
             ORDER BY w.oid",
            relation = statement.relation,
        ))
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let table = TableName::new(
        row.try_get::<&str>("schema_name")?.ok_or_else(missing)?,
        row.try_get::<&str>("table_name")?.ok_or_else(missing)?,
    );
    let name = row.try_get::<&str>("name")?.ok_or_else(missing)?;
    Err(DbError::Refused(format!(
        "unsafe rewrite rule `{name}` on `{table}`: a rule decides what a write does, and this row operation reaches that table. The statements a rule adds are not part of the plan and their triggers cannot be authenticated, so the write is refused rather than executed as the deployment role. Remove the rule before planning again.",
        table = table.shown()
    )))
}

async fn read_triggers(
    conn: &mut Conn,
    statement: &Statement,
    dropped: &Dropped,
) -> Result<Vec<Trigger>, DbError> {
    // Statement-level triggers fire on the relation the statement names, not on
    // each descendant it reaches; row-level ones fire wherever the row lands.
    //
    // A clone of a partitioned parent's row trigger is no declaration's: the
    // parent's trigger is the module, and the clone is created, renamed and
    // dropped with it, measured on 16 and 18 (#1632). It is authenticated as
    // that trigger, by the parent's table, name and definition; what fires,
    // and what the guard locks to, is still the clone's own oid and function.
    let rows = conn
        .query(&format!(
            "{ctes}
             SELECT t.oid::int8 AS oid, t.tgfoid::int8 AS function_oid,
                    pt.oid::int8 AS parent_oid,
                    COALESCE(pn.nspname, n.nspname) AS schema_name,
                    COALESCE(pc.relname, c.relname) AS table_name,
                    COALESCE(pt.tgname, t.tgname) AS name,
                    pg_catalog.pg_get_triggerdef(COALESCE(pt.oid, t.oid)) AS definition,
                    p.proowner::int8 AS owner_oid, p.prosecdef AS secdef,
                    l.lanname::text AS language,
                    p.oid::pg_catalog.regprocedure::text AS function,
                    EXISTS (SELECT FROM pg_catalog.unnest(p.proconfig) AS c(setting)
                             WHERE pg_catalog.starts_with(c.setting, 'search_path=')) AS fixed_search_path
             FROM pg_catalog.pg_trigger t
             JOIN relations r ON r.oid = t.tgrelid
             JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid
             JOIN pg_catalog.pg_language l ON l.oid = p.prolang
             JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             LEFT JOIN pg_catalog.pg_trigger pt ON pt.oid = t.tgparentid
             LEFT JOIN pg_catalog.pg_class pc ON pc.oid = pt.tgrelid
             LEFT JOIN pg_catalog.pg_namespace pn ON pn.oid = pc.relnamespace
             WHERE NOT t.tgisinternal AND (t.tgtype & {events}) <> 0
               AND ({events} <> {update} OR t.tgattr = ''::pg_catalog.int2vector OR EXISTS (
                   SELECT 1 FROM touched
                   WHERE touched.attrelid = t.tgrelid AND touched.attnum = ANY(t.tgattr)
               ))
               AND {named}
               AND (t.tgenabled = 'A' OR t.tgenabled =
                    CASE WHEN pg_catalog.current_setting('session_replication_role') = 'replica'
                         THEN 'R' ELSE 'O' END)
             ORDER BY t.oid",
            ctes = reached(statement, dropped),
            events = statement.event,
            update = UPDATE,
            named = if statement.rows_only {
                // Measured on 18.6: a movement fires the partitions' row
                // triggers and no statement-level DELETE or INSERT trigger at
                // all, on the partition or on the table the statement names.
                "(t.tgtype & 1) <> 0".to_owned()
            } else {
                format!(
                    "(t.tgrelid = {}::pg_catalog.oid OR (t.tgtype & 1) <> 0)",
                    statement.relation
                )
            },
        ))
        .await?;
    rows.iter()
        .map(|r| {
            Ok(Trigger {
                oid: r.try_get("oid")?.ok_or_else(missing)?,
                parent_oid: r.try_get("parent_oid")?,
                table: TableName::new(
                    r.try_get::<&str>("schema_name")?.ok_or_else(missing)?,
                    r.try_get::<&str>("table_name")?.ok_or_else(missing)?,
                ),
                function_oid: r.try_get("function_oid")?.ok_or_else(missing)?,
                name: r.try_get::<&str>("name")?.ok_or_else(missing)?.to_owned(),
                definition: r
                    .try_get::<&str>("definition")?
                    .ok_or_else(missing)?
                    .to_owned(),
                causes: Vec::new(),
                facts: FunctionFacts {
                    owner_oid: r.try_get("owner_oid")?.ok_or_else(missing)?,
                    secdef: r.try_get("secdef")?.ok_or_else(missing)?,
                    fixed_search_path: r.try_get("fixed_search_path")?.ok_or_else(missing)?,
                    language: r
                        .try_get::<&str>("language")?
                        .ok_or_else(missing)?
                        .to_owned(),
                    function: r
                        .try_get::<&str>("function")?
                        .ok_or_else(missing)?
                        .to_owned(),
                },
                through_an_action: false,
            })
        })
        .collect()
}

/// Whether this UPDATE can move a row from one partition to another.
///
/// Measured on PostgreSQL 18.6, a move fires the row-level BEFORE and AFTER
/// DELETE triggers of the partition the row leaves and the row-level INSERT
/// triggers of the one it lands in, and no UPDATE trigger at all. A partition
/// key written as an expression is taken as always movable: which columns feed
/// it is a question this tool does not parse (DECISIONS 174).
async fn can_move(
    conn: &mut Conn,
    statement: &Statement,
    dropped: &Dropped,
) -> Result<bool, DbError> {
    let rows = conn
        .query(&format!(
            "{ctes}
             SELECT EXISTS (
                 SELECT 1 FROM relations r
                 JOIN pg_catalog.pg_partitioned_table part ON part.partrelid = r.oid
                 WHERE 0 = ANY(part.partattrs) OR EXISTS (
                     SELECT 1 FROM touched
                     WHERE touched.attrelid = part.partrelid
                       AND touched.attnum = ANY(part.partattrs)
                 )
             ) AS movable",
            ctes = reached(statement, dropped),
        ))
        .await?;
    rows.first()
        .and_then(|row| row.try_get::<bool>("movable").transpose())
        .transpose()?
        .ok_or_else(missing)
}

/// The statements PostgreSQL runs itself for this one's referential actions.
///
/// CASCADE, SET NULL and SET DEFAULT each write the referencing side; NO ACTION
/// and RESTRICT only refuse. Measured on PostgreSQL 18.6: an ON UPDATE action
/// writes every column of the foreign key even when one referenced column
/// changed, an ON DELETE SET NULL/DEFAULT with a column list writes that list
/// (`confdelsetcols`, PostgreSQL 15 and later), and the statement names the
/// relation the constraint was *declared* on: for a copy PostgreSQL made on a
/// partition that is the partitioned parent, measured, while a foreign key
/// declared on one partition by hand keeps its own leaf.
async fn read_actions(
    conn: &mut Conn,
    statement: &Statement,
    dropped: &Dropped,
) -> Result<Vec<Statement>, DbError> {
    let rows = conn
        .query(&format!(
            "{ctes},
             -- A partitioned side catalogues the declared foreign key and a
             -- copy of it per partition, and a copy can even carry another
             -- name (`qc_a_id_fkey_1`, measured). The declared one is what the
             -- action's own statement names and the only one the plan can
             -- remove by name, so both questions are asked of it. Copies are
             -- not skipped outright -- when the write names a partition of the
             -- *referenced* side, the copy is the only row that matches it at
             -- all -- and a foreign key somebody declared on a single
             -- partition is its own declared row, so it stays on that leaf.
             ancestry(copy, above) AS (
                 SELECT con.oid, con.oid
                 FROM pg_catalog.pg_constraint con
                 JOIN relations r ON r.oid = con.confrelid
                 WHERE con.contype = 'f'
                 UNION
                 SELECT a.copy, above.conparentid
                 FROM ancestry a
                 JOIN pg_catalog.pg_constraint above ON above.oid = a.above
                 WHERE above.conparentid <> 0
             ),
             tops(copy, declared) AS (
                 SELECT a.copy, a.above
                 FROM ancestry a
                 JOIN pg_catalog.pg_constraint declared ON declared.oid = a.above
                 WHERE declared.conparentid = 0
             ),
             edges(constraint_oid, referencing, declared_on, cascades, written, sets_default) AS (
                 SELECT con.oid, con.conrelid, declared.conrelid,
                        {events} = {delete} AND con.confdeltype = 'c',
                        CASE WHEN {events} = {delete} AND con.confdelsetcols IS NOT NULL
                                  AND con.confdelsetcols <> '{{}}'::pg_catalog.int2[]
                             THEN con.confdelsetcols ELSE con.conkey END,
                        (CASE WHEN {events} = {delete} THEN con.confdeltype ELSE con.confupdtype END) = 'd'
                 FROM pg_catalog.pg_constraint con
                 JOIN relations r ON r.oid = con.confrelid
                 JOIN tops ON tops.copy = con.oid
                 JOIN pg_catalog.pg_constraint declared ON declared.oid = tops.declared
                 JOIN pg_catalog.pg_class child ON child.oid = declared.conrelid
                 JOIN pg_catalog.pg_namespace childns ON childns.oid = child.relnamespace
                 WHERE con.contype = 'f'
                   AND (CASE WHEN {events} = {delete} THEN con.confdeltype ELSE con.confupdtype END)
                       IN ('c', 'n', 'd')
                   AND ({events} <> {update} OR EXISTS (
                       SELECT 1 FROM touched
                       WHERE touched.attrelid = con.confrelid AND touched.attnum = ANY(con.confkey)
                   -- The action fires on the row the write leaves, not on the
                   -- statement's SET list. Measured on 18.6, the referenced
                   -- side's own constraint trigger carries no column list
                   -- (`tgattr` is empty) and compares values, so a BEFORE ROW
                   -- UPDATE trigger that rewrites a key nothing set makes the
                   -- action fire on it. Every key of a relation that can be
                   -- rewritten that way is therefore in the closure. Unlike
                   -- the generated-column rule above, a *disabled* trigger is
                   -- no hazard here: that rule is the planner's column list,
                   -- and this one is a value written while the row is being
                   -- built, which a trigger that does not run cannot write.
                   -- A rewriter's own UPDATE OF list is read the same way the
                   -- trigger read below reads one, and for the same reason: a
                   -- trigger the statement's columns do not fire writes
                   -- nothing either.
                   ) OR EXISTS (
                       SELECT 1 FROM pg_catalog.pg_trigger rewriter
                       JOIN pg_catalog.pg_class rw ON rw.oid = rewriter.tgrelid
                       JOIN pg_catalog.pg_namespace rwns ON rwns.oid = rw.relnamespace
                       WHERE rewriter.tgrelid = con.confrelid
                         AND (rewriter.tgtype & 19) = 19
                         -- A rewriter this plan drops rewrites nothing when the
                         -- row statement runs, exactly as a removed foreign key
                         -- writes nothing: `DropModule` is `order_key` 0.
                         AND NOT ({rewriter_gone})
                         AND (rewriter.tgattr = ''::pg_catalog.int2vector OR EXISTS (
                             SELECT 1 FROM touched
                             WHERE touched.attrelid = rewriter.tgrelid
                               AND touched.attnum = ANY(rewriter.tgattr)
                         ))
                         AND (rewriter.tgenabled = 'A'
                             OR (rewriter.tgenabled = 'O' AND pg_catalog.current_setting('session_replication_role') <> 'replica')
                             OR (rewriter.tgenabled = 'R' AND pg_catalog.current_setting('session_replication_role') = 'replica'))
                   ))
                   -- The action is the constraint's own trigger on the
                   -- referenced side, and a trigger that does not fire writes
                   -- nothing: measured on 18.6, neither a disabled one nor an
                   -- origin trigger under `session_replication_role = replica`
                   -- cascades at all. The same test `preflight`'s
                   -- DELETE_ACTION_FIRES makes, by the event this write raises.
                   --
                   -- Asked of the constraint that *owns* the trigger, which is
                   -- not always the row the walk matched. Measured on 18.6: a
                   -- partitioned referencing side catalogues a copy per
                   -- partition and the referenced side carries one pair of
                   -- action triggers for the declared constraint alone, so
                   -- `tgconstraint = con.oid` finds nothing for a copy and
                   -- reads a disabled action as a firing one. A partitioned
                   -- *referenced* side is the other way: each level owns its
                   -- own pair, on its own relation. `tgrelid` separates those
                   -- two, and the ancestry chain covers both -- and
                   -- `preflight` needs neither, because every probe there
                   -- filters `con.conparentid = 0`.
                   AND NOT EXISTS (
                       SELECT 1 FROM pg_catalog.pg_trigger action
                       JOIN ancestry owner
                         ON owner.copy = con.oid AND owner.above = action.tgconstraint
                       WHERE action.tgrelid = con.confrelid
                         AND (action.tgtype & {events}) <> 0
                         AND NOT (action.tgenabled = 'A'
                             OR (action.tgenabled = 'O' AND pg_catalog.current_setting('session_replication_role') <> 'replica')
                             OR (action.tgenabled = 'R' AND pg_catalog.current_setting('session_replication_role') = 'replica'))
                   )
                   AND NOT ({gone})
             )
             SELECT DISTINCT e.constraint_oid::int8 AS constraint_oid,
                    e.declared_on::int8 AS relation,
                    (CASE WHEN e.cascades THEN {delete} ELSE {update} END)::int2 AS event,
                    e.sets_default AS sets_default,
                    a.attname AS column_name
             FROM edges e
             LEFT JOIN pg_catalog.pg_attribute a
                    ON NOT e.cascades AND a.attrelid = e.referencing AND a.attnum = ANY(e.written)
             ORDER BY constraint_oid, relation, event",
            ctes = reached(statement, dropped),
            events = statement.event,
            delete = DELETE,
            update = UPDATE,
            gone = gone(dropped),
            rewriter_gone = gone_triggers(dropped, "rwns", "rw", "rewriter"),
        ))
        .await?;
    // One statement per constraint: merging two constraints' column sets would
    // authenticate each against columns only the other one writes.
    let mut derived: BTreeMap<(i64, i64, i16), (bool, BTreeSet<String>)> = BTreeMap::new();
    for row in &rows {
        let key = (
            row.try_get::<i64>("constraint_oid")?.ok_or_else(missing)?,
            row.try_get::<i64>("relation")?.ok_or_else(missing)?,
            row.try_get::<i16>("event")?.ok_or_else(missing)?,
        );
        let (sets_default, columns) = derived.entry(key).or_default();
        *sets_default = row.try_get::<bool>("sets_default")?.ok_or_else(missing)?;
        if let Some(column) = row.try_get::<&str>("column_name")? {
            columns.insert(column.to_owned());
        }
    }
    Ok(derived
        .into_iter()
        .map(
            |((_, relation, event), (sets_default, columns))| Statement {
                relation,
                event,
                columns,
                supplied: BTreeSet::new(),
                rewrite: RewriteScope::Indexes,
                from_action: true,
                sets_default,
                partitions_only: true,
                rows_only: false,
            },
        )
        .collect())
}

/// The removals this plan performs before the row statement, as a filter on
/// the *declared* constraint's referencing table — `declared` and `child` in
/// the query that uses this, never the partition copy the walk may have
/// matched. `false` when there are none: an empty `OR` list is not valid SQL,
/// and the caller negates this.
fn gone(dropped: &Dropped) -> String {
    let of_table = |table: &TableName| {
        format!(
            "childns.nspname = {} AND child.relname = {}",
            literal(&table.schema),
            literal(&table.name)
        )
    };
    let mut clauses: Vec<String> = dropped
        .foreign_keys
        .iter()
        .map(|(table, name)| {
            format!(
                "({} AND declared.conname = {})",
                of_table(table),
                literal(name.raw())
            )
        })
        .collect();
    clauses.extend(dropped.tables.iter().map(|t| format!("({})", of_table(t))));
    if clauses.is_empty() {
        "false".to_owned()
    } else {
        clauses.join(" OR ")
    }
}

/// The descendants of a table, by the catalog's own inheritance and partition
/// links: every table that inherits from it or is a partition of it, at any
/// depth.
fn descendants_of(table: &TableName) -> String {
    format!(
        "(WITH RECURSIVE kids(oid) AS (
              SELECT i.inhrelid FROM pg_catalog.pg_inherits i
              WHERE i.inhparent = (SELECT pc.oid FROM pg_catalog.pg_class pc
                                   JOIN pg_catalog.pg_namespace pn ON pn.oid = pc.relnamespace
                                   WHERE pn.nspname = {} AND pc.relname = {})
              UNION
              SELECT i.inhrelid FROM pg_catalog.pg_inherits i JOIN kids k ON k.oid = i.inhparent
          ) SELECT oid FROM kids)",
        literal(&table.schema),
        literal(&table.name)
    )
}

/// `name` is one of `pairs` on the relation that owns the expression (`n` and
/// `c`, the namespace and class a site query joins), or `false`.
///
/// The owner, not the table the statement names: a statement on a parent
/// reaches its inheritance children and partitions, and a plan that drops an
/// unfit CHECK on a child has dropped exactly the expression that child's site
/// reads (#2748). `inherited` is the extra condition under which the same
/// object on a descendant goes with the one the plan names: a CHECK that is not
/// the child's own, a default or a column the child takes from the parent.
fn gone_owned(
    pairs: &BTreeSet<(TableName, String)>,
    name: &str,
    inherited: Option<&str>,
) -> String {
    let clauses: Vec<String> = pairs
        .iter()
        .map(|(table, wanted)| {
            let own = format!(
                "(n.nspname = {} AND c.relname = {})",
                literal(&table.schema),
                literal(&table.name)
            );
            let owner = match inherited {
                Some(condition) => format!(
                    "({own} OR ({condition} AND c.oid IN {}))",
                    descendants_of(table)
                ),
                None => own,
            };
            format!("({name} = {} AND {owner})", literal(wanted))
        })
        .collect();
    if clauses.is_empty() {
        "false".to_owned()
    } else {
        clauses.join(" OR ")
    }
}

/// A partitioned parent's index the plan drops, as a filter on the parent
/// index `pi`, its table `pt` and that table's namespace `pn`: its clones on the
/// partitions go with it, under names of their own.
fn gone_parent_index(pairs: &BTreeSet<(TableName, String)>) -> String {
    let clauses: Vec<String> = pairs
        .iter()
        .map(|(table, wanted)| {
            format!(
                "(pi.relname::pg_catalog.text = {} AND pn.nspname = {} AND pt.relname = {})",
                literal(wanted),
                literal(&table.schema),
                literal(&table.name)
            )
        })
        .collect();
    if clauses.is_empty() {
        "false".to_owned()
    } else {
        clauses.join(" OR ")
    }
}

/// The relations this plan drops before the row statement, and the partitions
/// that go with each; `false` when there are none.
fn gone_tables(dropped: &Dropped) -> String {
    table_filter(&dropped.tables)
}

/// A filter on a class `gc` and its namespace `gn`: one of `tables`, or `false`.
fn table_filter(tables: &BTreeSet<TableName>) -> String {
    let clauses: Vec<String> = tables
        .iter()
        .map(|table| {
            format!(
                "(gn.nspname = {} AND gc.relname = {})",
                literal(&table.schema),
                literal(&table.name)
            )
        })
        .collect();
    if clauses.is_empty() {
        return "false".to_owned();
    }
    clauses.join(" OR ")
}

/// The expressions one statement runs, as stored (DEC-2563.9).
///
/// Which sites a statement runs is a measured fact, not a reading of the
/// catalog: see [`existing_expressions`]. What the plan removes before its rows
/// are written is left out here and only here; `check` passes no removals, so
/// what is still standing when the row statement runs is judged.
async fn read_sites(
    conn: &mut Conn,
    statement: &Statement,
    dropped: &Dropped,
) -> Result<Vec<Site>, DbError> {
    let (extra, arms) = site_arms(statement, dropped);
    run_sites(conn, statement, dropped, &extra, &arms).await
}

/// The queries, one per kind of expression the statement runs, and the CTE the
/// domain ones read. Pure, so the table of the module docs is a unit test.
fn site_arms(statement: &Statement, dropped: &Dropped) -> (String, Vec<String>) {
    let through_an_action = statement.from_action;
    let event = statement.event;
    let (insert, update, delete, rewrite) = (
        event == INSERT,
        event == UPDATE,
        event == DELETE,
        event == REWRITE,
    );
    // A movement's INSERT half evaluates no default, and a referential action
    // is the engine's own statement, which row security does not apply to
    // (measured on 16 and 18).
    let defaults =
        (insert && !statement.rows_only) || (update && through_an_action && statement.sets_default);
    let policies = (insert || update || delete) && !through_an_action && !statement.rows_only;
    // A rewrite validates the constraints on the column it recomputes, and the
    // domain that column has afterwards; one nobody measured is judged as an
    // insert is (DEC-2563.9).
    let unmeasured = rewrite && statement.rewrite == RewriteScope::Unmeasured;
    let rewrites_a_column = rewrite && matches!(statement.rewrite, RewriteScope::Column { .. });
    let only_mentioning = if rewrites_a_column {
        "AND EXISTS (SELECT 1 FROM touched t WHERE t.attrelid = con.conrelid \
                     AND t.attnum = ANY (con.conkey))"
    } else {
        ""
    };
    // An UPDATE runs the generated expressions and domain checks of the columns
    // it sets (and of the ones those feed); an INSERT, every one.
    let touched = if update { touched_join() } else { "" };
    let on = "n.nspname::pg_catalog.text || '.' || c.relname::pg_catalog.text";
    // A relation the plan drops, and the partitions that go with it, runs
    // nothing: it is gone before the row statement.
    // A partition the plan detaches is reached by a write through its parent no
    // more, but it still is by a write that names it (or what it holds).
    let alive = format!(
        "(r.oid NOT IN (SELECT oid FROM gone_tables) AND (r.oid NOT IN (SELECT oid FROM detached) \
         OR {relation}::pg_catalog.oid IN (SELECT oid FROM detached)))",
        relation = statement.relation
    );
    let from = "FROM relations r \
                JOIN pg_catalog.pg_class c ON c.oid = r.oid \
                JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                CROSS JOIN root";
    let mut arms: Vec<String> = Vec::new();
    if insert || update || unmeasured || rewrites_a_column {
        arms.push(format!(
            "SELECT 'check' AS kind, r.oid::int8 AS oid, {on} AS place,
                    con.conname::pg_catalog.text AS name, con.conbin::pg_catalog.text AS tree
             {from}
             JOIN pg_catalog.pg_constraint con
               ON con.conrelid = r.oid AND con.contype = 'c' AND con.conbin IS NOT NULL
             WHERE NOT ({gone}) AND {alive} {only_mentioning}",
            gone = gone_owned(
                &dropped.checks,
                "con.conname::pg_catalog.text",
                Some("NOT con.conislocal")
            )
        ));
    }
    // An INSERT runs the DEFAULT of the columns it leaves out and of no column it
    // supplies (measured on 16.15 and 18.6, #2748). Where the statement's column
    // list is not known `supplied` is empty and every DEFAULT is judged.
    let supplied = statement
        .supplied
        .iter()
        .map(|name| literal(name))
        .collect::<Vec<_>>()
        .join(", ");
    let omitted_only = if insert && !statement.supplied.is_empty() {
        format!("AND a.attname::pg_catalog.text <> ALL (ARRAY[{supplied}]::pg_catalog.text[])")
    } else {
        String::new()
    };
    // A DEFAULT is the one of the relation the statement names, before any
    // routing: an INSERT through a partitioned parent fills the columns it leaves
    // out from the parent's, and a `SET DEFAULT` action on a partitioned
    // referencing table sets the parent's, so a default a partition or a child
    // sets of its own runs only for a write that names it (measured on 16.15 and
    // 18.6, #2748).
    let named_only = format!("AND r.oid = {}::pg_catalog.oid", statement.relation);
    for (kind, generated, wanted) in [
        ("default", "= ''", defaults),
        ("generated", "<> ''", insert || update || unmeasured),
    ] {
        if wanted {
            let omitted_only = if kind == "default" {
                format!("{omitted_only} {named_only}")
            } else {
                String::new()
            };
            // A default is the INSERT's, and a referential action's own
            // `SET DEFAULT`; the latter reads the columns the statement sets.
            let join = touched;
            arms.push(format!(
                "SELECT '{kind}' AS kind, r.oid::int8 AS oid, {on} AS place,
                        a.attname::pg_catalog.text AS name, ad.adbin::pg_catalog.text AS tree
                 {from}
                 JOIN pg_catalog.pg_attribute a
                   ON a.attrelid = r.oid AND a.attnum > 0 AND NOT a.attisdropped
                  AND a.attgenerated {generated}
                 JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
                 {join}
                 WHERE NOT ({gone}) AND {alive} {omitted_only}",
                gone = gone_owned(
                    &dropped.values,
                    "a.attname::pg_catalog.text",
                    Some("a.attinhcount > 0")
                )
            ));
        }
    }
    if insert || update || rewrite {
        for (kind, column) in [("index_key", "indexprs"), ("index_filter", "indpred")] {
            // A clone of a partitioned parent's index goes with the parent's,
            // under a name of its own, at any depth of the partition tree.
            let gone = gone_owned(&dropped.indexes, "ic.relname::pg_catalog.text", None);
            let gone_parent = gone_parent_index(&dropped.indexes);
            arms.push(format!(
                "SELECT '{kind}' AS kind, r.oid::int8 AS oid, {on} AS place,
                        ic.relname::pg_catalog.text AS name, i.{column}::pg_catalog.text AS tree
                 {from}
                 JOIN pg_catalog.pg_index i ON i.indrelid = r.oid AND i.{column} IS NOT NULL
                 JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
                 WHERE NOT ({gone}) AND {alive} AND NOT EXISTS (
                     WITH RECURSIVE up(oid) AS (
                         SELECT ih.inhparent FROM pg_catalog.pg_inherits ih
                         WHERE ih.inhrelid = ic.oid
                         UNION
                         SELECT ih.inhparent FROM pg_catalog.pg_inherits ih
                         JOIN up ON up.oid = ih.inhrelid
                     )
                     SELECT 1 FROM up
                     JOIN pg_catalog.pg_class pi ON pi.oid = up.oid
                     JOIN pg_catalog.pg_index pii ON pii.indexrelid = pi.oid
                     JOIN pg_catalog.pg_class pt ON pt.oid = pii.indrelid
                     JOIN pg_catalog.pg_namespace pn ON pn.oid = pt.relnamespace
                     WHERE ({gone_parent}))"
            ));
        }
    }
    if policies {
        // The policies that apply to the deploying role for this command, and
        // only where row security applies to that role at all: the server's own
        // answer, which owners, superusers and `BYPASSRLS` roles do not meet
        // (DEC-2563.10). Per command, only the expression the engine runs: an
        // INSERT its WITH CHECK (a policy without one uses its USING), an
        // UPDATE the USING of the row it finds and the WITH CHECK of the row it
        // writes, a DELETE the USING of the row it removes; a SELECT policy's
        // USING joins an UPDATE and a DELETE, which read the row first.
        let expressions: &[(&str, &str, &str)] = if insert {
            &[(
                "WITH CHECK",
                "COALESCE(p.polwithcheck, p.polqual)",
                "'*', 'a'",
            )]
        } else if update {
            &[
                ("USING", "p.polqual", "'*', 'w', 'r'"),
                ("WITH CHECK", "p.polwithcheck", "'*', 'w'"),
            ]
        } else {
            &[("USING", "p.polqual", "'*', 'd', 'r'")]
        };
        for (suffix, column, commands) in expressions {
            arms.push(format!(
                "SELECT 'policy' AS kind, r.oid::int8 AS oid, {on} AS place,
                        p.polname::pg_catalog.text || ' {suffix}' AS name,
                        {column}::pg_catalog.text AS tree
                 {from}
                 JOIN pg_catalog.pg_policy p ON p.polrelid = r.oid AND {column} IS NOT NULL
                 WHERE r.oid = {relation}::pg_catalog.oid
                   AND pg_catalog.row_security_active(r.oid)
                   AND p.polcmd IN ({commands})
                   AND (0 = ANY(p.polroles) OR EXISTS (
                        SELECT 1 FROM pg_catalog.unnest(p.polroles) AS x(rid)
                        WHERE pg_catalog.pg_has_role(CURRENT_USER, x.rid, 'MEMBER')))
                   AND NOT ({gone}) AND {alive}",
                gone = gone_owned(&dropped.policies, "p.polname::pg_catalog.text", None),
                relation = statement.relation
            ));
        }
    }
    // The domain's checks: of the columns an UPDATE sets, of the column a rewrite
    // recomputes (the type it has after), of every column an INSERT, or a rewrite
    // nobody measured, writes.
    let domain_of_a_rewrite = match &statement.rewrite {
        RewriteScope::Indexes
        | RewriteScope::Column {
            domain: ColumnDomain::Leaves,
            ..
        } => false,
        RewriteScope::Column { .. } | RewriteScope::Unmeasured => rewrite,
    };
    if insert || update || domain_of_a_rewrite {
        let only_set = if update || (rewrite && rewrites_a_column) {
            "AND EXISTS (SELECT 1 FROM touched t WHERE t.attrelid = a.attrelid AND t.attnum = a.attnum)"
        } else {
            ""
        };
        let gone_domain: Vec<String> = dropped
            .domain_checks
            .iter()
            .map(|(ty, name)| {
                format!(
                    "(dn.nspname = {} AND dt.typname = {} AND con.conname = {})",
                    literal(ty.schema()),
                    literal(ty.name()),
                    literal(name)
                )
            })
            .collect();
        let gone_domain = if gone_domain.is_empty() {
            "false".to_owned()
        } else {
            gone_domain.join(" OR ")
        };
        // The checks of every domain on the column's chain; the DEFAULT of the
        // declared type alone, which is all the engine ever evaluates.
        arms.push(format!(
            "SELECT 'domain_check' AS kind, d.typid::int8 AS oid,
                    d.typid::pg_catalog.regtype::pg_catalog.text AS place,
                    con.conname::pg_catalog.text AS name, con.conbin::pg_catalog.text AS tree
             FROM domains d
             JOIN pg_catalog.pg_type dt ON dt.oid = d.typid
             JOIN pg_catalog.pg_namespace dn ON dn.oid = dt.typnamespace
             JOIN pg_catalog.pg_constraint con
               ON con.contypid = d.typid AND con.contype = 'c' AND con.conbin IS NOT NULL
             WHERE NOT ({gone_domain})"
        ));
        if defaults {
            // The domain's DEFAULT is what a column with no DEFAULT of its own
            // gets when the statement leaves it out. A column DEFAULT overrides
            // it, a generated column never uses it, and a column the INSERT
            // supplies needs neither (measured on 16.15 and 18.6, #2748).
            let gone_default = if dropped.domain_defaults.is_empty() {
                "false".to_owned()
            } else {
                dropped
                    .domain_defaults
                    .iter()
                    .map(|ty| {
                        format!(
                            "(dn.nspname = {} AND dt.typname = {})",
                            literal(ty.schema()),
                            literal(ty.name())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" OR ")
            };
            // Whether the column has a DEFAULT of its own is a fact of the relation
            // the statement names, so it is that relation's column that is asked.
            let domain_named_only =
                format!("AND d.attrelid = {}::pg_catalog.oid", statement.relation);
            let supplied_column = if insert && !statement.supplied.is_empty() {
                format!(
                    "OR a.attname::pg_catalog.text = ANY (ARRAY[{supplied}]::pg_catalog.text[])"
                )
            } else {
                String::new()
            };
            arms.push(format!(
                "SELECT 'domain_default' AS kind, d.typid::int8 AS oid,
                        d.typid::pg_catalog.regtype::pg_catalog.text AS place,
                        ''::pg_catalog.text AS name, dt.typdefaultbin::pg_catalog.text AS tree
                 FROM domains d
                 JOIN pg_catalog.pg_type dt ON dt.oid = d.typid
                 JOIN pg_catalog.pg_namespace dn ON dn.oid = dt.typnamespace
                 WHERE d.typid = d.declared AND dt.typdefaultbin IS NOT NULL
                   {domain_named_only}
                   AND NOT ({gone_default})
                   AND NOT EXISTS (
                       SELECT 1 FROM pg_catalog.pg_attribute a
                       WHERE a.attrelid = d.attrelid AND a.attnum = d.attnum
                         AND (a.atthasdef {supplied_column})
                   )"
            ));
        }
        // A column the plan drops or retypes no longer has the domain it has now,
        // and a column a parent's drop or retype takes along goes with it.
        let gone_columns = gone_owned(
            &dropped.columns,
            "a.attname::pg_catalog.text",
            Some("a.attinhcount > 0"),
        );
        // A column changed to a domain has it only once the statement has run, so
        // the domain is read by the name the change gives it.
        let (type_of_column, domain_join) = match &statement.rewrite {
            RewriteScope::Column {
                domain: ColumnDomain::Becomes(ty),
                ..
            } if rewrite => (
                "t.oid".to_owned(),
                format!(
                    "JOIN pg_catalog.pg_type t ON t.typtype = 'd' AND t.typname = {}
                       AND t.typnamespace = (SELECT x.oid FROM pg_catalog.pg_namespace x
                                             WHERE x.nspname = {})",
                    literal(ty.name()),
                    literal(ty.schema())
                ),
            ),
            RewriteScope::Indexes
            | RewriteScope::Unmeasured
            | RewriteScope::Column {
                domain: ColumnDomain::Unchanged | ColumnDomain::Leaves | ColumnDomain::Becomes(_),
                ..
            } => (
                "a.atttypid".to_owned(),
                "JOIN pg_catalog.pg_type t ON t.oid = a.atttypid AND t.typtype = 'd'".to_owned(),
            ),
        };
        let domains = format!(
            ", domains(attrelid, attnum, declared, typid) AS (
                 SELECT a.attrelid, a.attnum, {type_of_column}, {type_of_column}
                 FROM relations r
                 JOIN pg_catalog.pg_class c ON c.oid = r.oid
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                 JOIN pg_catalog.pg_attribute a
                   ON a.attrelid = r.oid AND a.attnum > 0 AND NOT a.attisdropped
                 {domain_join}
                 WHERE {alive} AND NOT ({gone_columns}) {only_set}
                 UNION ALL
                 SELECT d.attrelid, d.attnum, d.declared, t.typbasetype
                 FROM domains d
                 JOIN pg_catalog.pg_type t ON t.oid = d.typid
                 JOIN pg_catalog.pg_type b ON b.oid = t.typbasetype
                 WHERE b.typtype = 'd'
             )"
        );
        return (domains, arms);
    }
    (String::new(), arms)
}

/// The columns an UPDATE sets, joined to the attribute a default belongs to.
fn touched_join() -> &'static str {
    "JOIN touched t ON t.attrelid = a.attrelid AND t.attnum = a.attnum"
}

async fn run_sites(
    conn: &mut Conn,
    statement: &Statement,
    dropped: &Dropped,
    extra: &str,
    arms: &[String],
) -> Result<Vec<Site>, DbError> {
    if arms.is_empty() {
        return Ok(Vec::new());
    }
    // The domain arms read `domains`, which only an INSERT or an UPDATE defines.
    let sql = format!(
        "{ctes},
         root(schema_name, table_name) AS (
             SELECT n.nspname::pg_catalog.text, c.relname::pg_catalog.text
             FROM pg_catalog.pg_class c
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE c.oid = {relation}::pg_catalog.oid
         ),
         gone_tables(oid) AS (
             SELECT gc.oid FROM pg_catalog.pg_class gc
             JOIN pg_catalog.pg_namespace gn ON gn.oid = gc.relnamespace
             WHERE {gone_tables}
             UNION
             SELECT i.inhrelid FROM pg_catalog.pg_inherits i
             JOIN gone_tables g ON g.oid = i.inhparent
             JOIN pg_catalog.pg_class p ON p.oid = i.inhparent
             WHERE p.relkind = 'p'
         ),
         detached(oid) AS (
             SELECT gc.oid FROM pg_catalog.pg_class gc
             JOIN pg_catalog.pg_namespace gn ON gn.oid = gc.relnamespace
             WHERE {detached}
             UNION
             SELECT i.inhrelid FROM pg_catalog.pg_inherits i
             JOIN detached g ON g.oid = i.inhparent
             JOIN pg_catalog.pg_class p ON p.oid = i.inhparent
             WHERE p.relkind = 'p'
         ){extra}
         {arms}",
        gone_tables = gone_tables(dropped),
        detached = table_filter(&dropped.detached),
        ctes = reached(statement, dropped),
        relation = statement.relation,
        arms = arms.join("\n UNION ALL\n "),
    );
    let rows = conn.query(&sql).await?;
    rows.iter()
        .map(|row| {
            let label = row.try_get::<&str>("kind")?.ok_or_else(missing)?;
            Ok(Site {
                kind: Kind::from_label(label).ok_or_else(missing)?,
                oid: row.try_get("oid")?.ok_or_else(missing)?,
                on: row
                    .try_get::<&str>("place")?
                    .ok_or_else(missing)?
                    .to_owned(),
                name: row.try_get::<&str>("name")?.ok_or_else(missing)?.to_owned(),
                tree: row.try_get::<&str>("tree")?.ok_or_else(missing)?.to_owned(),
            })
        })
        .collect()
}

fn ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn literal(value: &str) -> String {
    format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

fn refused(trigger: &Trigger, written: &TableName) -> DbError {
    let table = &trigger.table;
    let name = &trigger.name;
    let reached = if trigger.through_an_action {
        format!(
            " `{table}` is written by a foreign-key referential action of the row operation on `{written}`, so its triggers run under the deployment role too.",
            table = table.shown(),
            written = written.shown()
        )
    } else {
        String::new()
    };
    let function = &trigger.facts.function;
    let why: String = trigger
        .causes
        .iter()
        .map(|cause| match cause {
            Cause::UntrustedOwner => {
                format!(
                    " Its function `{function}` is owned by a role an untrusted role can act as."
                )
            }
            Cause::SecurityDefiner => {
                format!(" Its function `{function}` is SECURITY DEFINER with an untrusted owner.")
            }
            Cause::NoSearchPath => format!(
                " Its function `{function}` sets no `search_path`; run `{}`.",
                admission::search_path_fix(function)
            ),
        })
        .collect();
    DbError::Refused(format!(
        "unsafe data trigger `{name}` on `{table}`: reference-data writes require an unchanged recorded managed trigger whose function is owned by a trusted role and sets a fixed `search_path`.{reached}{why} Remove the trigger or establish that managed/trusted definition before planning again. {} `unmanaged: ignore` and `warn` do not authorize executing external trigger code.",
        admission::remedies(false),
        table = table.shown()
    ))
}

fn moved(relation: i64) -> DbError {
    DbError::Refused(format!(
        "a table reached through a foreign-key action (oid {relation}) was renamed or dropped while the data-trigger guard was locking it; nothing was written. Re-run the command."
    ))
}

fn missing() -> DbError {
    DbError::Refused("data trigger catalog read returned a missing value".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statement(event: i16, rows_only: bool) -> Statement {
        Statement {
            relation: 16384,
            event,
            columns: BTreeSet::from(["b".to_owned()]),
            supplied: BTreeSet::new(),
            rewrite: RewriteScope::Indexes,
            from_action: false,
            sets_default: false,
            partitions_only: false,
            rows_only,
        }
    }

    /// A statement `read_actions` made.
    fn action_statement(event: i16, sets_default: bool) -> Statement {
        Statement {
            from_action: true,
            sets_default,
            ..statement(event, false)
        }
    }

    /// The kinds of expression `event` is judged for, by the label each query
    /// selects.
    fn kinds(event: i16, rows_only: bool, through_an_action: bool) -> BTreeSet<String> {
        let statement = if through_an_action {
            Statement {
                from_action: true,
                rows_only,
                ..statement(event, rows_only)
            }
        } else {
            statement(event, rows_only)
        };
        let (_, arms) = site_arms(&statement, &Dropped::default());
        arms.iter()
            .map(|arm| arm.split('\'').nth(1).expect("a labelled query").to_owned())
            .collect()
    }

    fn set(labels: &[&str]) -> BTreeSet<String> {
        labels.iter().map(|l| (*l).to_owned()).collect()
    }

    #[test]
    fn an_insert_runs_every_kind_and_a_movements_insert_half_no_default() {
        let all = [
            "check",
            "default",
            "generated",
            "index_key",
            "index_filter",
            "policy",
            "domain_check",
            "domain_default",
        ];
        assert_eq!(kinds(INSERT, false, false), set(&all));
        // Measured: the INSERT half of a row movement evaluates no default,
        // and row security is the one UPDATE's, judged there.
        assert_eq!(
            kinds(INSERT, true, false),
            set(&[
                "check",
                "generated",
                "index_key",
                "index_filter",
                "domain_check"
            ])
        );
    }

    /// The kinds a referential action's UPDATE is judged for.
    fn action_kinds(sets_default: bool) -> BTreeSet<String> {
        let (_, arms) = site_arms(&action_statement(UPDATE, sets_default), &Dropped::default());
        arms.iter()
            .map(|arm| arm.split('\'').nth(1).expect("a labelled query").to_owned())
            .collect()
    }

    #[test]
    fn an_update_runs_no_default_unless_a_referential_action_sets_it() {
        let own = set(&[
            "check",
            "generated",
            "index_key",
            "index_filter",
            "policy",
            "domain_check",
        ]);
        assert_eq!(kinds(UPDATE, false, false), own);
        // The engine's own statement, and row security does not apply to it
        // (measured on 16 and 18). Only `SET DEFAULT` evaluates the default of
        // the columns it sets, which a column of a domain takes from the domain;
        // `CASCADE` writes the new key and `SET NULL` a NULL (#2748).
        let without = set(&[
            "check",
            "generated",
            "index_key",
            "index_filter",
            "domain_check",
        ]);
        assert_eq!(action_kinds(false), without);
        let mut with = without.clone();
        with.extend(set(&["default", "domain_default"]));
        assert_eq!(action_kinds(true), with);
    }

    /// The kinds a rewrite of the given scope is judged for.
    fn rewrite_arms(scope: RewriteScope) -> (String, Vec<String>) {
        let mut rewrite = statement(REWRITE, false);
        if let RewriteScope::Column { column, .. } = &scope {
            rewrite.columns = BTreeSet::from([column.raw().to_owned()]);
        }
        rewrite.rewrite = scope;
        site_arms(&rewrite, &Dropped::default())
    }

    fn labels(arms: &[String]) -> BTreeSet<String> {
        arms.iter()
            .map(|arm| arm.split('\'').nth(1).expect("a labelled query").to_owned())
            .collect()
    }

    #[test]
    fn a_rewrite_is_judged_for_what_it_validates() {
        let column = |domain| RewriteScope::Column {
            column: "v".into(),
            domain,
        };
        // The indexes alone.
        let (_, arms) = rewrite_arms(RewriteScope::Indexes);
        assert_eq!(labels(&arms), set(&["index_key", "index_filter"]));
        // A column recomputed in place: the CHECKs that mention it, and its domain's.
        let (extra, arms) = rewrite_arms(column(ColumnDomain::Unchanged));
        assert_eq!(
            labels(&arms),
            set(&["check", "index_key", "index_filter", "domain_check"])
        );
        let check = arms.iter().find(|a| a.contains("'check' AS kind")).unwrap();
        assert!(check.contains("t.attnum = ANY (con.conkey)"), "{check}");
        assert!(extra.contains("FROM touched t"), "{extra}");
        // A column changed to a domain: the domain is read by the name the
        // change gives it, not the type the column has now.
        let (extra, arms) = rewrite_arms(column(ColumnDomain::Becomes("app.dm".parse().unwrap())));
        assert!(labels(&arms).contains("domain_check"));
        assert!(
            extra.contains("t.typname = E'dm'") && extra.contains("x.nspname = E'app'"),
            "{extra}"
        );
        // A column changed to a type that is no domain: the old one goes with it.
        let (extra, arms) = rewrite_arms(column(ColumnDomain::Leaves));
        assert_eq!(
            labels(&arms),
            set(&["check", "index_key", "index_filter"]),
            "{extra}"
        );
        // A rewrite nobody measured is judged in full, defaults and policies aside.
        let (_, arms) = rewrite_arms(RewriteScope::Unmeasured);
        assert_eq!(
            labels(&arms),
            set(&[
                "check",
                "generated",
                "index_key",
                "index_filter",
                "domain_check"
            ])
        );
        let check = arms.iter().find(|a| a.contains("'check' AS kind")).unwrap();
        assert!(!check.contains("conkey"), "{check}");
    }

    #[test]
    fn a_delete_runs_only_a_policy_and_a_cascaded_delete_nothing() {
        assert_eq!(kinds(DELETE, false, false), set(&["policy"]));
        assert_eq!(kinds(DELETE, false, true), set(&[]));
    }

    #[test]
    fn a_rewrite_runs_the_index_expressions_and_nothing_else() {
        assert_eq!(
            kinds(REWRITE, false, false),
            set(&["index_key", "index_filter"])
        );
    }

    #[test]
    fn every_policy_query_asks_the_server_whether_row_security_applies() {
        for event in [INSERT, UPDATE, DELETE] {
            let (_, arms) = site_arms(&statement(event, false), &Dropped::default());
            let policies: Vec<_> = arms
                .iter()
                .filter(|arm| arm.contains("'policy' AS kind"))
                .collect();
            // An INSERT runs a WITH CHECK, a DELETE a USING, an UPDATE both.
            let expected = if event == UPDATE { 2 } else { 1 };
            assert_eq!(policies.len(), expected, "the expressions {event} runs");
            for arm in policies {
                assert!(
                    arm.contains("pg_catalog.row_security_active(r.oid)"),
                    "{arm}"
                );
                // Only the policies that name the deploying role or `public`.
                assert!(arm.contains("pg_catalog.pg_has_role(CURRENT_USER"), "{arm}");
            }
        }
    }

    #[test]
    fn an_update_reads_only_the_generated_and_domain_columns_it_sets() {
        let (extra, arms) = site_arms(&statement(UPDATE, false), &Dropped::default());
        assert!(extra.contains("FROM touched t"), "{extra}");
        let generated = arms
            .iter()
            .find(|arm| arm.contains("'generated' AS kind"))
            .unwrap();
        assert!(generated.contains("JOIN touched t"), "{generated}");
        // An INSERT reads every one.
        let (extra, arms) = site_arms(&statement(INSERT, false), &Dropped::default());
        assert!(!extra.contains("touched"), "{extra}");
        assert!(
            !arms
                .iter()
                .find(|arm| arm.contains("'generated' AS kind"))
                .unwrap()
                .contains("touched")
        );
    }

    /// The expression columns the policy arms of `event` read.
    fn policy_columns(event: i16) -> Vec<String> {
        let (_, arms) = site_arms(&statement(event, false), &Dropped::default());
        arms.iter()
            .filter(|arm| arm.contains("'policy' AS kind"))
            .map(|arm| {
                [
                    "COALESCE(p.polwithcheck, p.polqual)",
                    "p.polwithcheck",
                    "p.polqual",
                ]
                .into_iter()
                .find(|c| arm.contains(&format!("{c}::pg_catalog.text AS tree")))
                .unwrap_or("?")
                .to_owned()
            })
            .collect()
    }

    #[test]
    fn a_command_judges_only_the_policy_expressions_it_runs() {
        // INSERT: the WITH CHECK, which a policy without one takes from its USING.
        assert_eq!(
            policy_columns(INSERT),
            ["COALESCE(p.polwithcheck, p.polqual)"]
        );
        // UPDATE: the USING of the row found, the WITH CHECK of the row written.
        assert_eq!(policy_columns(UPDATE), ["p.polqual", "p.polwithcheck"]);
        // DELETE: the USING alone; a WITH CHECK is never run by a DELETE.
        assert_eq!(policy_columns(DELETE), ["p.polqual"]);
        // And the commands a policy is for: a SELECT policy joins what reads a row.
        let (_, arms) = site_arms(&statement(INSERT, false), &Dropped::default());
        let insert = arms
            .iter()
            .find(|a| a.contains("'policy' AS kind"))
            .unwrap();
        assert!(insert.contains("p.polcmd IN ('*', 'a')"), "{insert}");
        let (_, arms) = site_arms(&statement(DELETE, false), &Dropped::default());
        let delete = arms
            .iter()
            .find(|a| a.contains("'policy' AS kind"))
            .unwrap();
        assert!(delete.contains("p.polcmd IN ('*', 'd', 'r')"), "{delete}");
    }

    fn domain(default: Option<&str>, checks: &[(&str, &str)]) -> pbps_model::ManagedType {
        pbps_model::ManagedType::Domain {
            base: "integer".parse().unwrap(),
            not_null: false,
            default: default.map(str::to_owned),
            checks: checks
                .iter()
                .map(|(n, t)| (pbps_model::ObjectLocalName::from(*n), (*t).to_owned()))
                .collect(),
            collation: None,
        }
    }

    /// What `change` leaves in a fresh `Dropped`, with the names unchanged.
    fn noted(change: &pbps_model::Change) -> Dropped {
        let mut dropped = Dropped::default();
        dropped.note_removed_expressions(change, &|table| Some(table.clone()), &|column| {
            column.name.raw().to_owned()
        });
        dropped
    }

    #[test]
    fn every_step_that_removes_an_expression_is_noted_under_the_name_the_catalog_holds() {
        use pbps_model::{Change, DomainHalf};
        let column: pbps_model::ColumnRef = "app.t.v".parse().unwrap();
        let table = TableName::new("app", "t");
        let uid: pbps_model::Uid = "c_aaaaaa".parse().unwrap();
        let held = BTreeSet::from([(table.clone(), "v".to_owned())]);
        // A dropped column loses its default or generated expression and the
        // domain it has now.
        let gone = noted(&Change::DropColumn {
            uid: uid.clone(),
            column: column.clone(),
        });
        assert_eq!((&gone.values, &gone.columns), (&held, &held));
        // A retyped column keeps its default and expressions (re-cast) and
        // loses the domain; a changed default or expression keeps the domain.
        let retyped = noted(&Change::AlterColumnType {
            uid: uid.clone(),
            column: column.clone(),
            from: "app.dm".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            through_external_partitions: false,
            from_collation: None,
            to_collation: None,
            using: None,
        });
        assert_eq!((retyped.values.len(), &retyped.columns), (0, &held));
        for change in [
            Change::AlterColumnDefault {
                uid: uid.clone(),
                column: column.clone(),
                from: Some("1".into()),
                to: None,
            },
            Change::AlterColumnExpression {
                uid: uid.clone(),
                column: column.clone(),
                from: "a".into(),
                to: "b".into(),
            },
        ] {
            let noted = noted(&change);
            assert_eq!((&noted.values, noted.columns.len()), (&held, 0));
        }
        // A domain's default is gone when the loosening half sets or drops it;
        // its checks when that half drops them; the tightening half drops none.
        let name: pbps_model::TypeRef = "app.dm".parse().unwrap();
        let alter = |from, to, half| Change::AlterDomain {
            uid: uid.clone(),
            name: name.clone(),
            from: Box::new(from),
            to: Box::new(to),
            half,
        };
        let set = noted(&alter(
            domain(None, &[]),
            domain(Some("1"), &[]),
            DomainHalf::Loosen,
        ));
        assert_eq!((set.domain_defaults.len(), set.domain_checks.len()), (1, 0));
        let dropped_default = noted(&alter(
            domain(Some("1"), &[]),
            domain(None, &[]),
            DomainHalf::Loosen,
        ));
        assert_eq!(dropped_default.domain_defaults.len(), 1);
        let check_only = noted(&alter(
            domain(Some("1"), &[("dc", "VALUE > 0")]),
            domain(Some("1"), &[]),
            DomainHalf::Loosen,
        ));
        assert_eq!(
            (
                check_only.domain_defaults.len(),
                check_only.domain_checks.len()
            ),
            (0, 1)
        );
        let tightened = noted(&alter(
            domain(None, &[]),
            domain(Some("1"), &[("dc", "VALUE > 0")]),
            DomainHalf::Tighten,
        ));
        assert!(tightened.domain_defaults.is_empty() && tightened.domain_checks.is_empty());
        // A detached partition is no longer reached through its parent.
        let detached = noted(&Change::DetachPartition {
            uid,
            table: table.clone(),
            parent: TableName::new("app", "parent"),
            names: Vec::new(),
            shape: Box::default(),
        });
        assert_eq!(detached.detached, BTreeSet::from([table]));
    }

    #[test]
    fn the_domain_scan_and_every_site_leave_out_what_the_plan_removes() {
        let table = TableName::new("app", "t");
        let mut dropped = Dropped::default();
        dropped.columns.insert((table.clone(), "x".to_owned()));
        dropped.domain_defaults.insert("app.dm".parse().unwrap());
        dropped.detached.insert(TableName::new("app", "p1"));
        let (extra, arms) = site_arms(&statement(INSERT, false), &dropped);
        // The columns come out of the domain scan, with the ones a parent's
        // drop takes along.
        assert!(
            extra.contains(
                "a.attname::pg_catalog.text = E'x' AND ((n.nspname = E'app' AND c.relname = E't')"
            ),
            "{extra}"
        );
        assert!(extra.contains("a.attinhcount > 0 AND c.oid IN"), "{extra}");
        // The domain's default is left out by the domain's name.
        let default = arms
            .iter()
            .find(|a| a.contains("'domain_default' AS kind"))
            .unwrap();
        assert!(
            default.contains("dn.nspname = E'app' AND dt.typname = E'dm'"),
            "{default}"
        );
        // And every query that reads the relations leaves out a detached
        // partition, unless it is the table the statement names.
        for arm in arms.iter().filter(|a| a.contains("FROM relations r")) {
            assert!(arm.contains("SELECT oid FROM detached"), "{arm}");
        }
        assert!(extra.contains("SELECT oid FROM detached"), "{extra}");
        // Nothing dropped, nothing left out of the domain scan.
        let (extra, _) = site_arms(&statement(INSERT, false), &Dropped::default());
        assert!(extra.contains("NOT (false)"), "{extra}");
    }

    #[test]
    fn an_insert_judges_the_default_of_the_columns_it_leaves_out() {
        let arm = |statement: &Statement, label: &str| -> String {
            let (_, arms) = site_arms(statement, &Dropped::default());
            arms.into_iter()
                .find(|a| a.contains(&format!("'{label}' AS kind")))
                .unwrap_or_else(|| panic!("no {label} query"))
        };
        // A column list that is known: only the columns it leaves out.
        let mut known = statement(INSERT, false);
        known.supplied = BTreeSet::from(["id".to_owned(), "b".to_owned()]);
        let default = arm(&known, "default");
        assert!(
            default.contains("a.attname::pg_catalog.text <> ALL (ARRAY[E'b', E'id']"),
            "{default}"
        );
        // A generated column is never supplied, and its expression always runs.
        assert!(!arm(&known, "generated").contains("<> ALL"));
        // The domain's DEFAULT is for a column with none of its own that the
        // statement leaves out.
        let domain = arm(&known, "domain_default");
        assert!(domain.contains("a.atthasdef OR a.attname"), "{domain}");
        assert!(domain.contains("ANY (ARRAY[E'b', E'id']"), "{domain}");
        // A column list that is not known judges every DEFAULT.
        let unknown = statement(INSERT, false);
        assert!(!arm(&unknown, "default").contains("<> ALL"));
        let domain = arm(&unknown, "domain_default");
        assert!(domain.contains("(a.atthasdef )"), "{domain}");
        assert!(!domain.contains("ANY (ARRAY["), "{domain}");
    }

    #[test]
    fn a_default_is_read_on_the_relation_the_statement_names_and_no_other() {
        // An INSERT and a `SET DEFAULT` action alike: the relation's own default
        // runs, a partition's or a child's override only for a write naming it.
        for (label, statement) in [
            ("insert", statement(INSERT, false)),
            ("action", action_statement(UPDATE, true)),
        ] {
            let (_, arms) = site_arms(&statement, &Dropped::default());
            let arm = |kind: &str| -> String {
                arms.iter()
                    .find(|a| a.contains(&format!("'{kind}' AS kind")))
                    .unwrap_or_else(|| panic!("{label}: no {kind} query"))
                    .clone()
            };
            assert!(
                arm("default").contains("AND r.oid = 16384::pg_catalog.oid"),
                "{label}"
            );
            assert!(
                arm("domain_default").contains("AND d.attrelid = 16384::pg_catalog.oid"),
                "{label}"
            );
            // What the descendants carry still runs for the row they hold.
            for kind in ["check", "generated", "index_key", "domain_check"] {
                let text = arm(kind);
                assert!(!text.contains("r.oid = 16384"), "{label} {kind}: {text}");
                assert!(
                    !text.contains("d.attrelid = 16384"),
                    "{label} {kind}: {text}"
                );
            }
        }
    }

    #[test]
    fn a_policy_is_read_on_the_relation_the_statement_names_and_no_other() {
        for event in [INSERT, UPDATE, DELETE] {
            let (_, arms) = site_arms(&statement(event, false), &Dropped::default());
            let policies: Vec<_> = arms
                .iter()
                .filter(|a| a.contains("'policy' AS kind"))
                .collect();
            assert!(!policies.is_empty());
            for arm in policies {
                assert!(arm.contains("r.oid = 16384::pg_catalog.oid"), "{arm}");
            }
            // Every other kind keeps the closure the statement reaches.
            for arm in arms.iter().filter(|a| a.contains("'check' AS kind")) {
                assert!(!arm.contains("r.oid = 16384"), "{arm}");
            }
        }
    }

    #[test]
    fn what_the_plan_drops_is_matched_on_the_relation_that_owns_the_expression() {
        let parent = TableName::new("app", "parent");
        let mut dropped = Dropped::default();
        dropped.checks.insert((parent.clone(), "ck".to_owned()));
        dropped.values.insert((parent.clone(), "d".to_owned()));
        dropped.indexes.insert((parent.clone(), "ix".to_owned()));
        dropped.policies.insert((parent.clone(), "p".to_owned()));
        dropped.tables.insert(TableName::new("app", "gone"));
        let (_, arms) = site_arms(&statement(INSERT, false), &dropped);
        let all = arms.join("\n");
        // The owner is the joined relation, never the table the statement names.
        assert!(!all.contains("root.table_name"), "{all}");
        assert!(
            all.contains("(n.nspname = E'app' AND c.relname = E'parent')"),
            "{all}"
        );
        // An inherited CHECK and an inherited column of a descendant go with it.
        assert!(all.contains("NOT con.conislocal AND c.oid IN"), "{all}");
        assert!(all.contains("a.attinhcount > 0 AND c.oid IN"), "{all}");
        // A policy and an index are not inherited, but a partitioned parent's
        // index has clones under names of their own.
        assert!(!all.contains("p.polname::pg_catalog.text = E'p' AND ((n.nspname = E'app' AND c.relname = E'parent') OR"), "{all}");
        assert!(all.contains("pi.relname::pg_catalog.text = E'ix' AND pn.nspname = E'app' AND pt.relname = E'parent'"), "{all}");
        // Every query leaves out a table the plan drops, and its partitions.
        let (extra, _) = site_arms(&statement(INSERT, false), &dropped);
        assert!(extra.contains("gone_tables"), "{extra}");
        assert!(
            arms.iter()
                .filter(|a| a.contains("FROM relations r"))
                .all(|a| a.contains("gone_tables")),
            "{all}"
        );
    }

    #[test]
    fn what_the_plan_drops_is_left_out_by_the_name_the_catalog_holds() {
        let table = TableName::new("app", "t");
        let mut dropped = Dropped::default();
        dropped.checks.insert((table.clone(), "ck".to_owned()));
        dropped.indexes.insert((table.clone(), "ix".to_owned()));
        dropped.policies.insert((table.clone(), "p".to_owned()));
        dropped.values.insert((table, "d".to_owned()));
        let (_, arms) = site_arms(&statement(INSERT, false), &dropped);
        let all = arms.join("\n");
        for needle in [
            "con.conname::pg_catalog.text = E'ck'",
            "ic.relname::pg_catalog.text = E'ix'",
            "p.polname::pg_catalog.text = E'p'",
            "a.attname::pg_catalog.text = E'd'",
        ] {
            assert!(all.contains(needle), "{needle} is not in {all}");
        }
        // And nothing is left out when the plan drops nothing: `check` asks so.
        let (_, arms) = site_arms(&statement(INSERT, false), &Dropped::default());
        assert!(arms.join("\n").contains("NOT (false)"));
    }

    #[test]
    fn the_operations_that_fire_no_trigger_are_the_ones_that_are_not_row_writes() {
        assert!(fires_triggers(&RowOperation::Insert {
            columns: BTreeSet::new()
        }));
        assert!(fires_triggers(&RowOperation::Delete));
        assert!(fires_triggers(&RowOperation::Update {
            columns: BTreeSet::new()
        }));
        let rewrite = RowOperation::Rewrite {
            scope: RewriteScope::Indexes,
        };
        assert!(!fires_triggers(&rewrite));
        let copy = RowOperation::Copy {
            columns: BTreeSet::new(),
        };
        assert!(!fires_triggers(&copy));
        assert_eq!(event(&copy), INSERT);
        assert_eq!(event(&rewrite), REWRITE);
    }
}
