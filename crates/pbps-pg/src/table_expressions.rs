//! The admission predicate at the column-level expressions a plan introduces on
//! a table: CHECK, DEFAULT and generated columns (#2563, DEC-2563.4).
//!
//! Before this, a table's expressions were only change-detected (DEC-319.1): a
//! function any role owned ran as the deployer. Each change that carries one is
//! bound here, in a transaction that is rolled back, the way a requirement is:
//! a temporary table with the table's columns and the one expression, whose
//! stored tree names every routine it calls. The routines are judged by the one
//! predicate ([`crate::input_path::judge_trees`]), so what a human reviews and
//! pins is what the engine would have run.
//!
//! The same function serves three moments. `plan` calls it to learn which
//! routines the review admits, so the plan can show and pin them; apply calls it
//! before the pre-flight probes, which evaluate these expressions over the
//! stored rows; and apply calls it again before each statement that introduces
//! an expression, when everything the plan created earlier exists. That last
//! call is what judges a routine or a type the same plan creates: the earlier
//! two cannot bind it, and say so as a note rather than refuse.

use pbps_db::{Conn, Param};
use pbps_model::{Change, ManagedType, TableName, TypeRef, ValueSource};

use crate::admission::Admission;
use crate::input_path::{Part, Use, judge_trees};
use crate::pins::Within;

/// The savepoint the early binding runs inside. The binding table itself is
/// named for the table it stands for ([`Spec::bound`]).
const TEMP: &str = "pbps_table_expressions";

/// The placeholder name of the column an existing column's new expression is
/// bound as; [`name_bound_columns`] replaces it before anything runs.
const BIND: &str = "pbps_bound";

/// The domain a created domain's code is bound as. Another name than [`TEMP`],
/// which a table's row type would share the namespace with.
const TEMP_DOMAIN: &str = "pbps_domain_expressions";

/// The policy bound on the binding table, which lives in the session's own
/// `pg_temp` beside a table that copies no policy.
const TEMP_POLICY: &str = "pbps_table_expressions_policy";

/// When the expressions are judged, which decides what an unbindable one means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// Plan time and the pre-flight: objects the plan creates do not exist yet,
    /// so an expression that does not bind is a note, judged later.
    Early,
    /// Just before the statement that introduces them: whatever the plan
    /// created earlier exists, so an expression that does not bind is refused
    /// rather than run unjudged.
    Statement,
}

/// The statement shape of one change's expressions.
struct Spec {
    /// What a refusal calls them: `the CHECK `positive` of app.t`.
    what: String,
    /// The table they belong to, as a refusal shows it.
    table: String,
    /// The schema whose path the statement that carries them runs on.
    schema: String,
    /// The name of the table they belong to. The binding table takes it, in
    /// `pg_temp`, so a column an expression qualifies with the table's name
    /// (`CHECK (t.a > 0)`) resolves as it does on the real table. A table of
    /// that name already in the session's `pg_temp` makes the binding fail to
    /// build, which is an expression that cannot be bound, never a table
    /// dropped.
    bound: String,
    /// The real table whose row type a column of the binding table, named like
    /// the table, carries ([`takes_row_type`]). A bare `t` in an expression is
    /// the row of the table. On the binding table it would be the row of the
    /// binding table, a type no function takes, so a policy
    /// `USING (app.visible(t))` that PostgreSQL accepts on the real table would
    /// not resolve. A column of that name is found before the row and has the
    /// type the function takes. `t.*` is not covered: it names the relation, and
    /// a relation's row type is always its own (DEC-2563.7).
    whole_row: Option<TableName>,
    /// The table the columns are copied from, or `None` for a table the plan
    /// creates, whose columns are all listed.
    like: Option<TableName>,
    /// Name in the temporary table, the name to show, the type, the value.
    columns: Vec<Column>,
    checks: Vec<(String, String)>,
    /// An index whose expressions are bound, by name.
    index: Option<(String, Box<pbps_model::Index>)>,
    /// A policy created on the binding table, by name.
    policy: Option<(String, Box<pbps_model::RowPolicy>)>,
}

/// One CHECK of the binding table and what its tree is called in a refusal.
struct Constraint {
    name: String,
    expression: String,
    label: String,
}

struct Column {
    bound: String,
    shown: String,
    ty: String,
    value: Option<Value>,
}

enum Value {
    Default(String),
    Generated(String),
}

impl Spec {
    /// The binding table as SQL spells it.
    fn relation(&self) -> Result<String, String> {
        Ok(format!(
            "pg_temp.{}",
            crate::quote(&self.bound).map_err(|e| e.to_string())?
        ))
    }

    /// The same, as the text of a `regclass` literal.
    fn regclass(&self) -> Result<String, String> {
        Ok(format!(
            "'{}'::pg_catalog.regclass",
            self.relation()?.replace('\'', "''")
        ))
    }

    /// Everything bound as a CHECK. Binding a generated column, an index key
    /// or an index filter as the column or index it is would let the engine
    /// fold the expression while it prepares the object: measured on 16 and
    /// 18, a call of an IMMUTABLE function with constant arguments runs then,
    /// before anything has judged it. A CHECK is stored as written. A generated
    /// expression is bound cast to the column's type, which is where its
    /// assignment coercion would put any cast routine; a key is bound as
    /// `IS NOT NULL`, which adds a node and no routine.
    fn constraints(&self) -> Vec<Constraint> {
        let mut taken: std::collections::BTreeSet<String> =
            self.checks.iter().map(|(n, _)| n.clone()).collect();
        let mut fresh = |prefix: &str| {
            let mut name = prefix.to_owned();
            let mut n = 0_usize;
            while taken.contains(&name) {
                n += 1;
                name = format!("{prefix}_{n}");
            }
            taken.insert(name.clone());
            name
        };
        let mut out: Vec<Constraint> = self
            .checks
            .iter()
            .map(|(name, expression)| Constraint {
                name: name.clone(),
                expression: expression.clone(),
                label: format!("the CHECK `{name}` of {}", self.table),
            })
            .collect();
        for c in &self.columns {
            if let Some(Value::Generated(e)) = &c.value {
                out.push(Constraint {
                    name: fresh("pbps_generated"),
                    expression: format!("({e}\n)::{} IS NOT NULL", c.ty),
                    label: format!("the generated expression of {}.{}", self.table, c.shown),
                });
            }
        }
        if let Some((name, index)) = &self.index {
            for (i, column) in index.columns.iter().enumerate() {
                if let pbps_model::IndexKey::Expression(text) = &column.key {
                    out.push(Constraint {
                        name: fresh("pbps_key"),
                        expression: format!("({text}\n) IS NOT NULL"),
                        label: format!("key {} of index `{name}` on {}", i + 1, self.table),
                    });
                }
            }
            if let Some(filter) = &index.filter {
                out.push(Constraint {
                    name: fresh("pbps_filter"),
                    expression: filter.clone(),
                    label: format!("the filter of index `{name}` on {}", self.table),
                });
            }
        }
        out
    }
}

/// The name of the table `change` is about: what its binding table is called.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "the changes that carry no table expression are every other one"
)]
fn bound_table(change: &Change) -> Option<&str> {
    match change {
        Change::CreateTable { name, .. } => Some(&name.name),
        Change::AddColumn { table, .. }
        | Change::AddCheck { table, .. }
        | Change::AddIndex { table, .. }
        | Change::CreatePolicy { table, .. } => Some(&table.name),
        Change::AlterColumnDefault { column, .. }
        | Change::AlterColumnExpression { column, .. } => Some(&column.table.name),
        _ => None,
    }
}

/// Whether `change` introduces an expression this module judges.
#[must_use]
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "the changes that carry no expression this module judges are every other one"
)]
pub fn carries(change: &Change) -> bool {
    if domain_code(change).is_some() {
        return true;
    }
    match change {
        Change::CreateTable { table, .. } => {
            table.partition_of.is_none()
                && (!table.checks.is_empty()
                    || table.columns.values().any(|c| has_value(&c.value_source)))
        }
        Change::AddColumn { column, .. } => has_value(&column.value_source),
        Change::AlterColumnDefault { to, .. } => to.is_some(),
        Change::AlterColumnExpression { .. } | Change::AddCheck { .. } => true,
        // A key that is only a column, and no filter, calls nothing; such an
        // index is also the one built concurrently, outside any transaction.
        Change::AddIndex { index, .. } => index.holds_expression(),
        Change::CreatePolicy { policy, .. } => {
            policy.using.is_some() || policy.with_check.is_some()
        }
        _ => false,
    }
}

/// Whether a pre-flight probe can evaluate the expressions `change` carries over
/// stored rows, so that leaving them unjudged must leave the probe unrun. The
/// only probe that evaluates an index is the duplicate count of a unique index
/// under a filter; no probe evaluates a policy or a domain.
#[must_use]
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "every change this module judges, but these, can be probed"
)]
pub fn probed(change: &Change) -> bool {
    match change {
        Change::AddIndex { index, .. } => index.unique && index.filter.is_some(),
        Change::CreatePolicy { .. } => false,
        _ => domain_code(change).is_none(),
    }
}

/// The code a domain gets from `change`, as a domain that holds only it, and
/// the domain's name: its CHECKs and DEFAULT when it is created or rebuilt, and
/// whichever half of an in-place alteration adds them. `None` when there is
/// none (NOT NULL runs no code).
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "the changes that give a domain no code are every other one"
)]
fn domain_code(change: &Change) -> Option<(TypeRef, ManagedType)> {
    let (name, mut domain) = match change {
        Change::CreateType {
            name, definition, ..
        } => (name, (**definition).clone()),
        Change::EnumRebuild {
            name,
            step:
                pbps_model::EnumRebuildStep::Create {
                    definition, over, ..
                },
            ..
        } => (
            name,
            crate::emit::over_the_replacement(definition, over.as_ref()),
        ),
        Change::AlterDomain {
            name,
            from,
            to,
            half,
            ..
        } => {
            let delta = pbps_model::DomainDelta::of(from, to)?;
            let mut domain = (**to).clone();
            if let ManagedType::Domain {
                default, checks, ..
            } = &mut domain
            {
                match half {
                    pbps_model::DomainHalf::Loosen => {
                        *default = delta.default.flatten().map(str::to_owned);
                        checks.clear();
                    }
                    pbps_model::DomainHalf::Tighten => {
                        *default = None;
                        *checks = delta
                            .add_checks
                            .iter()
                            .map(|(n, t)| ((*n).clone(), (*t).to_owned()))
                            .collect();
                    }
                }
            }
            (name, domain)
        }
        _ => return None,
    };
    let ManagedType::Domain {
        not_null,
        default,
        checks,
        ..
    } = &mut domain
    else {
        return None;
    };
    *not_null = false;
    (default.is_some() || !checks.is_empty()).then(|| (name.clone(), domain))
}

fn has_value(source: &ValueSource) -> bool {
    matches!(source, ValueSource::Default(_) | ValueSource::Generated(_))
}

/// What judging a change's expressions came to. Two cases that must not be
/// read alike: only `Admitted` means the routines were judged and admitted, so
/// nothing may evaluate the expressions on any other reading (a pre-flight
/// probe runs them over stored rows).
#[derive(Debug, PartialEq, Eq)]
#[must_use]
pub enum Vetted {
    /// Every expression was bound and judged, or the change carries none.
    Admitted,
    /// Some could not be bound yet (before the plan creates what they use, or
    /// where the role cannot build the binding table); the notes say which.
    /// They are judged at the statement that introduces them.
    Unjudged(Vec<String>),
}

impl Vetted {
    /// For the moment that cannot leave anything unjudged: the statement
    /// itself, where an expression that does not bind is already a refusal.
    ///
    /// # Errors
    /// Anything but `Admitted`.
    pub fn admitted(self) -> Result<(), String> {
        match self {
            Self::Admitted => Ok(()),
            Self::Unjudged(notes) => Err(notes.join("; ")),
        }
    }
}

/// Judges the expressions `change` introduces. `Err` is a refusal, with the
/// fix; `Unjudged` is returned only before the statement.
pub async fn vet_change(
    conn: &mut Conn,
    admission: &Admission,
    within: Within,
    moment: Moment,
    change: &Change,
) -> Result<Vetted, String> {
    if !carries(change) {
        return Ok(Vetted::Admitted);
    }
    if within == Within::CallersTransaction {
        return vet_in_callers_transaction(conn, admission, moment, change).await;
    }
    let framing = crate::input_path::framing(within, TEMP);
    conn.begin(&framing).await.map_err(|_| {
        "the expressions of a table could not be vetted: no transaction to bind them in".to_owned()
    })?;
    let result = vet_bound(conn, admission, moment, change).await;
    let closed = conn.rollback(&framing).await;
    let notes = result?;
    closed.map_err(|_| {
        "the expressions of a table could not be vetted: its transaction did not close".to_owned()
    })?;
    Ok(notes)
}

/// Binds inside the apply's own transaction, with no savepoint: measured on
/// PostgreSQL 18, a savepoint taken after an `ALTER DOMAIN` of that
/// transaction makes a later `CREATE TABLE` fail with "cannot start a
/// subtransaction when there are unprocessed inval messages" (XX000). A bind
/// that fails here is a refusal, which ends the apply and rolls the whole
/// transaction back, so only a bind that succeeds has to clean up after
/// itself: the temporary table, and the `search_path` it set.
async fn vet_in_callers_transaction(
    conn: &mut Conn,
    admission: &Admission,
    moment: Moment,
    change: &Change,
) -> Result<Vetted, String> {
    let unclean = || {
        "the expressions of a table could not be vetted: its binding table could not be removed"
            .to_owned()
    };
    let rows = conn
        .query("SELECT pg_catalog.current_setting('search_path') AS path")
        .await
        .map_err(|_| unclean())?;
    let path = rows
        .first()
        .and_then(|r| r.try_get::<&str>("path").ok().flatten())
        .map(str::to_owned)
        .ok_or_else(unclean)?;
    let admitted = vet_bound(conn, admission, moment, change).await?;
    // Only a binding that stood is removed: an unjudged one never built its
    // table, and a table of that name in the session is not ours to drop.
    if admitted != Vetted::Admitted {
        return Ok(admitted);
    }
    let drop = match (domain_code(change), bound_table(change)) {
        (Some(_), _) => format!("DROP DOMAIN IF EXISTS pg_temp.{TEMP_DOMAIN}"),
        (None, Some(table)) => format!(
            "DROP TABLE IF EXISTS pg_temp.{}",
            crate::quote(table).map_err(|_| unclean())?
        ),
        (None, None) => return Ok(admitted),
    };
    conn.execute(&drop).await.map_err(|_| unclean())?;
    conn.execute_with(
        "SELECT pg_catalog.set_config('search_path', $1::pg_catalog.text, true)",
        &[pbps_db::Param::Str(&path)],
    )
    .await
    .map_err(|_| unclean())?;
    Ok(admitted)
}

async fn vet_bound(
    conn: &mut Conn,
    admission: &Admission,
    moment: Moment,
    change: &Change,
) -> Result<Vetted, String> {
    let zone = conn
        .session_zone()
        .cloned()
        .unwrap_or_else(pbps_model::SessionZone::utc);
    conn.execute(&crate::session_pins(&zone)).await.map_err(|_| {
        "the expressions of a table could not be vetted: its session settings could not be pinned"
            .to_owned()
    })?;
    let unbound = |what: &str, why: &str| unbound(moment, what, why);
    if let Some((name, domain)) = domain_code(change) {
        return vet_domain(conn, admission, moment, &describe(change), &name, &domain).await;
    }
    let mut spec = match spec(conn, change).await {
        Ok(Some(spec)) => spec,
        Ok(None) => return Ok(Vetted::Admitted),
        Err(why) => return unbound(&describe(change), &why),
    };
    if let Some(table) = spec.like.clone() {
        match column_names(conn, &table).await {
            Ok(taken) => {
                // A column the table, or this change, already calls by the
                // table's name is what that name means on the real table too.
                let free = !taken.contains(&spec.bound)
                    && !spec.columns.iter().any(|c| c.bound == spec.bound);
                name_bound_columns(&mut spec, taken);
                if free {
                    match takes_row_type(conn, &table).await {
                        Ok(true) => spec.whole_row = Some(table),
                        Ok(false) => {}
                        Err(why) => return unbound(&spec.what, &why),
                    }
                }
            }
            Err(why) => return unbound(&spec.what, &why),
        }
    }
    bind_path(conn, &spec.schema, &spec.what).await?;
    // The extended protocol refuses more than one command, so an expression that
    // closes its clause and adds a statement is refused as one that does not
    // bind, never run before it is judged (as for a requirement, DEC-1635.4).
    if let Err(e) = conn.execute_with(&create_statement(&spec)?, &[]).await {
        let code = e
            .server_error_code()
            .unwrap_or_else(|| "unknown".to_owned());
        return unbound(&spec.what, &code);
    }
    let unreadable = || {
        format!(
            "{} could not be vetted: the catalog could not be read",
            spec.what
        )
    };
    let like_columns = match &spec.like {
        Some(table) => crate::input_path::column_count(conn, table)
            .await
            .map_err(|_| unreadable())?,
        None => 0,
    };
    let constraints = spec.constraints();
    let regclass = spec.regclass()?;
    let expected = [
        like_columns + count(spec.columns.len()) + count(usize::from(spec.whole_row.is_some())),
        count(constraints.len()),
        count(
            spec.columns
                .iter()
                .filter(|c| matches!(c.value, Some(Value::Default(_))))
                .count(),
        ),
    ];
    let shape = conn
        .query(&format!(
            "SELECT
               (SELECT count(*) FROM pg_catalog.pg_attribute a
                 WHERE a.attrelid OPERATOR(pg_catalog.=) r.oid
                   AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped)::pg_catalog.int4 AS columns,
               (SELECT count(*) FROM pg_catalog.pg_constraint c
                 WHERE c.conrelid OPERATOR(pg_catalog.=) r.oid
                   AND c.contype OPERATOR(pg_catalog.<>) 'n')::pg_catalog.int4 AS constraints,
               (SELECT count(*) FROM pg_catalog.pg_attrdef d
                 WHERE d.adrelid OPERATOR(pg_catalog.=) r.oid)::pg_catalog.int4 AS defaults
               FROM pg_catalog.pg_class r
              WHERE r.oid OPERATOR(pg_catalog.=) {regclass}"
        ))
        .await
        .map_err(|_| unreadable())?;
    let counts: Option<Vec<i32>> = shape.first().and_then(|row| {
        ["columns", "constraints", "defaults"]
            .iter()
            .map(|name| row.try_get::<i32>(name).ok().flatten())
            .collect()
    });
    if counts.as_deref() != Some(&expected) {
        return Err(format!(
            "{} are more than the expressions declared: one closes its clause and adds to the \
             statement, and each may be the one expression and nothing else; nothing has been changed",
            spec.what
        ));
    }
    let rows = conn
        .query(&format!(
            "SELECT 'check' AS kind, c.conname::pg_catalog.text AS name, c.conbin::pg_catalog.text AS tree
               FROM pg_catalog.pg_constraint c
              WHERE c.conrelid OPERATOR(pg_catalog.=) {regclass}
                AND c.contype OPERATOR(pg_catalog.=) 'c'
             UNION ALL
             SELECT 'default', a.attname::pg_catalog.text, d.adbin::pg_catalog.text
               FROM pg_catalog.pg_attrdef d
               JOIN pg_catalog.pg_attribute a
                 ON a.attrelid OPERATOR(pg_catalog.=) d.adrelid AND a.attnum OPERATOR(pg_catalog.=) d.adnum
              WHERE d.adrelid OPERATOR(pg_catalog.=) {regclass}"
        ))
        .await
        .map_err(|_| unreadable())?;
    let mut trees = Vec::new();
    for row in &rows {
        let (Some(kind), Some(name), Some(tree)) = (
            row.try_get::<&str>("kind").ok().flatten(),
            row.try_get::<&str>("name").ok().flatten(),
            row.try_get::<&str>("tree").ok().flatten(),
        ) else {
            return Err(unreadable());
        };
        let label = if kind == "check" {
            let Some(found) = constraints.iter().find(|c| c.name == name) else {
                return Err(too_many(&spec.what));
            };
            found.label.clone()
        } else {
            format!(
                "the DEFAULT of {}.{}",
                spec.table,
                shown_column(&spec, name)
            )
        };
        trees.push((
            Use {
                ty: label,
                part: Part::TableExpression,
            },
            tree.to_owned(),
        ));
    }
    if let Some((name, policy)) = &spec.policy {
        // The roles it names may be ones the plan creates: they run no code.
        let mut bound = policy.clone();
        bound.to = pbps_model::PolicyRoles::Public;
        let table = TableName::new("pg_temp", &spec.bound);
        let sql = crate::emit::create_policy_sql(&table, TEMP_POLICY, &bound)
            .map_err(|e| format!("{} could not be bound: {e}", spec.what))?;
        if let Err(e) = conn.execute_with(&sql, &[]).await {
            return unbound(&spec.what, &server_code(&e));
        }
        trees.extend(policy_trees(conn, &spec, name, policy).await?);
    }
    judge(conn, admission, trees, &spec.what).await
}

async fn judge(
    conn: &mut Conn,
    admission: &Admission,
    trees: Vec<(Use, String)>,
    what: &str,
) -> Result<Vetted, String> {
    judge_trees(conn, admission, trees, false, &|why: &str| {
        format!("{what} {why}")
    })
    .await?;
    Ok(Vetted::Admitted)
}

/// The code of a created, rebuilt or altered domain, bound as a domain of the
/// session's own `pg_temp` and read from `pg_constraint.conbin` and
/// `pg_type.typdefaultbin` (DEC-2563.5).
async fn vet_domain(
    conn: &mut Conn,
    admission: &Admission,
    moment: Moment,
    what: &str,
    name: &TypeRef,
    domain: &ManagedType,
) -> Result<Vetted, String> {
    let ManagedType::Domain {
        default, checks, ..
    } = domain
    else {
        return Ok(Vetted::Admitted);
    };
    let temp = TableName::new("pg_temp", TEMP_DOMAIN);
    let sql = crate::emit::domain_sql(&temp, domain).map_err(|e| e.to_string())?;
    // The statement that creates the domain runs on its schema's path.
    bind_path(conn, name.schema(), what).await?;
    if let Err(e) = conn.execute_with(&sql, &[]).await {
        return unbound(moment, what, &server_code(&e));
    }
    let unreadable = || format!("{what} could not be vetted: the catalog could not be read");
    let rows = conn
        .query(&format!(
            "SELECT 'default' AS kind, ''::pg_catalog.text AS name, t.typdefaultbin::pg_catalog.text AS tree
               FROM pg_catalog.pg_type t
              WHERE t.oid OPERATOR(pg_catalog.=) 'pg_temp.{TEMP_DOMAIN}'::pg_catalog.regtype
                AND t.typdefaultbin IS NOT NULL
             UNION ALL
             SELECT 'check', c.conname::pg_catalog.text, c.conbin::pg_catalog.text
               FROM pg_catalog.pg_constraint c
              WHERE c.contypid OPERATOR(pg_catalog.=) 'pg_temp.{TEMP_DOMAIN}'::pg_catalog.regtype
                AND c.contype OPERATOR(pg_catalog.=) 'c'"
        ))
        .await
        .map_err(|_| unreadable())?;
    let mut trees = Vec::new();
    let (mut defaults, mut checked) = (0, 0);
    for row in &rows {
        let (Some(kind), Some(label), Some(tree)) = (
            row.try_get::<&str>("kind").ok().flatten(),
            row.try_get::<&str>("name").ok().flatten(),
            row.try_get::<&str>("tree").ok().flatten(),
        ) else {
            return Err(unreadable());
        };
        let shown = name.shown();
        let label = if kind == "default" {
            defaults += 1;
            format!("the DEFAULT of domain {shown}")
        } else {
            checked += 1;
            format!("the CHECK `{label}` of domain {shown}")
        };
        trees.push((
            Use {
                ty: label,
                part: Part::TableExpression,
            },
            tree.to_owned(),
        ));
    }
    // A domain over a domain takes the base's default when it declares none
    // (measured: DEC-2537), so an undeclared default may be there once; a declared
    // one replaces it and is the only one.
    if defaults > 1 || (default.is_some() && defaults != 1) || checked != checks.len() {
        return Err(too_many(what));
    }
    judge(conn, admission, trees, what).await
}

fn too_many(what: &str) -> String {
    format!(
        "{what} are more than the expressions declared: one closes its clause and adds to the \
         statement, and each may be the one expression and nothing else; nothing has been changed"
    )
}

/// The trees the policy bound on the binding table stores. Held to the
/// one policy declared: a text that closes its clause and adds a check changes
/// which of USING and WITH CHECK are there, and is refused.
async fn policy_trees(
    conn: &mut Conn,
    spec: &Spec,
    name: &str,
    policy: &pbps_model::RowPolicy,
) -> Result<Vec<(Use, String)>, String> {
    let unreadable = || {
        format!(
            "{} could not be vetted: the catalog could not be read",
            spec.what
        )
    };
    let regclass = spec.regclass()?;
    let rows = conn
        .query(&format!(
            "SELECT p.polqual::pg_catalog.text AS using_tree, p.polwithcheck::pg_catalog.text AS check_tree
               FROM pg_catalog.pg_policy p
              WHERE p.polrelid OPERATOR(pg_catalog.=) {regclass}"
        ))
        .await
        .map_err(|_| unreadable())?;
    let [row] = rows.as_slice() else {
        return Err(too_many(&spec.what));
    };
    let text = |column: &str| {
        row.try_get::<&str>(column)
            .map(|v| v.map(str::to_owned))
            .map_err(|_| unreadable())
    };
    let (using, check) = (text("using_tree")?, text("check_tree")?);
    if using.is_some() != policy.using.is_some() || check.is_some() != policy.with_check.is_some() {
        return Err(too_many(&spec.what));
    }
    let on = &spec.table;
    let mut trees = Vec::new();
    for (tree, part) in [(using, "USING"), (check, "WITH CHECK")] {
        if let Some(tree) = tree {
            trees.push((
                Use {
                    ty: format!("the {part} of policy `{name}` on {on}"),
                    part: Part::TableExpression,
                },
                tree,
            ));
        }
    }
    Ok(trees)
}

/// A failure to bind is a note before the statement and a refusal at it.
fn unbound(moment: Moment, what: &str, why: &str) -> Result<Vetted, String> {
    match moment {
        Moment::Early => Ok(Vetted::Unjudged(vec![note(what, why)])),
        Moment::Statement => Err(format!(
            "{what} could not be bound just before they run (SQLSTATE {why}), so they cannot be \
             judged; nothing has been changed"
        )),
    }
}

/// Bound on the path the real statement runs on, or an unqualified name would
/// resolve differently here than where it is created (`SET LOCAL` ends with the
/// transaction or savepoint that is rolled back).
async fn bind_path(conn: &mut Conn, schema: &str, what: &str) -> Result<(), String> {
    let path =
        crate::emit::write_path(&crate::Postgres::new(), schema).map_err(|e| e.to_string())?;
    conn.execute(&format!("SET LOCAL search_path = {path}"))
        .await
        .map_err(|_| format!("{what} could not be vetted: its search_path could not be set"))?;
    Ok(())
}

fn server_code(e: &pbps_db::DbError) -> String {
    e.server_error_code()
        .unwrap_or_else(|| "unknown".to_owned())
}

fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn shown_column<'a>(spec: &'a Spec, bound: &'a str) -> &'a str {
    spec.columns
        .iter()
        .find(|c| c.bound == bound)
        .map_or(bound, |c| c.shown.as_str())
}

fn note(what: &str, why: &str) -> String {
    format!(
        "{what} cannot be bound at plan time (SQLSTATE {why}), so they are judged at apply only: \
         they use a type, routine, table or column this plan creates"
    )
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "the changes that carry no expression this module judges are every other one"
)]
fn describe(change: &Change) -> String {
    if let Some((name, _)) = domain_code(change) {
        return format!("the expressions of domain {}", name.shown());
    }
    match change {
        Change::CreateTable { name, .. } => {
            format!("the expressions of the new table {}", name.shown())
        }
        Change::AddColumn { table, name, .. } => {
            format!("the expression of {}.{}", table.shown(), name.shown())
        }
        Change::AlterColumnDefault { column, .. }
        | Change::AlterColumnExpression { column, .. } => {
            format!(
                "the expression of {}.{}",
                column.table.shown(),
                column.name.shown()
            )
        }
        Change::AddCheck { table, name, .. } => {
            format!("the CHECK `{}` of {}", name.shown(), table.shown())
        }
        Change::AddIndex { table, name, .. } => {
            format!(
                "the expressions of index `{}` on {}",
                name.shown(),
                table.shown()
            )
        }
        Change::CreatePolicy { table, name, .. } => {
            format!(
                "the expressions of row policy `{name}` on {}",
                table.shown()
            )
        }
        _ => "the expressions of a table".to_owned(),
    }
}

/// The statement that builds the binding table. Nothing in it is evaluated: the
/// table has no row, and a default and a check run on one; a generated
/// expression and an index are bound as checks so that the engine does not fold
/// them either ([`Spec::constraints`]).
fn create_statement(spec: &Spec) -> Result<String, String> {
    let quoted = |name: &str| crate::quote(name).map_err(|e| e.to_string());
    // Each expression closes on its own line: one ending in a `--` comment
    // would otherwise swallow the parenthesis that ends it.
    let mut parts = Vec::new();
    if let Some(table) = &spec.like {
        parts.push(format!(
            "LIKE {} EXCLUDING ALL",
            crate::emit::qualified(table).map_err(|e| e.to_string())?
        ));
    }
    if let Some(table) = &spec.whole_row {
        parts.push(format!(
            "{} {}",
            quoted(&spec.bound)?,
            crate::emit::qualified(table).map_err(|e| e.to_string())?
        ));
    }
    for c in &spec.columns {
        let value = match &c.value {
            Some(Value::Default(e)) => format!(" DEFAULT ({e}\n)"),
            Some(Value::Generated(_)) | None => String::new(),
        };
        parts.push(format!("{} {}{value}", quoted(&c.bound)?, c.ty));
    }
    for c in spec.constraints() {
        parts.push(format!(
            "CONSTRAINT {} CHECK ({}\n)",
            quoted(&c.name)?,
            c.expression
        ));
    }
    Ok(format!(
        "CREATE TEMPORARY TABLE {} ({})",
        spec.relation()?,
        parts.join(", ")
    ))
}

/// The names of the columns `table` has today, dropped ones excluded.
async fn column_names(
    conn: &mut Conn,
    table: &TableName,
) -> Result<std::collections::BTreeSet<String>, String> {
    let qualified = crate::emit::qualified(table).map_err(|e| e.to_string())?;
    let rows = conn
        .query_with(
            "SELECT a.attname::pg_catalog.text AS name
               FROM pg_catalog.pg_attribute a
              WHERE a.attrelid OPERATOR(pg_catalog.=) $1::pg_catalog.text::pg_catalog.regclass
                AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped",
            &[Param::Str(&qualified)],
        )
        .await
        .map_err(|e| {
            e.server_error_code()
                .unwrap_or_else(|| "unknown".to_owned())
        })?;
    Ok(rows
        .iter()
        .filter_map(|r| r.try_get::<&str>("name").ok().flatten().map(str::to_owned))
        .collect())
}

/// Whether any function or operator takes the row type of `table`, which is
/// the only way a bare `t` in an expression can need that type. Asked so that a
/// binding that no such routine can reach is built exactly as before: the extra
/// column asks for USAGE on the type, which a table owner may have revoked.
async fn takes_row_type(conn: &mut Conn, table: &TableName) -> Result<bool, String> {
    let qualified = crate::emit::qualified(table).map_err(|e| e.to_string())?;
    let rows = conn
        .query_with(
            "SELECT (EXISTS (SELECT 1 FROM pg_catalog.pg_proc p
                              WHERE r.reltype OPERATOR(pg_catalog.=) ANY (p.proargtypes::pg_catalog.oid[]))
                  OR EXISTS (SELECT 1 FROM pg_catalog.pg_operator o
                              WHERE r.reltype OPERATOR(pg_catalog.=) o.oprleft
                                 OR r.reltype OPERATOR(pg_catalog.=) o.oprright)) AS taken
               FROM pg_catalog.pg_class r
              WHERE r.oid OPERATOR(pg_catalog.=) $1::pg_catalog.text::pg_catalog.regclass",
            &[Param::Str(&qualified)],
        )
        .await
        .map_err(|e| {
            e.server_error_code()
                .unwrap_or_else(|| "unknown".to_owned())
        })?;
    rows.first()
        .and_then(|r| r.try_get::<bool>("taken").ok().flatten())
        .ok_or_else(|| "42P01".to_owned())
}

/// Gives every bound column a name the copied columns do not have. The copy
/// (`LIKE`) brings the table's own columns, so a fixed helper name, or the
/// name of a column the plan drops and adds back, would make the temporary
/// table fail to build and refuse a valid plan. The name is derived from the
/// column set itself (`pbps_bound`, then `pbps_bound_1`, ...), so it cannot
/// collide: no declared column can have a name this search skipped. The other
/// helpers cannot collide by construction: the temporary table lives in the
/// session's own `pg_temp`, `LIKE ... EXCLUDING ALL` copies no constraint or
/// default, and no alias is used.
fn name_bound_columns(spec: &mut Spec, mut taken: std::collections::BTreeSet<String>) {
    for column in &mut spec.columns {
        if column.bound == BIND || taken.contains(&column.bound) {
            let mut n = 0_usize;
            let mut name = BIND.to_owned();
            while taken.contains(&name) {
                n += 1;
                name = format!("{BIND}_{n}");
            }
            column.bound = name;
        }
        taken.insert(column.bound.clone());
    }
}

/// The type the table has for `column` today, for an expression that changes
/// an existing column's default or generation.
async fn column_type(conn: &mut Conn, column: &pbps_model::ColumnRef) -> Result<String, String> {
    let qualified = crate::emit::qualified(&column.table).map_err(|e| e.to_string())?;
    let rows = conn
        .query_with(
            "SELECT pg_catalog.format_type(a.atttypid, a.atttypmod) AS ty
               FROM pg_catalog.pg_attribute a
              WHERE a.attrelid OPERATOR(pg_catalog.=) $1::pg_catalog.text::pg_catalog.regclass
                AND a.attname OPERATOR(pg_catalog.=) $2::pg_catalog.text::pg_catalog.name
                AND NOT a.attisdropped",
            &[Param::Str(&qualified), Param::Str(column.name.raw())],
        )
        .await
        .map_err(|e| {
            e.server_error_code()
                .unwrap_or_else(|| "unknown".to_owned())
        })?;
    rows.first()
        .and_then(|r| r.try_get::<&str>("ty").ok().flatten())
        .map(str::to_owned)
        .ok_or_else(|| "42703".to_owned())
}

/// What `change` binds, or why it cannot be built here (a SQLSTATE).
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "the changes that carry no expression this module judges are every other one"
)]
async fn spec(conn: &mut Conn, change: &Change) -> Result<Option<Spec>, String> {
    let value = |source: &ValueSource| match source {
        ValueSource::Default(e) => Some(Value::Default(e.clone())),
        ValueSource::Generated(g) => Some(Value::Generated(g.expression.clone())),
        ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => None,
    };
    let sql_type =
        |ty: &pbps_model::ColumnType| crate::emit::sql_type(ty).map_err(|_| "0A000".to_owned());
    Ok(Some(match change {
        Change::CreateTable { name, table, .. } => Spec {
            what: describe(change),
            bound: bound_table(change).unwrap_or_default().to_owned(),
            whole_row: None,
            table: name.shown().to_string(),
            schema: name.schema.clone(),
            like: None,
            columns: table
                .columns
                .iter()
                .map(|(n, c)| {
                    Ok(Column {
                        bound: n.raw().to_owned(),
                        shown: n.raw().to_owned(),
                        ty: sql_type(&c.ty)?,
                        value: value(&c.value_source),
                    })
                })
                .collect::<Result<_, String>>()?,
            checks: table
                .checks
                .iter()
                .map(|(n, c)| (n.raw().to_owned(), c.expression.clone()))
                .collect(),
            index: None,
            policy: None,
        },
        Change::AddColumn {
            table,
            name,
            column,
            ..
        } => Spec {
            what: describe(change),
            bound: bound_table(change).unwrap_or_default().to_owned(),
            whole_row: None,
            table: table.shown().to_string(),
            schema: table.schema.clone(),
            like: Some(table.clone()),
            columns: vec![Column {
                bound: name.raw().to_owned(),
                shown: name.raw().to_owned(),
                ty: sql_type(&column.ty)?,
                value: value(&column.value_source),
            }],
            checks: Vec::new(),
            index: None,
            policy: None,
        },
        Change::AddCheck {
            table,
            name,
            constraint,
        } => Spec {
            what: describe(change),
            bound: bound_table(change).unwrap_or_default().to_owned(),
            whole_row: None,
            table: table.shown().to_string(),
            schema: table.schema.clone(),
            like: Some(table.clone()),
            columns: Vec::new(),
            checks: vec![(name.raw().to_owned(), constraint.expression.clone())],
            index: None,
            policy: None,
        },
        Change::AddIndex {
            table, name, index, ..
        } => on_table(
            change,
            table,
            Some((name.shown().to_string(), index.clone())),
            None,
        ),
        Change::CreatePolicy {
            table,
            name,
            policy,
            ..
        } => on_table(
            change,
            table,
            None,
            Some((name.clone(), Box::new(policy.clone()))),
        ),
        Change::AlterColumnDefault { column, to, .. } => {
            let Some(expression) = to else {
                return Ok(None);
            };
            existing(conn, change, column, Value::Default(expression.clone())).await?
        }
        Change::AlterColumnExpression { column, to, .. } => {
            existing(conn, change, column, Value::Generated(to.clone())).await?
        }
        _ => return Ok(None),
    }))
}

/// An existing column given a new expression, bound as an extra column of the
/// column's type so that the copy of the table keeps the one it replaces.
async fn existing(
    conn: &mut Conn,
    change: &Change,
    column: &pbps_model::ColumnRef,
    value: Value,
) -> Result<Spec, String> {
    Ok(Spec {
        what: describe(change),
        bound: bound_table(change).unwrap_or_default().to_owned(),
        whole_row: None,
        table: column.table.shown().to_string(),
        schema: column.table.schema.clone(),
        like: Some(column.table.clone()),
        columns: vec![Column {
            bound: BIND.to_owned(),
            shown: column.name.raw().to_owned(),
            ty: column_type(conn, column).await?,
            value: Some(value),
        }],
        checks: Vec::new(),
        index: None,
        policy: None,
    })
}

/// An index or a policy of a table that stands: bound on a copy of its columns.
fn on_table(
    change: &Change,
    table: &TableName,
    index: Option<(String, Box<pbps_model::Index>)>,
    policy: Option<(String, Box<pbps_model::RowPolicy>)>,
) -> Spec {
    Spec {
        what: describe(change),
        bound: bound_table(change).unwrap_or_default().to_owned(),
        whole_row: None,
        table: table.shown().to_string(),
        schema: table.schema.clone(),
        like: Some(table.clone()),
        columns: Vec::new(),
        checks: Vec::new(),
        index,
        policy,
    }
}
