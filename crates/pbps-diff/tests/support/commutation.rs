//! The commutation suite's engine-independent half (#1965, slice S1b-iii of
//! #1776), shared by `pbps-pg/tests/live.rs` and `pbps-mssql/tests/live.rs`.
//!
//! The verifier holds an order to the declared effects, so an effect nobody
//! declared is invisible to it: the scheduler and the verifier agree on the
//! same wrong plan. Each scenario names a pair of steps the effects call
//! independent, runs the plan in its own order and in one that swaps the
//! pair (`pbps_diff::order::commute::swapped`), each on its own database, and
//! holds the two to three checks:
//!
//! 1. the same final catalog, read as a pull reads it (columns in the
//!    engine's order too), and an empty next plan;
//! 2. the same grants and ownership on every object in the schema;
//! 3. the same rows in every table the base seeded.
//!
//! A difference means the pair is not independent and the effects are wrong.
//! A third database runs the plan made with every uid minted again, which
//! needs to pass the same checks without being byte-identical; the plan made
//! twice from the same uids needs to be byte-identical.
//!
//! Each engine lists its own sample, at most [`CAP`] pairs, chosen so that
//! each kind of change it plans is in at least one. A new pair replaces one
//! or raises the cap in a recorded change, never silently. A pair the engine
//! shows the effects wrong about is listed with the issue that tracks it,
//! and the suite holds it to still showing, so the day the effects order it
//! the list has to change.

use std::path::PathBuf;

use pbps_dialect::Dialect;
use pbps_diff::order::commute;
use pbps_model::{ChangeSet, IdsFile, Schema, TableName};

/// At most this many pairs per engine: the suite's cost is fixed up front
/// (#1965).
pub const CAP: usize = 30;

/// The schema every scenario declares into. Each database is its own, so the
/// name is shared.
pub const SCHEMA: &str = "app";

/// A pair, and the declarations around it: `base` is what every database
/// starts from, rows included, and `desired` what the plan reaches. Each is
/// a list of declaration files separated by `---` lines, `{role}` standing
/// for this run's own role.
pub struct Scenario {
    pub name: &'static str,
    /// The kinds of the two steps, as the shadow report spells them: the
    /// first step of each, or the first two of one kind.
    pub pair: [&'static str; 2],
    /// The issue of a known gap: a pair the effects call independent and the
    /// engine does not.
    pub gap: Option<u32>,
    pub base: &'static str,
    pub desired: &'static str,
}

/// What a database holds once a plan ran, as the three checks read it.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub catalog: Schema,
    /// Each table's columns in the engine's order: a pull writes them so,
    /// and `Schema` equality does not see it.
    pub columns: Vec<(String, Vec<String>)>,
    pub access: Vec<String>,
    pub rows: Vec<(String, String)>,
}

/// What a pulled database holds in [`SCHEMA`], and the base a connected plan
/// would be made against.
pub struct Pulled {
    pub catalog: Schema,
    pub base: Schema,
}

/// The engine's half: a database of its own, what it runs, and what it
/// reads back.
pub trait Engine {
    type Db;
    fn name(&self) -> &'static str;
    fn dialect(&self) -> &dyn Dialect;
    /// A new database with [`SCHEMA`] in it and nothing else.
    async fn database(&self, tag: &str) -> Self::Db;
    async fn drop(&self, db: Self::Db);
    async fn execute(&self, db: &mut Self::Db, sql: &str) -> Result<(), String>;
    /// The first column of every row, as text.
    async fn texts(&self, db: &mut Self::Db, sql: &str) -> Result<Vec<String>, String>;
    async fn pull(&self, db: &mut Self::Db, desired: &Schema) -> Result<Pulled, String>;
    /// One row per object in [`SCHEMA`]: its owner and its grants.
    fn access_sql(&self) -> String;
    /// Every row of `table`, one text per row, in a stable order.
    fn rows_sql(&self, table: &TableName) -> String;
}

/// What is wrong with a scenario.
#[derive(Debug)]
pub enum Finding {
    /// The harness or the declarations: the scenario never reached the pair.
    Harness(String),
    /// The effects order the pair, so it is no sample of independence.
    Stale(String),
    /// The two orders are not equivalent: the effects are wrong.
    Differs(String),
    /// The plan made twice from the same uids is not byte-identical. Nothing
    /// excuses it.
    Nondeterministic(String),
}

/// What a scenario's run found: the pair's result, and apart from it whether
/// the plan made with new uids ends as the plan does. Kept apart so that
/// neither can stand in for the other: the swapped run is checked whatever
/// the plan with new uids did.
#[derive(Debug)]
pub struct Checked {
    pub pair: Result<(), Finding>,
    /// How the plan with new uids ended otherwise, if it did. Not the pair's
    /// fault, so a listed `gap` does not excuse it, and nothing else does: a
    /// plan whose result depends on its uids is fixed when found, not listed
    /// (#2104, DEC-2104.1).
    pub reminted: Option<String>,
}

impl From<String> for Finding {
    fn from(text: String) -> Self {
        Finding::Harness(text)
    }
}

/// One side's declarations, loaded the way `plan` loads a schema directory.
pub fn load(engine: &str, scenario: &str, side: &str, text: &str, role: &str) -> pbps_load::Loaded {
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "pbps_commute_{engine}_{}_{scenario}_{side}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory for the declarations");
    for (i, file) in text.replace("{role}", role).split("\n---\n").enumerate() {
        std::fs::write(dir.join(format!("f{i}.yml")), file.trim_start())
            .expect("write a declaration");
    }
    let loaded = pbps_load::load_schema_dir(&dir)
        .unwrap_or_else(|e| panic!("{scenario} {side} does not load: {e:?}"));
    let _ = std::fs::remove_dir_all(&dir);
    loaded
}

fn ctx() -> pbps_diff::Context {
    pbps_diff::Context {
        operator: "commutation".into(),
        today: "2026-10-16".into(),
    }
}

/// The ids `plan` would mint, every drop given the reason it would otherwise
/// be refused for lacking.
// A blocker other than a drop's missing reason is a scenario that cannot
// plan, which the panic names; a new kind of blocker is one more such.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn mint(schema: &Schema, base: &IdsFile, intents: &[pbps_model::Intent]) -> IdsFile {
    use pbps_diff::identity::Blocker;
    use pbps_model::Intent;
    let mut intents = intents.to_vec();
    loop {
        match pbps_diff::resolve(schema, base, &intents, &ctx()) {
            Ok(resolution) => return resolution.ids,
            Err(blockers) => {
                let reason = || "a commutation scenario".parse().expect("a reason");
                let before = intents.len();
                for b in blockers {
                    match b {
                        Blocker::DropColumnNeedsReason { column } => {
                            intents.push(Intent::DropColumn {
                                column,
                                reason: reason(),
                            });
                        }
                        Blocker::DropTableNeedsReason { table } => {
                            intents.push(Intent::DropTable {
                                table,
                                reason: reason(),
                            });
                        }
                        Blocker::DropRoleNeedsReason { role } => {
                            intents.push(Intent::DropRole {
                                role,
                                reason: reason(),
                            });
                        }
                        other => panic!("resolve: {other:?}"),
                    }
                }
                assert!(intents.len() > before, "resolve refused without a blocker");
            }
        }
    }
}

pub fn diff(
    dialect: &dyn Dialect,
    base: (&Schema, &IdsFile),
    desired: (&Schema, &IdsFile),
    hints: &pbps_model::Hints,
) -> ChangeSet {
    pbps_diff::diff(
        pbps_diff::Side {
            schema: base.0,
            ids: base.1,
        },
        pbps_diff::Side {
            schema: desired.0,
            ids: desired.1,
        },
        dialect,
        hints,
    )
    .expect("diff")
}

/// [`diff`], with the zero-statement changes `plan` carries beside it: the
/// keys an `ensure` block manages (DEC-1808.1), which no diff can see.
pub fn planned(
    dialect: &dyn Dialect,
    base: (&Schema, &IdsFile),
    desired: (&Schema, &IdsFile),
    hints: &pbps_model::Hints,
) -> ChangeSet {
    let mut changes = diff(dialect, base, desired, hints);
    for change in pbps_model::data::ensure_key_changes(
        &base.0.data_scopes(),
        &desired.0.data_scopes(),
        base.1,
        desired.1,
    ) {
        changes.changes.push(pbps_model::PlannedChange::new(change));
    }
    changes
}

/// The kind of a change, as the shadow report spells it.
pub fn kind(change: &pbps_model::Change) -> String {
    format!("{change:?}")
        .split([' ', '{', '('])
        .next()
        .unwrap_or("")
        .to_owned()
}

/// Emits and executes `changes` in `order`, one statement at a time as
/// `apply` does. An engine refusal is returned, not panicked on: in the
/// swapped order it is the suite's finding.
pub async fn run<E: Engine>(
    engine: &E,
    db: &mut E::Db,
    changes: &ChangeSet,
    order: &[usize],
) -> Result<(), String> {
    for &i in order {
        let p = &changes.changes[i];
        for stmt in engine
            .dialect()
            .emit(&p.change, p.strategy)
            .map_err(|e| e.to_string())?
        {
            engine
                .execute(db, &stmt.sql)
                .await
                .map_err(|e| format!("{} refused at\n{}\n{e}", kind(&p.change), stmt.sql))?;
        }
    }
    Ok(())
}

/// Reads the three checks' facts from `db`, and holds it to an empty next
/// plan against `desired`. A read that fails is the harness's (`Harness`);
/// a database read whole that has not converged on the declarations is a
/// difference (`Differs`), which [`converged`] says whose it is.
pub async fn outcome<E: Engine>(
    engine: &E,
    db: &mut E::Db,
    desired: &pbps_load::Loaded,
    ids: &IdsFile,
    seeded: &[TableName],
) -> Result<Outcome, Finding> {
    let Pulled { catalog, mut base } = engine.pull(db, &desired.schema).await?;
    let columns = catalog
        .tables
        .iter()
        .map(|(name, t)| {
            (
                name.raw().to_string(),
                t.columns.keys().map(|c| c.raw().to_owned()).collect(),
            )
        })
        .collect();
    // The next plan is about what the declarations converge on. A module's
    // text is the engine's own respelling once read back, which `plan --db`
    // reconciles and this read does not; the databases' modules are compared
    // with each other instead. A deprecation is recorded state, never
    // catalog.
    base.modules = desired.schema.modules.clone();
    for (name, table) in &mut base.tables {
        if let Some(declared) = desired.schema.tables.get(name) {
            for (column, c) in &mut table.columns {
                if let Some(d) = declared.columns.get(column) {
                    c.deprecated = d.deprecated.clone();
                }
            }
        }
    }
    let next = diff(
        engine.dialect(),
        (&base, ids),
        (&desired.schema, ids),
        &desired.hints,
    );
    if !next.changes.is_empty() {
        let changes: Vec<String> = next
            .changes
            .iter()
            .map(|p| format!("{:?}", p.change))
            .collect();
        return Err(Finding::Differs(format!(
            "the next plan is not empty:\n{}",
            changes.join("\n")
        )));
    }
    let access = engine.texts(db, &engine.access_sql()).await?;
    let mut rows = Vec::new();
    for table in seeded {
        let dump = engine.texts(db, &engine.rows_sql(table)).await?;
        rows.push((table.raw().to_string(), dump.join("\n")));
    }
    Ok(Outcome {
        catalog,
        columns,
        access,
        rows,
    })
}

/// A database with the base in it: the schema, its rows, and nothing else.
async fn based<E: Engine>(engine: &E, tag: &str, creation: &ChangeSet) -> E::Db {
    let mut db = engine.database(tag).await;
    let all: Vec<usize> = (0..creation.changes.len()).collect();
    if let Err(e) = run(engine, &mut db, creation, &all).await {
        engine.drop(db).await;
        panic!("the base does not build: {e}");
    }
    db
}

/// The lines of `a`'s rendering that `b`'s lacks, and the other way round:
/// a difference a reader can find, not two renderings to compare by eye.
fn delta(a: &impl std::fmt::Debug, b: &impl std::fmt::Debug) -> String {
    let (a, b) = (format!("{a:#?}"), format!("{b:#?}"));
    let only = |x: &str, y: &str, sign: char| {
        let mut rest: Vec<&str> = y.lines().collect();
        x.lines()
            .filter(|line| match rest.iter().position(|r| r == line) {
                Some(i) => {
                    rest.remove(i);
                    false
                }
                None => true,
            })
            .map(|line| format!("{sign} {}", line.trim()))
            .collect::<Vec<_>>()
    };
    let mut out = only(&a, &b, '-');
    out.extend(only(&b, &a, '+'));
    out.join("\n")
}

/// Holds two outcomes to the three checks.
pub fn same(what: &str, a: &Outcome, b: &Outcome) -> Result<(), Finding> {
    let differs = |name: &str, same: bool| {
        if same {
            Ok(())
        } else {
            Err(Finding::Differs(format!(
                "{what}: the {name} differ\n{}",
                delta(a, b)
            )))
        }
    };
    differs("catalogs", a.catalog == b.catalog)?;
    differs("column orders", a.columns == b.columns)?;
    differs("grants and owners", a.access == b.access)?;
    differs("rows", a.rows == b.rows)
}

/// A scenario's plan, ready to run in any order.
pub struct Prepared {
    pub base: pbps_load::Loaded,
    pub desired: pbps_load::Loaded,
    pub base_ids: IdsFile,
    pub ids: IdsFile,
    pub creation: ChangeSet,
    pub changes: ChangeSet,
    pub kinds: Vec<String>,
    pub seeded: Vec<TableName>,
}

pub fn prepare(engine: &str, dialect: &dyn Dialect, scenario: &Scenario, role: &str) -> Prepared {
    let base = load(engine, scenario.name, "base", scenario.base, role);
    let desired = load(engine, scenario.name, "desired", scenario.desired, role);
    let base_ids = mint(&base.schema, &IdsFile::default(), &base.intents);
    let ids = mint(&desired.schema, &base_ids, &desired.intents);
    let creation = diff(
        dialect,
        (&Schema::default(), &IdsFile::default()),
        (&base.schema, &base_ids),
        &base.hints,
    );
    let changes = planned(
        dialect,
        (&base.schema, &base_ids),
        (&desired.schema, &ids),
        &desired.hints,
    );
    let kinds = changes.changes.iter().map(|p| kind(&p.change)).collect();
    let seeded = base
        .schema
        .tables
        .iter()
        .filter(|(_, t)| t.data.is_some())
        .map(|(name, _)| name.clone())
        .collect();
    Prepared {
        base,
        desired,
        base_ids,
        ids,
        creation,
        changes,
        kinds,
        seeded,
    }
}

/// The new-uid result: `None` when the plan with new uids ended as the
/// plan did, the difference when it ended otherwise (it ran to another
/// state, was refused, or left a next plan), and the error when it could not
/// be read at all.
pub fn reminted_of(
    plan: &Outcome,
    reminted: Result<Outcome, Finding>,
) -> Result<Option<String>, Finding> {
    match reminted.and_then(|c| same("the plan with new uids", plan, &c)) {
        Ok(()) => Ok(None),
        Err(Finding::Differs(e)) => Ok(Some(e)),
        Err(other) => Err(other),
    }
}

/// Whose an outcome's failure is. The plan's own order not converging is a
/// broken scenario; a swapped order that runs and does not converge is the
/// pair not commuting, which a listed gap may hold.
pub fn converged(own: bool, found: Finding) -> Finding {
    match found {
        Finding::Differs(e) if own => Finding::Harness(e),
        kept @ (Finding::Harness(_)
        | Finding::Differs(_)
        | Finding::Stale(_)
        | Finding::Nondeterministic(_)) => kept,
    }
}

/// Runs `changes` in `order` on a database of its own, built from
/// `creation`, and reads its outcome. `own` says whether this is the plan's
/// own order, whose refusal is a broken scenario rather than a finding.
#[allow(clippy::too_many_arguments)]
pub async fn outcome_of<E: Engine>(
    engine: &E,
    tag: &str,
    p: &Prepared,
    creation: &ChangeSet,
    changes: &ChangeSet,
    order: &[usize],
    ids: &IdsFile,
    own: bool,
) -> Result<Outcome, Finding> {
    let mut db = based(engine, tag, creation).await;
    let out = match run(engine, &mut db, changes, order).await {
        Err(e) if own => Err(Finding::Harness(e)),
        Err(e) => Err(Finding::Differs(e)),
        Ok(()) => outcome(engine, &mut db, &p.desired, ids, &p.seeded)
            .await
            .map_err(|f| converged(own, f)),
    };
    engine.drop(db).await;
    out.map_err(|f| match f {
        Finding::Harness(e) => Finding::Harness(format!("{tag}: {e}")),
        Finding::Differs(e) => Finding::Differs(format!("{tag}: {e}")),
        kept @ (Finding::Stale(_) | Finding::Nondeterministic(_)) => kept,
    })
}

/// Runs one scenario and says what, if anything, is wrong with it.
pub async fn check<E: Engine>(engine: &E, scenario: &Scenario, role: &str) -> Checked {
    match checked(engine, scenario, role).await {
        Ok(found) => found,
        // Nothing past the failure ran, so there is no new-uid result.
        Err(f) => Checked {
            pair: Err(f),
            reminted: None,
        },
    }
}

async fn checked<E: Engine>(
    engine: &E,
    scenario: &Scenario,
    role: &str,
) -> Result<Checked, Finding> {
    let dialect = engine.dialect();
    let p = prepare(engine.name(), dialect, scenario, role);
    // Determinism: the same plan from the same uids, byte for byte.
    let again = planned(
        dialect,
        (&p.base.schema, &p.base_ids),
        (&p.desired.schema, &p.ids),
        &p.desired.hints,
    );
    let bytes = |c: &ChangeSet| serde_json::to_vec(c).expect("a plan serializes");
    if bytes(&p.changes) != bytes(&again) {
        return Err(Finding::Nondeterministic(
            "the same plan from the same uids is not byte-identical".into(),
        ));
    }
    let kinds = &p.kinds;
    let nth = |k: &str, n: usize| {
        kinds
            .iter()
            .enumerate()
            .filter(|(_, x)| *x == k)
            .nth(n)
            .map(|(i, _)| i)
            .ok_or_else(|| format!("the plan has no {k} #{n}: {kinds:?}"))
    };
    let (x, y) = (
        nth(scenario.pair[0], 0)?,
        nth(
            scenario.pair[1],
            usize::from(scenario.pair[0] == scenario.pair[1]),
        )?,
    );
    let (earlier, later) = (x.min(y), x.max(y));
    let sides = (
        pbps_diff::Side {
            schema: &p.base.schema,
            ids: &p.base_ids,
        },
        pbps_diff::Side {
            schema: &p.desired.schema,
            ids: &p.ids,
        },
    );
    let declared = &p.desired.hints.module_deps;
    let read = commute::read(sides.0, sides.1, dialect, declared, &p.changes, None)?;
    if !read.report.unowned.is_empty() || !read.report.violations.is_empty() {
        return Err(Finding::Harness(format!(
            "the plan's own order is not clean: {:?} {:?}",
            read.report.unowned, read.report.violations
        )));
    }
    let n = p.changes.changes.len();
    let swapped = commute::swapped(&read.report.graph, n, earlier, later).ok_or_else(|| {
        let edges: Vec<String> = read
            .report
            .graph
            .edges
            .iter()
            .map(|e| format!("{} -> {} {:?}", kinds[e.before], kinds[e.after], e.because))
            .collect();
        Finding::Stale(format!(
            "the effects order {} and {}\n{}",
            kinds[earlier],
            kinds[later],
            edges.join("\n")
        ))
    })?;
    let again = commute::read(
        sides.0,
        sides.1,
        dialect,
        declared,
        &p.changes,
        Some(&swapped),
    )?;
    if !again.report.violations.is_empty() {
        return Err(Finding::Harness(format!(
            "the verifier refuses the swapped order: {:?}",
            again.report.violations
        )));
    }
    // The plan with every uid minted again: its order may differ where uids
    // break ties, and its result may not.
    let base_ids2 = mint(&p.base.schema, &IdsFile::default(), &p.base.intents);
    let ids2 = mint(&p.desired.schema, &base_ids2, &p.desired.intents);
    let creation2 = diff(
        dialect,
        (&Schema::default(), &IdsFile::default()),
        (&p.base.schema, &base_ids2),
        &p.base.hints,
    );
    let changes2 = planned(
        dialect,
        (&p.base.schema, &base_ids2),
        (&p.desired.schema, &ids2),
        &p.desired.hints,
    );
    let tag = |t: &str| {
        format!(
            "cm_{t}_{}",
            scenario.name.chars().take(28).collect::<String>()
        )
    };
    let own: Vec<usize> = (0..n).collect();
    let a = outcome_of(
        engine,
        &tag("a"),
        &p,
        &p.creation,
        &p.changes,
        &own,
        &p.ids,
        true,
    )
    .await?;
    // The plan with new uids first: whatever the pair shows, an unstable
    // plan is a failure of its own. It is read as a swapped order is (`own`
    // false): refused or left short of the declarations, it ended otherwise,
    // which is the new-uid result and not a broken harness.
    let reminted: Vec<usize> = (0..changes2.changes.len()).collect();
    let c = outcome_of(
        engine,
        &tag("c"),
        &p,
        &creation2,
        &changes2,
        &reminted,
        &ids2,
        false,
    )
    .await;
    let reminted = reminted_of(&a, c)?;
    // The swapped run, whatever the plan with new uids did.
    let pair = match outcome_of(
        engine,
        &tag("b"),
        &p,
        &p.creation,
        &p.changes,
        &swapped,
        &p.ids,
        false,
    )
    .await
    {
        Ok(b) => same(
            &format!("{} and {} swapped", kinds[earlier], kinds[later]),
            &a,
            &b,
        ),
        Err(f) => Err(f),
    };
    Ok(Checked { pair, reminted })
}

/// Runs a scenario's pair in both orders whatever the effects say of it, and
/// returns what the three checks found: the negative that shows the checks
/// can tell two orders apart at all. Only SQL Server has one (DEC-1965.1), so
/// PostgreSQL's copy of this core leaves it unused.
#[allow(dead_code)]
pub async fn forced<E: Engine>(engine: &E, scenario: &Scenario, role: &str) -> Result<(), Finding> {
    let p = prepare(engine.name(), engine.dialect(), scenario, role);
    let at = |k: &str| {
        p.kinds
            .iter()
            .position(|x| x == k)
            .unwrap_or_else(|| panic!("the plan has no {k}: {:?}", p.kinds))
    };
    let (x, y) = (at(scenario.pair[0]), at(scenario.pair[1]));
    let own: Vec<usize> = (0..p.changes.changes.len()).collect();
    let mut other = own.clone();
    other.swap(x, y);
    let tag = |t: &str| {
        format!(
            "cm_{t}_{}",
            scenario.name.chars().take(28).collect::<String>()
        )
    };
    let a = outcome_of(
        engine,
        &tag("a"),
        &p,
        &p.creation,
        &p.changes,
        &own,
        &p.ids,
        true,
    )
    .await?;
    let b = outcome_of(
        engine,
        &tag("b"),
        &p,
        &p.creation,
        &p.changes,
        &other,
        &p.ids,
        false,
    )
    .await?;
    same("the forced order", &a, &b)
}

/// What a scenario's run says, `None` when it is as listed. A listed gap
/// excuses only the pair's own difference (`Differs`). The plan with new
/// uids ending otherwise, a plan that is not byte-identical from the same
/// uids, a broken harness or a stale pair fails whatever is listed, each
/// judged whatever the others found.
pub fn verdict(scenario: &Scenario, found: Checked) -> Option<String> {
    let name = scenario.name;
    let mut wrong = Vec::new();
    if let Some(e) = &found.reminted {
        wrong.push(format!(
            "{name}: the plan with new uids ends otherwise: {e}"
        ));
    }
    match (found.pair, scenario.gap) {
        (Ok(()), None) | (Err(Finding::Differs(_)), Some(_)) => {}
        (Ok(()), Some(gap)) => wrong.push(format!(
            "{name}: the gap #{gap} no longer shows; list the pair as an ordinary one"
        )),
        (Err(Finding::Stale(e)), Some(gap)) => wrong.push(format!(
            "{name}: the effects now order the pair of gap #{gap}; replace it\n{e}"
        )),
        (Err(Finding::Nondeterministic(e)), _) => wrong.push(format!(
            "{name}: the same plan from the same uids differs: {e}"
        )),
        (Err(f), _) => wrong.push(format!("{name}: {f:?}")),
    }
    (!wrong.is_empty()).then(|| wrong.join("\n"))
}

/// Whether the suite runs as an audit: every pair of each scenario's plan the
/// effects call independent, not only the listed one (#2091). An effect
/// deleted or never written makes pairs independent that are not, and the
/// listed pair is rarely one of them; the audit swaps them all, on the
/// engine. It costs a database per pair, so it is a manual or nightly run,
/// outside the merge group's cap.
pub fn audits() -> bool {
    std::env::var_os("PBPS_COMMUTE_ALL").is_some()
}

/// Every pair of `scenario`'s plan the effects call independent, swapped and
/// run against the plan's own order: how many ran, and one line for each
/// that ended otherwise. The scenario's listed gap pair is left out, since
/// the sample already holds it to showing.
pub async fn audit<E: Engine>(engine: &E, scenario: &Scenario, role: &str) -> (usize, Vec<String>) {
    let dialect = engine.dialect();
    let p = prepare(engine.name(), dialect, scenario, role);
    let sides = (
        pbps_diff::Side {
            schema: &p.base.schema,
            ids: &p.base_ids,
        },
        pbps_diff::Side {
            schema: &p.desired.schema,
            ids: &p.ids,
        },
    );
    let declared = &p.desired.hints.module_deps;
    let name = scenario.name;
    let read = match commute::read(sides.0, sides.1, dialect, declared, &p.changes, None) {
        Ok(read) => read,
        Err(e) => return (0, vec![format!("{name}: the effects cannot be read: {e}")]),
    };
    let n = p.changes.changes.len();
    let tag = |t: &str| format!("cx_{t}_{}", name.chars().take(28).collect::<String>());
    let own: Vec<usize> = (0..n).collect();
    let a = match outcome_of(
        engine,
        &tag("a"),
        &p,
        &p.creation,
        &p.changes,
        &own,
        &p.ids,
        true,
    )
    .await
    {
        Ok(a) => a,
        Err(f) => return (0, vec![format!("{name}: the plan's own order: {f:?}")]),
    };
    let listed = |i: usize, j: usize| {
        scenario.gap.is_some()
            && [i, j].iter().any(|&x| p.kinds[x] == scenario.pair[0])
            && [i, j].iter().any(|&x| p.kinds[x] == scenario.pair[1])
    };
    let (mut ran, mut found) = (0, Vec::new());
    for earlier in 0..n {
        for later in earlier + 1..n {
            if listed(earlier, later) {
                continue;
            }
            let Some(swapped) = commute::swapped(&read.report.graph, n, earlier, later) else {
                continue;
            };
            ran += 1;
            let what = format!(
                "{name}: {} #{earlier} and {} #{later} swapped",
                p.kinds[earlier], p.kinds[later]
            );
            let ended = outcome_of(
                engine,
                &tag("b"),
                &p,
                &p.creation,
                &p.changes,
                &swapped,
                &p.ids,
                false,
            )
            .await
            .and_then(|b| same(&what, &a, &b));
            if let Err(f) = ended {
                found.push(format!("{what}: {f:?}"));
            }
        }
    }
    (ran, found)
}

/// Runs the whole sample and fails with every scenario that is wrong: a pair
/// that does not commute, a stale pair, and a listed gap that no longer
/// shows. As an audit ([`audits`]) it swaps every independent pair instead,
/// in the sample and in `held`, and fails with each one that ends otherwise.
///
/// `held` are plans that hold a dependency an effect states, one per effect
/// the engine can show (#2091): with the effect deleted, the audit swaps the
/// pair it ordered and the engine refuses or ends otherwise. They run only
/// in the audit, so they cost the merge group nothing and the cap does not
/// count them.
pub async fn suite<E: Engine>(engine: &E, scenarios: &[Scenario], held: &[Scenario], role: &str) {
    assert!(
        scenarios.len() <= CAP,
        "the sample is capped at {CAP} pairs"
    );
    let started = std::time::Instant::now();
    let (mut swaps, mut failures) = (0, Vec::new());
    let all: Vec<&Scenario> = if audits() {
        scenarios.iter().chain(held).collect()
    } else {
        scenarios.iter().collect()
    };
    for scenario in all {
        if audits() {
            let (ran, found) = audit(engine, scenario, role).await;
            swaps += ran;
            failures.extend(found);
        } else if let Some(failure) = verdict(scenario, check(engine, scenario, role).await) {
            failures.push(failure);
        }
    }
    eprintln!(
        "commutation {} ({}): {} scenarios, {} swapped pairs, in {:.1}s",
        if audits() { "audit" } else { "suite" },
        engine.name(),
        if audits() {
            scenarios.len() + held.len()
        } else {
            scenarios.len()
        },
        if audits() { swaps } else { scenarios.len() },
        started.elapsed().as_secs_f64()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[cfg(test)]
mod verdict_tests {
    use super::*;

    fn scenario(gap: Option<u32>) -> Scenario {
        Scenario {
            name: "s",
            pair: ["AddColumn", "AddColumn"],
            gap,
            base: "",
            desired: "",
        }
    }

    fn found(pair: Result<(), Finding>, reminted: Option<&str>) -> Checked {
        Checked {
            pair,
            reminted: reminted.map(str::to_owned),
        }
    }

    fn differs() -> Result<(), Finding> {
        Err(Finding::Differs("x".into()))
    }

    #[test]
    fn a_listed_gap_excuses_the_pairs_own_difference() {
        assert_eq!(verdict(&scenario(Some(1)), found(differs(), None)), None);
        assert_eq!(verdict(&scenario(None), found(Ok(()), None)), None);
    }

    /// The plan's own instability is never the pair's: a gap does not hide
    /// a plan that is not byte-identical from the same uids, or ends
    /// otherwise with new ones.
    #[test]
    fn a_listed_gap_does_not_excuse_an_unstable_plan() {
        let same_uids = || Err(Finding::Nondeterministic("not byte-identical".into()));
        let gap = scenario(Some(1));
        assert!(verdict(&gap, found(same_uids(), None)).is_some());
        assert!(verdict(&gap, found(differs(), Some("column order"))).is_some());
        let harness = Err(Finding::Harness("refused".into()));
        assert!(verdict(&gap, found(harness, None)).is_some());
    }

    /// Nothing excuses the plan with new uids ending otherwise, and the
    /// swapped run is judged beside it: neither hides the other (#2104).
    #[test]
    fn a_plan_that_ends_otherwise_with_new_uids_fails_whatever_is_listed() {
        for gap in [None, Some(1)] {
            assert!(verdict(&scenario(gap), found(Ok(()), Some("x"))).is_some());
        }
        let both = verdict(&scenario(None), found(differs(), Some("new uids"))).unwrap();
        assert!(
            both.contains("new uids") && both.contains("Differs"),
            "{both}"
        );
    }

    /// A swapped order that runs but leaves a next plan is the pair's
    /// difference, which a gap may list; the plan's own order doing so is a
    /// broken scenario, and a read that fails is the harness's either way.
    #[test]
    fn a_swapped_order_that_does_not_converge_differs() {
        let unconverged = || Finding::Differs("the next plan is not empty".into());
        assert!(matches!(
            converged(false, unconverged()),
            Finding::Differs(_)
        ));
        assert!(matches!(
            converged(true, unconverged()),
            Finding::Harness(_)
        ));
        let unread = || Finding::Harness("pull failed".into());
        assert!(matches!(converged(false, unread()), Finding::Harness(_)));
    }

    fn held() -> Outcome {
        Outcome {
            catalog: Schema::default(),
            columns: Vec::new(),
            access: Vec::new(),
            rows: Vec::new(),
        }
    }

    /// The plan with new uids refused, or left short of the declarations,
    /// ended otherwise: that is its result, kept beside the pair's, never a
    /// broken harness that would stop the swapped run being judged.
    #[test]
    fn a_new_uid_plan_that_does_not_converge_is_its_result_not_the_harness() {
        let refused = Err(Finding::Differs("c: the next plan is not empty".into()));
        assert!(matches!(reminted_of(&held(), refused), Ok(Some(_))));
        assert!(matches!(reminted_of(&held(), Ok(held())), Ok(None)));
        let unread = Err(Finding::Harness("c: pull failed".into()));
        assert!(matches!(
            reminted_of(&held(), unread),
            Err(Finding::Harness(_))
        ));
    }

    #[test]
    fn a_gap_that_no_longer_shows_or_is_ordered_fails() {
        assert!(verdict(&scenario(Some(1)), found(Ok(()), None)).is_some());
        let stale = Err(Finding::Stale("edge".into()));
        assert!(verdict(&scenario(Some(1)), found(stale, None)).is_some());
        assert!(verdict(&scenario(None), found(differs(), None)).is_some());
    }
}
