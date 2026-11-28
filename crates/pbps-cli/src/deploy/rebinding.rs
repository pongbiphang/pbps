//! Which of the rebuilds a dialect asks for by text a connected plan keeps
//! (DEC-2987.5).
//!
//! A dialect makes an unchanged module a rebuild candidate when its text
//! mentions a name the plan makes arrive (DECISIONS 422). The mention is all
//! an offline plan has. A connected one can ask the engine what the module
//! bound when it was created, and a mention that bound nothing an arrival can
//! move is then no reason to rebuild.

use pbps_db::Conn;
use pbps_model::{Change, ChangeSet, ModuleChange, ModuleId, Schema};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ops::Deref;

/// The modules `plan` restates that the target already holds.
fn restated(plan: &ChangeSet, base: &Schema) -> BTreeSet<ModuleId> {
    plan.changes
        .iter()
        .filter_map(|planned| {
            if let Change::Module(ModuleChange::AlterModule { id, .. }) = &planned.change
                && base.modules.contains_key(id)
            {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect()
}

/// `ordinary`, without the candidate rebuilds the engine's recorded
/// dependencies prove unnecessary, and the note of a read that failed.
///
/// `exempt` is the set the differ leaves out of the dialect's candidates, and
/// `diff` plans again with whatever it holds. On return it holds exactly the
/// modules the returned plan was proven for, so every plan `diff` makes later
/// keeps the same narrowing.
///
/// Proven against the plan that is returned, not against a guess at it: the
/// facts are read for a plan, and a module is exempt only while the plan made
/// without its rebuild still proves it. Each round can only shrink the set,
/// so it ends. Anything short of a proof keeps the rebuild: no facts from
/// this engine, a failed read, a plan of a class the facts do not describe,
/// a routine whose body the engine did not bind.
pub(super) async fn narrowed<D, P, E>(
    dialect: &D,
    conn: &mut Conn,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    ordinary: P,
    exempt: &RefCell<BTreeSet<ModuleId>>,
    diff: impl Fn() -> Result<P, E>,
) -> Result<(P, Option<crate::output::Finding>), E>
where
    D: crate::engine::PlanSession,
    P: Deref<Target = ChangeSet>,
{
    let asked = restated(&ordinary, base.schema);
    if asked.is_empty() {
        return Ok((ordinary, None));
    }
    // The plan with no candidate at all shows which restatements were the
    // dialect's: the ones that are gone.
    exempt.replace(desired.schema.modules.keys().cloned().collect());
    let mut plan = diff()?;
    let mut proven: BTreeSet<ModuleId> = asked
        .difference(&restated(&plan, base.schema))
        .cloned()
        .collect();
    let mut note = None;
    loop {
        if !proven.is_empty() {
            proven = match pbps_diff::resolver::needed_facts(base, desired, &plan, dialect) {
                None => BTreeSet::new(),
                Some(needed) => match dialect.dependency_facts(conn, &needed).await {
                    Ok(Some(facts)) => pbps_diff::resolver::proven_by_dependency(
                        base, desired, &plan, dialect, &facts, &proven,
                    ),
                    Ok(None) => BTreeSet::new(),
                    Err(error) => {
                        note = Some(crate::resolution::rebuilds_kept_unread(&error));
                        BTreeSet::new()
                    }
                },
            };
        }
        if proven == *exempt.borrow() {
            return Ok((plan, note));
        }
        exempt.replace(proven.clone());
        if proven.is_empty() {
            return Ok((ordinary, note));
        }
        plan = diff()?;
    }
}
