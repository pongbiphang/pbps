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

use pbps_db::{Conn, DbError};
use pbps_dialect::{RowOperation, RowWrite};
use pbps_model::{ModuleId, ModuleKind, Schema, TableName};

/// `insufficient_privilege`, which `LOCK TABLE` raises for a table the
/// deployment role may read but not write.
const DENIED: &str = "42501";

/// `pg_trigger.tgtype` event bits.
const INSERT: i16 = 4;
const DELETE: i16 = 8;
const UPDATE: i16 = 16;

/// Valid only inside the transaction that authenticated and locked the tables.
/// Oids survive a table rename; names alone would also bless a replacement.
#[derive(Default)]
pub struct Guard {
    approved: BTreeMap<i64, i64>,
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
    pub foreign_keys: BTreeSet<(TableName, String)>,
    pub tables: BTreeSet<TableName>,
}

/// Authenticate existing triggers before any plan statement changes names.
/// New tables have no baseline trigger allowance. A trigger the plan drops is
/// not allowed: it must actually be gone by the time a row statement runs.
pub async fn prepare(
    conn: &mut Conn,
    writes: &[RowWrite],
    baseline: &Schema,
    dropped: &Dropped,
) -> Result<Guard, DbError> {
    let mut writes = writes.to_vec();
    writes.sort_by(|a, b| a.table.cmp(&b.table).then(a.operation.cmp(&b.operation)));
    writes.dedup();
    let mut guard = Guard::default();
    for write in writes {
        for trigger in reachable(conn, &write, dropped).await? {
            let id = ModuleId::Trigger {
                on: trigger.table.clone(),
                name: trigger.name.clone(),
            };
            if dropped.modules.contains(&id) {
                continue;
            }
            let matches_record = baseline.modules.get(&id).is_some_and(|m| {
                m.kind == ModuleKind::Trigger
                    && crate::introspect::after_the_name(&trigger.definition, "CREATE TRIGGER ")
                        == Some(m.definition.as_str())
            });
            if !matches_record || !trigger.trusted_owner {
                return Err(refused(&trigger, &write.table));
            }
            guard.approved.insert(trigger.oid, trigger.function_oid);
        }
    }
    Ok(guard)
}

/// Recheck immediately before each row statement, including on a newly created
/// or renamed table. The lock stays held through that statement's transaction.
pub async fn check(conn: &mut Conn, write: &RowWrite, guard: &Guard) -> Result<(), DbError> {
    for trigger in reachable(conn, write, &Dropped::default()).await? {
        if guard.approved.get(&trigger.oid) != Some(&trigger.function_oid) || !trigger.trusted_owner
        {
            return Err(refused(&trigger, &write.table));
        }
    }
    Ok(())
}

fn event(operation: &RowOperation) -> i16 {
    match operation {
        RowOperation::Insert => INSERT,
        RowOperation::Update { .. } => UPDATE,
        RowOperation::Delete => DELETE,
    }
}

struct Trigger {
    oid: i64,
    table: TableName,
    function_oid: i64,
    name: String,
    definition: String,
    trusted_owner: bool,
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
) -> Result<Vec<Trigger>, DbError> {
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
    let result = walk(conn, write, dropped).await;
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
) -> Result<Vec<Trigger>, DbError> {
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
        return Ok(Vec::new());
    };
    let named = row.try_get::<i64>("oid")?.ok_or_else(missing)?;
    let mut queue = VecDeque::from([Statement {
        relation: named,
        event: event(&write.operation),
        columns: match &write.operation {
            RowOperation::Update { columns } => columns.iter().cloned().collect(),
            RowOperation::Insert | RowOperation::Delete => BTreeSet::new(),
        },
        partitions_only: false,
        rows_only: false,
    }]);
    let mut done = BTreeSet::new();
    let mut locked = BTreeSet::from([named]);
    let mut triggers = Vec::new();
    while let Some(statement) = queue.pop_front() {
        if !done.insert(statement.clone()) {
            continue;
        }
        if locked.insert(statement.relation) {
            lock_by_oid(conn, statement.relation).await?;
        }
        refuse_rules(conn, &statement).await?;
        triggers.extend(
            read_triggers(conn, &statement)
                .await?
                .into_iter()
                .map(|mut trigger| {
                    trigger.through_an_action = statement.relation != named;
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
        // and an INSERT on the one it lands in (DECISIONS 449).
        if statement.event == UPDATE && !statement.rows_only && can_move(conn, &statement).await? {
            queue.extend([DELETE, INSERT].map(|event| Statement {
                relation: statement.relation,
                event,
                columns: BTreeSet::new(),
                partitions_only: statement.partitions_only,
                rows_only: true,
            }));
        }
    }
    Ok(triggers)
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
            "SELECT n.nspname AS schema_name, c.relname AS table_name
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
    // ROW EXCLUSIVE needs INSERT, UPDATE, DELETE or TRUNCATE on the table;
    // measured on 18.6, SELECT alone is `permission denied`. The engine runs
    // the action as the referencing table's owner and so needs none of them,
    // which makes this the one plan the closure can refuse for a reason that
    // is not a trigger at all: say which table and why (DECISIONS 451).
    conn.execute(&format!(
        "LOCK TABLE {}.{} IN ROW EXCLUSIVE MODE",
        ident(&schema),
        ident(&name)
    ))
    .await
    .map_err(|e| {
        if matches!(&e, DbError::Driver { code, .. } if code.as_deref() == Some(DENIED)) {
            return DbError::Driver {
                code: Some(DENIED.to_owned()),
                message: format!(
                    "the data-trigger guard cannot lock `{table}`, which this row operation writes through a foreign-key referential action ({e}). The deployment role needs INSERT, UPDATE, DELETE or TRUNCATE on it to hold its triggers still through the write."
                ),
            };
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
fn reached(statement: &Statement) -> String {
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
                       WHERE before_update.tgrelid = a.attrelid
                         AND (before_update.tgtype & 19) = 19
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
    )
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
/// doing and not a statement of its own.
async fn refuse_rules(conn: &mut Conn, statement: &Statement) -> Result<(), DbError> {
    if statement.rows_only {
        return Ok(());
    }
    let event = match statement.event {
        INSERT => '3',
        DELETE => '4',
        _ => '2',
    };
    let rows = conn
        .query(&format!(
            "{ctes}
             SELECT n.nspname AS schema_name, c.relname AS table_name, w.rulename AS name
             FROM relations r
             JOIN pg_catalog.pg_rewrite w ON w.ev_class = r.oid
             JOIN pg_catalog.pg_class c ON c.oid = r.oid
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE w.rulename <> '_RETURN' AND w.ev_type = '{event}'
               AND (w.ev_enabled = 'A' OR w.ev_enabled =
                    CASE WHEN pg_catalog.current_setting('session_replication_role') = 'replica'
                         THEN 'R' ELSE 'O' END)
             ORDER BY w.oid",
            ctes = reached(statement),
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
    Err(DbError::Driver {
        code: None,
        message: format!(
            "unsafe rewrite rule `{name}` on `{table}`: a rule decides what a write does, and this row operation reaches that table. The statements a rule adds are not part of the plan and their triggers cannot be authenticated, so the write is refused rather than executed as the deployment role. Remove the rule before planning again."
        ),
    })
}

async fn read_triggers(conn: &mut Conn, statement: &Statement) -> Result<Vec<Trigger>, DbError> {
    // Statement-level triggers fire on the relation the statement names, not on
    // each descendant it reaches; row-level ones fire wherever the row lands.
    let rows = conn
        .query(&format!(
            "{ctes}
             SELECT t.oid::int8 AS oid, t.tgfoid::int8 AS function_oid,
                    n.nspname AS schema_name, c.relname AS table_name,
                    t.tgname AS name, pg_catalog.pg_get_triggerdef(t.oid) AS definition,
                    NOT EXISTS (
                        SELECT 1 FROM pg_catalog.pg_roles editor
                        WHERE pg_catalog.pg_has_role(editor.oid, p.proowner, 'USAGE')
                          AND NOT pg_catalog.pg_has_role(editor.oid, current_user::regrole::oid, 'SET')
                    ) AS trusted_owner
             FROM pg_catalog.pg_trigger t
             JOIN relations r ON r.oid = t.tgrelid
             JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid
             JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
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
            ctes = reached(statement),
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
                trusted_owner: r.try_get("trusted_owner")?.ok_or_else(missing)?,
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
async fn can_move(conn: &mut Conn, statement: &Statement) -> Result<bool, DbError> {
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
            ctes = reached(statement),
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
             edges(constraint_oid, referencing, declared_on, cascades, written) AS (
                 SELECT con.oid, con.conrelid, declared.conrelid,
                        {events} = {delete} AND con.confdeltype = 'c',
                        CASE WHEN {events} = {delete} AND con.confdelsetcols IS NOT NULL
                                  AND con.confdelsetcols <> '{{}}'::pg_catalog.int2[]
                             THEN con.confdelsetcols ELSE con.conkey END
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
                       WHERE rewriter.tgrelid = con.confrelid
                         AND (rewriter.tgtype & 19) = 19
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
                   AND NOT EXISTS (
                       SELECT 1 FROM pg_catalog.pg_trigger action
                       WHERE action.tgconstraint = con.oid
                         AND action.tgrelid = con.confrelid
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
                    a.attname AS column_name
             FROM edges e
             LEFT JOIN pg_catalog.pg_attribute a
                    ON NOT e.cascades AND a.attrelid = e.referencing AND a.attnum = ANY(e.written)
             ORDER BY constraint_oid, relation, event",
            ctes = reached(statement),
            events = statement.event,
            delete = DELETE,
            update = UPDATE,
            gone = gone(dropped),
        ))
        .await?;
    // One statement per constraint: merging two constraints' column sets would
    // authenticate each against columns only the other one writes.
    let mut derived: BTreeMap<(i64, i64, i16), BTreeSet<String>> = BTreeMap::new();
    for row in &rows {
        let key = (
            row.try_get::<i64>("constraint_oid")?.ok_or_else(missing)?,
            row.try_get::<i64>("relation")?.ok_or_else(missing)?,
            row.try_get::<i16>("event")?.ok_or_else(missing)?,
        );
        let columns = derived.entry(key).or_default();
        if let Some(column) = row.try_get::<&str>("column_name")? {
            columns.insert(column.to_owned());
        }
    }
    Ok(derived
        .into_iter()
        .map(|((_, relation, event), columns)| Statement {
            relation,
            event,
            columns,
            partitions_only: true,
            rows_only: false,
        })
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
                literal(name)
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
            " `{table}` is written by a foreign-key referential action of the row operation on `{written}`, so its triggers run under the deployment role too."
        )
    } else {
        String::new()
    };
    DbError::Driver {
        code: None,
        message: format!(
            "unsafe data trigger `{name}` on `{table}`: reference-data writes require an unchanged recorded managed trigger whose function ownership rights are held only by roles that can act as the deployment role.{reached} Remove the trigger or establish that managed/trusted definition before planning again. `unmanaged: ignore` and `warn` do not authorize executing external trigger code."
        ),
    }
}

fn moved(relation: i64) -> DbError {
    DbError::Driver {
        code: None,
        message: format!(
            "a table reached through a foreign-key action (oid {relation}) was renamed or dropped while the data-trigger guard was locking it; nothing was written. Re-run the command."
        ),
    }
}

fn missing() -> DbError {
    DbError::Driver {
        code: None,
        message: "data trigger catalog read returned a missing value".to_owned(),
    }
}
