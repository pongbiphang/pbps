//! What stands on an enum a plan rebuilds under its uid, read from the catalog
//! (DEC-1635.7, measured on PostgreSQL 16.15 and 18.6).
//!
//! The rebuild moves every column of the type to a replacement and drops the
//! old type. The census is **catalog-driven** rather than a list of known
//! kinds: every `pg_depend` edge on the type and its array, and every column
//! of either, is classified, and one the plan neither moves nor rebuilds is
//! refused by name before any SQL. The engine never has to abort mid-plan on a
//! dependent this crate failed to list.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError, Param};
use pbps_diff::order::catalog::{Dependent, Holds, Part};
use pbps_model::{
    Change, ChangeSet, ColumnName, EnumRebuildStep, IdsFile, ManagedType, ModuleId, Schema,
    TableName, TypeRef,
};

fn refused(why: String) -> DbError {
    DbError::Refused(why)
}

fn quoted(name: &TypeRef) -> Result<String, DbError> {
    let part = |ident: &str| crate::quote(ident).map_err(|e| refused(e.to_string()));
    Ok(format!("{}.{}", part(name.schema())?, part(name.name())?))
}

/// What the plan does about what the catalog found.
#[derive(Debug, Default)]
pub struct Census {
    /// Declared modules the plan must rebuild around the retypes and does not
    /// yet: the connected planner plans again with them (`diff_rebuilding`).
    pub rebuild: BTreeSet<ModuleId>,
}

/// The comment of each enum, read before planning so the swap can carry it.
pub async fn comments(
    conn: &mut Conn,
    types: &[TypeRef],
) -> Result<BTreeMap<TypeRef, String>, DbError> {
    let mut out = BTreeMap::new();
    for name in types {
        let rows = conn
            .query_with(
                "SELECT pg_catalog.obj_description(t.oid, 'pg_type') AS comment
                   FROM pg_catalog.pg_type t
                  WHERE t.oid = pg_catalog.to_regtype($1)",
                &[Param::Str(&quoted(name)?)],
            )
            .await?;
        if let Some(comment) = rows
            .first()
            .map(|r| r.try_get::<&str>("comment"))
            .transpose()?
            .flatten()
        {
            out.insert(name.clone(), comment.to_owned());
        }
    }
    Ok(out)
}

struct Held {
    parent: Option<i64>,
    kind: String,
    schema: String,
    table: String,
    column: String,
}

/// Classify everything standing on each enum `changes` rebuilds. Inside the
/// planning transaction, so the answer holds until the plan's own reads, and
/// again inside the apply, after the lock and before the first statement.
///
/// `declared` is the project's desired schema at planning: a dependent module
/// is rebuilt only when the project holds it. At apply it is `None`, and a
/// dependent the plan does not already rebuild appeared after the plan was
/// made. `before` names a column as the database has it today, which a rename
/// in the same plan changes. `resuming` is a staged run past its first
/// statement, when the replacement may already stand.
pub async fn census(
    conn: &mut Conn,
    changes: &ChangeSet,
    declared: Option<&Schema>,
    before: &IdsFile,
    resuming: bool,
) -> Result<Census, DbError> {
    crate::modules::require_the_callers_transaction(conn).await?;
    let mut out = Census::default();
    let rebuilt: Vec<_> = changes
        .changes
        .iter()
        .filter_map(|p| {
            if let Change::EnumRebuild {
                uid,
                name,
                tmp,
                step: EnumRebuildStep::Create { definition, .. },
            } = &p.change
            {
                Some((
                    uid,
                    name,
                    tmp,
                    matches!(definition.as_ref(), ManagedType::Domain { .. }),
                ))
            } else {
                None
            }
        })
        .collect();
    // The types this plan rebuilds, which a type built on one of them need not
    // block: it is rebuilt too.
    let rebuilt_names: BTreeSet<String> = rebuilt
        .iter()
        .map(|(_, name, _, _)| format!("{}.{}", name.schema(), name.name()))
        .collect();
    for (uid, name, tmp, is_domain) in rebuilt {
        let noun = if is_domain { "domain" } else { "enum" };
        let shown = name.shown().to_string();
        let rows = conn
            .query_with(
                "SELECT t.oid::int8 AS oid, t.typarray::int8 AS array, t.typtype::text AS kind,
                        (pg_catalog.pg_get_userbyid(t.typowner) <> current_user) AS foreign_owner,
                        (t.typacl IS NOT NULL) AS has_acl,
                        (pg_catalog.to_regtype($2) IS NOT NULL) AS tmp_taken
                   FROM pg_catalog.pg_type t
                  WHERE t.oid = pg_catalog.to_regtype($1)",
                &[Param::Str(&quoted(name)?), Param::Str(&quoted(tmp)?)],
            )
            .await?;
        let Some(row) = rows.first() else {
            // A run past the drop has the replacement alone, and nothing is
            // left to block the rename.
            let standing = conn
                .query_with(
                    "SELECT (pg_catalog.to_regtype($1) IS NOT NULL) AS standing",
                    &[Param::Str(&quoted(tmp)?)],
                )
                .await?;
            if resuming
                && standing
                    .first()
                    .map(|r| r.try_get::<bool>("standing"))
                    .transpose()?
                    .flatten()
                    == Some(true)
            {
                continue;
            }
            return Err(refused(format!("{noun} {shown} is not in the database")));
        };
        let (ty, array) = (
            row.try_get::<i64>("oid")?.unwrap_or_default(),
            row.try_get::<i64>("array")?.unwrap_or_default(),
        );
        if row.try_get::<&str>("kind")?.unwrap_or_default() != if is_domain { "d" } else { "e" } {
            return Err(refused(format!(
                "{shown} is not a{} {noun} in the database",
                if is_domain { "" } else { "n" }
            )));
        }
        if row.try_get::<bool>("foreign_owner")?.unwrap_or_default() {
            return Err(refused(format!(
                "{noun} {shown} is owned by a role other than the one deploying; the replacement would not keep its owner, so the rebuild is refused"
            )));
        }
        if row.try_get::<bool>("has_acl")?.unwrap_or_default() {
            return Err(refused(format!(
                "{noun} {shown} has an explicit privilege list, which the replacement would not keep; the rebuild is refused"
            )));
        }
        if !resuming && row.try_get::<bool>("tmp_taken")?.unwrap_or_default() {
            return Err(refused(format!(
                "the type name {} that the rebuild of {shown} needs is taken in the database",
                tmp.shown()
            )));
        }

        // The columns the plan moves, as the database has them today.
        let mut moved: BTreeSet<(String, String, String)> = BTreeSet::new();
        for planned in &changes.changes {
            if let Change::EnumRebuild {
                uid: u,
                step:
                    EnumRebuildStep::Retype {
                        column_uid, column, ..
                    },
                ..
            } = &planned.change
                && u == uid
            {
                let live = before.columns.get(column_uid).unwrap_or(column);
                moved.insert((
                    live.table.schema.clone(),
                    live.table.name.clone(),
                    live.name.raw().to_owned(),
                ));
            }
        }

        // 1. Every column that holds the type or its array.
        let mut held = BTreeMap::new();
        // The columns that follow a moved one down its partitions and
        // inheritance children: the engine retypes them with the root, so
        // what stands on them comes down with it.
        let mut follow: BTreeSet<(String, String, String)> = BTreeSet::new();
        let mut names: BTreeMap<i64, TableName> = BTreeMap::new();
        for row in conn
            .query_with(
                "SELECT a.attrelid::int8 AS relation, c.relkind::text AS kind,
                        n.nspname AS schema, c.relname AS name, a.attname AS column,
                        (SELECT i.inhparent::int8 FROM pg_catalog.pg_inherits i
                          WHERE i.inhrelid = c.oid LIMIT 1) AS parent
                   FROM pg_catalog.pg_attribute a
                   JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                  WHERE a.atttypid IN (($1::int8)::oid, ($2::int8)::oid)
                    AND a.attnum > 0 AND NOT a.attisdropped
                  ORDER BY 3, 4, 5",
                &[Param::I64(ty), Param::I64(array)],
            )
            .await?
        {
            let relation = row.try_get::<i64>("relation")?.unwrap_or_default();
            let text = |c: &str| -> Result<String, DbError> {
                Ok(row.try_get::<&str>(c)?.unwrap_or_default().to_owned())
            };
            names.insert(relation, TableName::new(text("schema")?, text("name")?));
            held.insert(
                (relation, text("column")?),
                Held {
                    parent: row.try_get::<i64>("parent")?,
                    kind: text("kind")?,
                    schema: text("schema")?,
                    table: text("name")?,
                    column: text("column")?,
                },
            );
        }
        // A partition's default is its parent's, copied: the plan drops it
        // where the declaration holds it, at the root.
        let parents: BTreeMap<TableName, TableName> = held
            .values()
            .filter_map(|h| {
                let parent = names.get(&h.parent?)?;
                Some((TableName::new(&h.schema, &h.table), parent.clone()))
            })
            .collect();
        let renamed = Renamed::of(changes);
        for h in held.values() {
            let shown_column = format!("{}.{}.{}", h.schema, h.table, h.column);
            // A partition's column follows its root's retype; the root is the
            // ancestor that is not itself a partition.
            let root = root_of(&held, h);
            let reached =
                moved.contains(&(root.schema.clone(), root.table.clone(), root.column.clone()));
            let gone = gone_before_the_swap(
                changes,
                &renamed,
                &parents,
                &TableName::new(&h.schema, &h.table),
                &ColumnName::from(h.column.as_str()),
            );
            let what: String = match h.kind.as_str() {
                "r" | "p" if gone => continue,
                "r" | "p" if reached => {
                    follow.insert((h.schema.clone(), h.table.clone(), h.column.clone()));
                    continue;
                }
                // An index holds the type in its key; the engine rebuilds it
                // with the table, and one with constants of the type is
                // judged below as a dependent.
                "i" | "I" => continue,
                "r" | "p" => "a table the declarations do not retype".into(),
                "v" => {
                    let id = ModuleId::Named(pbps_model::ObjectName::new(&h.schema, &h.table));
                    match module_verdict(&id, changes, declared) {
                        Verdict::Fine => continue,
                        Verdict::Rebuild(id) => {
                            out.rebuild.insert(id);
                            continue;
                        }
                        Verdict::Refuse(why) => why,
                    }
                }
                "m" => "a materialized view, which no declaration holds".into(),
                "c" => {
                    "a composite type's attribute, which cannot move to a replacement type".into()
                }
                "f" => "a foreign table".into(),
                _ => "a relation of a kind pbps cannot rebuild around".into(),
            };
            return Err(refused(format!(
                "column {shown_column} holds {noun} {shown}, and it is {what}; the rebuild of {shown} is refused"
            )));
        }

        // Signature and argument-default edges are pg_proc dependents too:
        // the shared reader identifies their overload with format_type and
        // module_verdict sends them through the ordinary drop/recreate path
        // (DEC-2392.1), including its owner and ACL checks.
        // Everything else on the type is classified by the same census.
        let mut seen: Vec<(Level, Dependent)> = Vec::new();
        for dependent in crate::modules::type_dependents(conn, ty, array).await? {
            // A column that holds the type was judged above, whether the
            // reader names it a column or, for a partition's, its parent.
            let column = dependent.described.starts_with("column ")
                || matches!(&dependent.holds, Holds::Unrepresentable(why)
                    if why.contains("a relation of a kind this reader does not know"));
            if !column {
                seen.push((Level::Type, dependent));
            }
        }
        // Everything on a column the plan moves that the engine does not
        // rebuild: views, bodies, triggers, policies, generated columns.
        for (schema, table, column) in moved.union(&follow) {
            let rows = conn
                .query_with(
                    "SELECT a.attrelid::int8 AS relation, a.attnum::int8 AS num
                       FROM pg_catalog.pg_attribute a
                       JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
                       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3
                        AND NOT a.attisdropped",
                    &[Param::Str(schema), Param::Str(table), Param::Str(column)],
                )
                .await?;
            let Some(row) = rows.first() else {
                return Err(refused(format!(
                    "column {schema}.{table}.{column} is not in the database"
                )));
            };
            seen.extend(
                crate::modules::column_dependents(
                    conn,
                    row.try_get::<i64>("relation")?.unwrap_or_default(),
                    row.try_get::<i64>("num")?.unwrap_or_default(),
                )
                .await?
                .into_iter()
                .map(|d| (Level::Column, d)),
            );
        }
        let mut judged = BTreeSet::new();
        for (level, dependent) in seen {
            if !judged.insert(dependent.described.clone()) {
                continue;
            }
            // A domain built on the type is rebuilt by this plan too, over the
            // replacement; it holds nothing the swap drops (DEC-1635.9).
            // So are the domain's own checks, which stand on it and go with it
            // (the dependent names the domain it is a check of).
            let on_a_rebuilt_domain = |of: &str| rebuilt_names.contains(&of.replace('"', ""));
            if level == Level::Type
                && (dependent
                    .described
                    .strip_prefix("type ")
                    .is_some_and(on_a_rebuilt_domain)
                    || constraint_owner(&dependent.holds)
                        .as_deref()
                        .is_some_and(on_a_rebuilt_domain))
            {
                continue;
            }
            match verdict(level, &dependent, changes, declared, &parents) {
                Verdict::Fine => {}
                Verdict::Rebuild(id) => {
                    out.rebuild.insert(id);
                }
                Verdict::Refuse(why) => {
                    return Err(refused(format!(
                        "{} stands on {noun} {shown} ({why}); the rebuild of {shown} is refused",
                        dependent.described
                    )));
                }
            }
        }
    }
    Ok(out)
}

/// The domain a dependent constraint belongs to, when the dependent is a check
/// of a domain rather than of a table (its text is `modules.rs`'s).
fn constraint_owner(holds: &Holds) -> Option<String> {
    match holds {
        Holds::Unrepresentable(why) => why
            .strip_prefix("a constraint on domain ")
            .and_then(|rest| rest.split_once(" rather than on a table"))
            .map(|(domain, _)| domain.to_owned()),
        Holds::Module(_) | Holds::TablePart { .. } => None,
    }
}

/// The column a partition's or child's follows when its root is retyped: the
/// topmost ancestor that holds the same column.
fn root_of<'a>(held: &'a BTreeMap<(i64, String), Held>, h: &'a Held) -> &'a Held {
    let mut root = h;
    while let Some(parent) = root.parent {
        match held.get(&(parent, h.column.clone())) {
            Some(up) if h.kind == "r" || h.kind == "p" => root = up,
            _ => break,
        }
    }
    root
}

/// Whether the plan drops a column that holds the type, by its own drop, with
/// its table, or with the table it is a partition of. Those drops come before
/// the replacement is created, so by the swap the column no longer holds the
/// old type and is neither retyped nor refused.
fn gone_before_the_swap(
    changes: &ChangeSet,
    renamed: &Renamed,
    parents: &BTreeMap<TableName, TableName>,
    table: &TableName,
    column: &ColumnName,
) -> bool {
    let mut owner = Some(table.clone());
    for _ in 0..32 {
        let Some(table) = owner else { break };
        if renamed.drops_column(changes, &table, column) {
            return true;
        }
        owner = parents.get(&table).cloned();
    }
    false
}

/// Where a dependent was found: on the type, by a constant of it in an
/// expression, or on a column the plan moves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Type,
    Column,
}

enum Verdict {
    /// The engine rebuilds it, or the plan already drops and restores it.
    Fine,
    Rebuild(ModuleId),
    Refuse(String),
}

/// The names the plan gives what the catalog still has under its old ones. The
/// census reads the database before the plan runs, but the plan's own drops are
/// written against the names the declarations give, so a table or column the
/// same plan renames is looked up here before it is matched.
#[derive(Default)]
struct Renamed {
    tables: BTreeMap<TableName, TableName>,
    columns: BTreeMap<(TableName, ColumnName), ColumnName>,
}

impl Renamed {
    fn of(changes: &ChangeSet) -> Self {
        let mut out = Self::default();
        for planned in &changes.changes {
            if let Change::RenameTable { from, to, .. } = &planned.change {
                out.tables.insert(from.clone(), to.clone());
            }
            // A column rename names its table as the plan leaves it; the
            // catalog has the column under the table's name from before, when
            // the plan renamed that too.
            if let Change::RenameColumn {
                table,
                from,
                to,
                table_was,
                ..
            } = &planned.change
            {
                out.columns.insert(
                    (
                        table_was.clone().unwrap_or_else(|| table.clone()),
                        from.clone(),
                    ),
                    to.clone(),
                );
            }
        }
        out
    }

    fn table(&self, live: &TableName) -> TableName {
        self.tables.get(live).unwrap_or(live).clone()
    }

    fn column(&self, live: &TableName, column: &ColumnName) -> ColumnName {
        self.columns
            .get(&(live.clone(), column.clone()))
            .unwrap_or(column)
            .clone()
    }

    /// Whether the plan removes `table`, a table as the database has it today.
    fn drops_table(&self, changes: &ChangeSet, table: &TableName) -> bool {
        let target = self.table(table);
        changes.changes.iter().any(
            |p| matches!(&p.change, Change::DropTable { name, .. } if *name == target || name == table),
        )
    }

    /// Whether the plan removes `column` of `table` (as the database has them
    /// today), by its own `DropColumn` or with its table. Both come before the
    /// replacement is created, so by the swap nothing of it holds the type:
    /// neither retyped nor refused (ORDERING classes 5 and 6, before 7 and the
    /// swap).
    fn drops_column(&self, changes: &ChangeSet, table: &TableName, column: &ColumnName) -> bool {
        let target = self.table(table);
        let named = self.column(table, column);
        self.drops_table(changes, table)
            || changes.changes.iter().any(|p| {
                matches!(&p.change, Change::DropColumn { column: c, .. }
                    if c.table == target && c.name == named)
            })
    }
}

/// Whether the plan already drops `part` of `table`, a table as the database
/// has it today.
fn drops(changes: &ChangeSet, table: &TableName, part: &Part) -> bool {
    let renamed = Renamed::of(changes);
    let target = renamed.table(table);
    // A table's parts go with it, and a column's default with the column.
    if renamed.drops_table(changes, table) {
        return true;
    }
    if let Part::Default(c) = part
        && renamed.drops_column(changes, table, c)
    {
        return true;
    }
    changes.changes.iter().any(|p| match (&p.change, part) {
        (Change::DropCheck { table: t, name }, Part::Check(n)) => *t == target && name == n,
        (Change::DropIndex { table: t, name }, Part::Index(n)) => *t == target && name == n,
        (
            Change::AlterColumnDefault {
                column, to: None, ..
            },
            Part::Default(c),
        ) => column.table == target && column.name == renamed.column(table, c),
        _ => false,
    })
}

/// A module standing on a moved column comes down before the retypes and goes
/// back after the swap: dropped by the plan, it already does. One the plan
/// only restates in place would run after the retype with the old text
/// standing at it, so it joins the rebuilt set and the plan is made again with
/// its drop and create around the retypes, the create from the desired text.
/// Where nothing is declared, at apply, that second plan is the only one that
/// is admitted.
fn module_verdict(id: &ModuleId, changes: &ChangeSet, declared: Option<&Schema>) -> Verdict {
    let planned = |wanted: fn(&Change) -> Option<&ModuleId>| {
        changes
            .changes
            .iter()
            .any(|p| wanted(&p.change).is_some_and(|m| m == id))
    };
    let drops = planned(|c| {
        if let Change::DropModule { id, .. } = c {
            Some(id)
        } else {
            None
        }
    });
    let restates = planned(|c| {
        if let Change::AlterModule { id, .. } = c {
            Some(id)
        } else {
            None
        }
    });
    match declared {
        _ if drops => Verdict::Fine,
        Some(d) if restates || d.modules.contains_key(id) => Verdict::Rebuild(id.clone()),
        Some(_) => Verdict::Refuse("a module the project does not declare".into()),
        None if restates => {
            Verdict::Refuse("a module the plan restates in place, after the retype".into())
        }
        None => Verdict::Refuse("a module that appeared after the plan was made".into()),
    }
}

fn verdict(
    level: Level,
    dependent: &Dependent,
    changes: &ChangeSet,
    declared: Option<&Schema>,
    parents: &BTreeMap<TableName, TableName>,
) -> Verdict {
    match &dependent.holds {
        Holds::Module(id) => module_verdict(id, changes, declared),
        Holds::TablePart { table, part } => match part {
            // Dropped and restored from the declaration, which the engine
            // would refuse to retype beside: constants typed as the old enum.
            // On a column, they only name it, and the engine rebuilds them.
            Part::Check(_) | Part::Index(_) | Part::Default(_) => {
                // A partition's default is its parent's, copied: the plan
                // drops it where the declaration holds it, at the root.
                let mut owner = table.clone();
                for _ in 0..32 {
                    match parents.get(&owner) {
                        Some(parent) if matches!(part, Part::Default(_)) => {
                            owner = parent.clone();
                        }
                        _ => break,
                    }
                }
                if level == Level::Column || drops(changes, &owner, part) {
                    Verdict::Fine
                } else if matches!(part, Part::Index(_) | Part::Check(_)) {
                    Verdict::Refuse(
                        "the plan does not drop and restore it, which its constants of the old type require"
                            .into(),
                    )
                } else {
                    Verdict::Refuse("a default the plan does not restore".into())
                }
            }
            Part::Policy(_) => Verdict::Refuse("a policy, which a rebuild does not restore".into()),
            Part::Generated(_) => {
                Verdict::Refuse("a generated column, which PostgreSQL cannot retype".into())
            }
            Part::PrimaryKey(_) | Part::Unique(_) | Part::ForeignKey(_) => Verdict::Fine,
        },
        Holds::Unrepresentable(why) => {
            // Keys, unique and exclusion constraints and extended statistics
            // are rebuilt by the engine itself; a foreign key is declared and
            // restored by the plan.
            if why.contains("a constraint of a kind that is not a check")
                || why.contains("`pg_catalog.pg_statistic_ext`")
                || why.contains("`pg_statistic_ext`")
            {
                Verdict::Fine
            } else {
                Verdict::Refuse(why.clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Module, ModuleKind, ObjectName, PlannedChange};

    fn dependent(holds: Holds) -> Dependent {
        Dependent {
            described: "something".into(),
            holds,
        }
    }

    fn plan(changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        }
    }

    fn view(name: &str) -> ModuleId {
        ModuleId::Named(ObjectName::new("app", name))
    }

    fn declaring(id: &ModuleId) -> Schema {
        Schema {
            modules: [(
                id.clone(),
                Module {
                    kind: ModuleKind::View,
                    description: None,
                    definition: "SELECT 1".into(),
                    options: Default::default(),
                },
            )]
            .into(),
            ..Default::default()
        }
    }

    fn part(part: Part) -> Holds {
        Holds::TablePart {
            table: TableName::new("app", "t"),
            part,
        }
    }

    #[test]
    fn a_domains_own_check_names_its_domain_and_nothing_else_does() {
        let check = Holds::Unrepresentable(
            "a constraint on domain app.d rather than on a table, which this model does not hold"
                .into(),
        );
        assert_eq!(constraint_owner(&check).as_deref(), Some("app.d"));
        // Another reason, and a table's own part, name no domain.
        assert_eq!(
            constraint_owner(&Holds::Unrepresentable("a materialized view".into())),
            None
        );
        assert_eq!(constraint_owner(&part(Part::Check("c".into()))), None);
    }

    #[test]
    fn routine_signature_dependents_use_the_module_rebuild_verdict() {
        // The catalog reader supplies identity arguments only: return/OUT
        // types and defaults create edges without adding identity arguments.
        for name in [
            "app.f(app.mood)",
            "app.f(app.mood[])",
            "app.arr()",
            "app.outarg()",
            "app.default_only(text)",
            "app.p(app.mood)",
        ] {
            let id: ModuleId = name.parse().unwrap();
            let held = dependent(Holds::Module(id.clone()));
            let mut schema = declaring(&id);
            let kind = if name.starts_with("app.p(") {
                ModuleKind::Procedure
            } else {
                ModuleKind::Function
            };
            schema.modules.get_mut(&id).unwrap().kind = kind;
            assert!(matches!(
                verdict(Level::Type, &held, &plan(vec![]), Some(&schema), &Default::default()),
                Verdict::Rebuild(found) if found == id
            ));
            // An overload or another schema's routine is not a declaration
            // of this identity, and apply cannot discover new rebuild intent.
            assert!(matches!(
                verdict(
                    Level::Type,
                    &held,
                    &plan(vec![]),
                    Some(&declaring(&view("f"))),
                    &Default::default()
                ),
                Verdict::Refuse(_)
            ));
            assert!(matches!(
                verdict(Level::Type, &held, &plan(vec![]), None, &Default::default()),
                Verdict::Refuse(_)
            ));
            let dropped = plan(vec![Change::DropModule {
                id: id.clone(),
                kind,
            }]);
            assert!(matches!(
                verdict(Level::Type, &held, &dropped, None, &Default::default()),
                Verdict::Fine
            ));
        }
    }

    #[test]
    fn a_declared_module_is_rebuilt_and_an_undeclared_one_is_refused() {
        let id = view("v");
        let kinds = [
            (Level::Type, Holds::Module(id.clone())),
            (Level::Column, Holds::Module(id.clone())),
        ];
        for (level, holds) in kinds {
            let v = verdict(
                level,
                &dependent(holds.clone()),
                &plan(vec![]),
                Some(&declaring(&id)),
                &Default::default(),
            );
            assert!(matches!(&v, Verdict::Rebuild(m) if *m == id));
            // At apply nothing is declared: a module the plan does not already
            // rebuild appeared after it was made, and one it rebuilds stands.
            let v = verdict(
                level,
                &dependent(holds.clone()),
                &plan(vec![]),
                None,
                &Default::default(),
            );
            assert!(matches!(v, Verdict::Refuse(why) if why.contains("after the plan")));
            let rebuilt = plan(vec![Change::DropModule {
                id: id.clone(),
                kind: ModuleKind::View,
            }]);
            let v = verdict(
                level,
                &dependent(holds.clone()),
                &rebuilt,
                None,
                &Default::default(),
            );
            assert!(matches!(v, Verdict::Fine));
            // A module the plan only restates in place runs after the retype:
            // it is rebuilt around them at planning, and refused as it stands
            // at apply.
            let restated = plan(vec![Change::AlterModule {
                id: id.clone(),
                module: Box::new(Module {
                    kind: ModuleKind::View,
                    description: None,
                    definition: "SELECT 2".into(),
                    options: Default::default(),
                }),
            }]);
            let v = verdict(
                level,
                &dependent(holds.clone()),
                &restated,
                Some(&Schema::default()),
                &Default::default(),
            );
            assert!(matches!(&v, Verdict::Rebuild(m) if *m == id));
            let v = verdict(
                level,
                &dependent(holds.clone()),
                &restated,
                None,
                &Default::default(),
            );
            assert!(matches!(v, Verdict::Refuse(why) if why.contains("restates")));
            let v = verdict(
                level,
                &dependent(holds),
                &plan(vec![]),
                Some(&Schema::default()),
                &Default::default(),
            );
            assert!(matches!(v, Verdict::Refuse(why) if why.contains("does not declare")));
        }
    }

    #[test]
    fn constants_of_the_old_type_need_the_drop_the_plan_makes_and_a_column_alone_does_not() {
        let table = TableName::new("app", "t");
        let drop_check = Change::DropCheck {
            table: table.clone(),
            name: "c".into(),
        };
        let drop_default = Change::AlterColumnDefault {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "m", 0),
            column: table.column("m"),
            from: Some("'a'".into()),
            to: None,
        };
        let planned = plan(vec![drop_check, drop_default]);
        let none = plan(vec![]);
        let schema = Schema::default();
        for held in [
            part(Part::Check("c".into())),
            part(Part::Default("m".into())),
        ] {
            let fine = |p: &ChangeSet, level| {
                matches!(
                    verdict(
                        level,
                        &dependent(held.clone()),
                        p,
                        Some(&schema),
                        &Default::default(),
                    ),
                    Verdict::Fine
                )
            };
            // Typed constants refuse the retype unless the plan drops them.
            assert!(fine(&planned, Level::Type));
            assert!(!fine(&none, Level::Type));
            // A name on the column alone is the engine's to rebuild.
            assert!(fine(&none, Level::Column));
        }
        // An index, and a check the plan drops under another name, are refused.
        assert!(matches!(
            verdict(
                Level::Type,
                &dependent(part(Part::Index("i".into()))),
                &planned,
                Some(&schema),
                &Default::default()
            ),
            Verdict::Refuse(_)
        ));
        assert!(matches!(
            verdict(
                Level::Type,
                &dependent(part(Part::Check("other".into()))),
                &planned,
                Some(&schema),
                &Default::default()
            ),
            Verdict::Refuse(_)
        ));
    }

    #[test]
    fn a_default_the_plan_drops_under_the_names_it_renames_to_is_the_one_the_catalog_has() {
        // The catalog has `app.old.m`; the plan renames the table and the column
        // and drops the default under the names it leaves.
        let live = TableName::new("app", "old");
        let target = TableName::new("app", "new");
        let rename_table = Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "t", 0),
            from: live.clone(),
            to: target.clone(),
            defaults: vec![],
        };
        let rename_column = Change::RenameColumn {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "m", 0),
            table: target.clone(),
            from: "m".into(),
            to: "mood".into(),
            table_was: Some(live.clone()),
        };
        let drop_default = |table: &TableName, column: &str| Change::AlterColumnDefault {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "m", 0),
            column: table.column(column),
            from: Some("'a'".into()),
            to: None,
        };
        let held = Holds::TablePart {
            table: live.clone(),
            part: Part::Default("m".into()),
        };
        let judge = |changes: Vec<Change>| {
            verdict(
                Level::Type,
                &dependent(held.clone()),
                &plan(changes),
                Some(&Schema::default()),
                &Default::default(),
            )
        };
        assert!(matches!(
            judge(vec![
                rename_table.clone(),
                rename_column.clone(),
                drop_default(&target, "mood"),
            ]),
            Verdict::Fine
        ));
        // A table rename alone leaves the column's name.
        assert!(matches!(
            judge(vec![rename_table.clone(), drop_default(&target, "m")]),
            Verdict::Fine
        ));
        // A real missing restore is still refused beside an unrelated rename,
        // and so is a drop of another table's default or another column's.
        assert!(matches!(
            judge(vec![rename_table.clone(), rename_column.clone()]),
            Verdict::Refuse(_)
        ));
        assert!(matches!(
            judge(vec![
                rename_table.clone(),
                rename_column.clone(),
                drop_default(&target, "m"),
            ]),
            Verdict::Refuse(_)
        ));
        assert!(matches!(
            judge(vec![
                rename_table,
                rename_column,
                drop_default(&live, "mood")
            ]),
            Verdict::Refuse(_)
        ));
    }

    #[test]
    fn a_check_and_an_index_on_a_renamed_table_are_matched_by_the_name_it_leaves() {
        let live = TableName::new("app", "old");
        let target = TableName::new("app", "new");
        let rename = Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "t", 0),
            from: live.clone(),
            to: target.clone(),
            defaults: vec![],
        };
        let at = |table: &TableName| {
            [
                Change::DropCheck {
                    table: table.clone(),
                    name: "c".into(),
                },
                Change::DropIndex {
                    table: table.clone(),
                    name: "i".into(),
                },
            ]
        };
        for part in [Part::Check("c".into()), Part::Index("i".into())] {
            let held = Holds::TablePart {
                table: live.clone(),
                part,
            };
            let judge = |changes: Vec<Change>| {
                verdict(
                    Level::Type,
                    &dependent(held.clone()),
                    &plan(changes),
                    Some(&Schema::default()),
                    &Default::default(),
                )
            };
            let mut renamed = vec![rename.clone()];
            renamed.extend(at(&target));
            assert!(matches!(judge(renamed), Verdict::Fine));
            // Without the rename the drop names another table.
            assert!(matches!(judge(at(&target).to_vec()), Verdict::Refuse(_)));
        }
    }

    fn drop_column(table: &str, column: &str) -> Change {
        Change::DropColumn {
            uid: pbps_model::Uid::generate(pbps_model::UidKind::Column),
            column: TableName::new("app", table).column(column),
        }
    }

    fn drop_table(table: &str) -> Change {
        Change::DropTable {
            uid: pbps_model::Uid::generate(pbps_model::UidKind::Table),
            name: TableName::new("app", table),
            detach_from: None,
        }
    }

    fn gone(changes: &ChangeSet, table: &str, column: &str) -> bool {
        gone_before_the_swap(
            changes,
            &Renamed::of(changes),
            &Default::default(),
            &TableName::new("app", table),
            &column.into(),
        )
    }

    /// Dropped before the replacement exists, the column is not a blocker. The
    /// negatives matter as much: a column the plan keeps stays refused by
    /// name, and a drop on another table or of another column excuses nothing.
    #[test]
    fn a_column_the_plan_drops_before_the_swap_is_not_a_blocker_and_another_is() {
        let drops = plan(vec![drop_column("t", "m")]);
        assert!(gone(&drops, "t", "m"));
        assert!(!gone(&drops, "t", "other"), "another column of the table");
        assert!(!gone(&drops, "u", "m"), "the same column name elsewhere");
        assert!(!gone(&plan(vec![]), "t", "m"), "a column nothing drops");
        // Named as the plan leaves the table, which the catalog has under its
        // old name.
        let renamed = plan(vec![
            Change::RenameTable {
                uid: pbps_model::Uid::generate(pbps_model::UidKind::Table),
                from: TableName::new("app", "t"),
                to: TableName::new("app", "tt"),
                defaults: vec![],
            },
            drop_column("tt", "m"),
        ]);
        assert!(gone(&renamed, "t", "m"));
    }

    #[test]
    fn a_table_the_plan_drops_takes_its_columns_and_its_partitions_with_it() {
        let drops = plan(vec![drop_table("t")]);
        assert!(gone(&drops, "t", "m"));
        assert!(!gone(&drops, "u", "m"), "a drop of another table");
        let parents: BTreeMap<TableName, TableName> =
            [(TableName::new("app", "t_1"), TableName::new("app", "t"))].into();
        let through_parent = |changes: &ChangeSet, table: &str| {
            gone_before_the_swap(
                changes,
                &Renamed::of(changes),
                &parents,
                &TableName::new("app", table),
                &"m".into(),
            )
        };
        assert!(
            through_parent(&drops, "t_1"),
            "a partition of the dropped table"
        );
        assert!(!through_parent(&plan(vec![drop_table("u")]), "t_1"));
        assert!(!through_parent(&plan(vec![]), "t_1"));
    }

    /// What stands on a dropped column or table goes with it: the plan need not
    /// also drop and restore it.
    #[test]
    fn a_default_check_or_index_goes_with_its_dropped_column_or_table() {
        let table = TableName::new("app", "t");
        let default = Part::Default("m".into());
        let check = Part::Check("c".into());
        let index = Part::Index("i".into());
        let column = plan(vec![drop_column("t", "m")]);
        assert!(drops(&column, &table, &default));
        assert!(!drops(&column, &table, &Part::Default("other".into())));
        // A check or index is dropped by its own change; a column's drop alone
        // does not say which of them it takes.
        assert!(!drops(&column, &table, &check));
        let whole = plan(vec![drop_table("t")]);
        for part in [&default, &check, &index] {
            assert!(drops(&whole, &table, part));
            assert!(!drops(&whole, &TableName::new("app", "u"), part));
        }
    }

    #[test]
    fn a_partitions_column_follows_its_root_and_a_plain_table_does_not() {
        let held = |relation: i64, parent: Option<i64>, kind: &str, table: &str| {
            (
                (relation, "m".to_owned()),
                Held {
                    parent,
                    kind: kind.into(),
                    schema: "app".into(),
                    table: table.into(),
                    column: "m".into(),
                },
            )
        };
        let all: BTreeMap<_, _> = [
            held(1, None, "p", "root"),
            held(2, Some(1), "p", "mid"),
            held(3, Some(2), "r", "leaf"),
            held(4, None, "r", "alone"),
            // Its parent holds no such column: nothing to follow.
            held(5, Some(9), "r", "orphan"),
        ]
        .into();
        let root = |relation: i64| {
            root_of(&all, &all[&(relation, "m".to_owned())])
                .table
                .clone()
        };
        assert_eq!(root(3), "root");
        assert_eq!(root(2), "root");
        assert_eq!(root(4), "alone");
        assert_eq!(root(5), "orphan");
    }

    #[test]
    fn what_the_rebuild_cannot_restore_is_refused_whatever_the_level() {
        let schema = Schema::default();
        let none = plan(vec![]);
        for level in [Level::Type, Level::Column] {
            for held in [
                part(Part::Policy("p".into())),
                part(Part::Generated("g".into())),
                Holds::Unrepresentable("a dependent in `pg_catalog.pg_publication_rel`, which this reader has no rule for".into()),
                Holds::Unrepresentable("a materialized view, which this model does not hold".into()),
                Holds::Unrepresentable("a dependent in `pg_type`, which this reader has no rule for".into()),
            ] {
                assert!(
                    matches!(
                        verdict(level, &dependent(held.clone()), &none, Some(&schema), &Default::default()),
                        Verdict::Refuse(_)
                    ),
                    "{held:?}"
                );
            }
            // Keys, unique and exclusion constraints and extended statistics
            // are the engine's own to rebuild.
            for held in [
                Holds::Unrepresentable("a constraint of a kind that is not a check".into()),
                Holds::Unrepresentable(
                    "a dependent in `pg_statistic_ext`, which this reader has no rule for".into(),
                ),
                part(Part::Unique("u".into())),
            ] {
                assert!(
                    matches!(
                        verdict(
                            level,
                            &dependent(held.clone()),
                            &none,
                            Some(&schema),
                            &Default::default()
                        ),
                        Verdict::Fine
                    ),
                    "{held:?}"
                );
            }
        }
    }
}
