//! Paired binding shares the unsealed code-goal lifecycle (DEC-2354.1).
use super::{Provenance, Standard, domain};
use pbps_db::Conn;
use pbps_model::{Hints, IdsFile, PlanMode, RetainedExpression, Schema};

pub(super) struct Comparison<'a> {
    pub before: pbps_diff::Side<'a>,
    pub after: pbps_diff::Side<'a>,
    pub live: &'a Schema,
    pub hints: &'a Hints,
    pub requests: &'a [RetainedExpression],
}
impl Comparison<'_> {
    pub async fn run(&self, conn: &mut Conn) -> Result<Vec<RetainedExpression>, String> {
        let pg = pbps_pg::Postgres::new();
        let old = Schema {
            types: self.before.schema.types.clone(),
            ..Default::default()
        };
        let new = Schema {
            types: self.after.schema.types.clone(),
            ..Default::default()
        };
        let old_ids = IdsFile {
            types: self.before.ids.types.clone(),
            ..Default::default()
        };
        let new_ids = IdsFile {
            types: self.after.ids.types.clone(),
            ..Default::default()
        };
        let before = pbps_diff::Side {
            schema: &old,
            ids: &old_ids,
        };
        let after = pbps_diff::Side {
            schema: &new,
            ids: &new_ids,
        };
        let changes = pbps_diff::diff(before, after, &pg, self.hints)
            .map_err(|_| "cannot order the label changes")?;
        if changes.changes().changes.iter().any(|p| {
            !matches!(
                p.change,
                pbps_model::Change::RenameEnumValue { .. } | pbps_model::Change::RenameType { .. }
            )
        }) {
            return Ok(Vec::new());
        }
        let ordered = pbps_diff::order::deployment::schedule::schedule(
            changes,
            &pbps_diff::order::deployment::schedule::Declarations {
                schema: &new,
                sources: pbps_diff::order::deployment::Sources {
                    before,
                    after: &new_ids,
                },
                module_deps: &self.hints.module_deps,
            },
            &Default::default(),
            &pg,
        )
        .map_err(|e| e.to_string())?
        .changes
        .ordered(PlanMode::Transactional, &pg);
        let lowered = pbps_dialect::lower::lower_all(
            &ordered as &dyn pbps_diff::order::PlanView<pbps_pg::Postgres>,
            &pg,
        )
        .map_err(|e| e.to_string())?;
        let mut trees = Vec::new();
        for (index, e) in self.requests.iter().enumerate() {
            let table = e.target.table();
            let result = async {
                let shape = self
                    .live
                    .tables
                    .get(table)
                    .ok_or("the carrier table is absent")?;
                let expr = e
                    .target
                    .expression(self.live)
                    .ok_or("the live expression is absent")?;
                let carrier =
                    pbps_pg::retained::create_carrier(conn, &pg, table, shape, index, false)
                        .await?;
                let tree = pbps_pg::retained::bind(conn, &pg, &e.target, expr, &carrier).await?;
                Ok::<_, String>((carrier, tree))
            }
            .await;
            trees.push(result.ok());
        }
        for statement in lowered.statements() {
            conn.execute(&statement.sql)
                .await
                .map_err(|_| "the approved label rename did not bind")?;
        }
        let mut equal = Vec::new();
        for (e, tree) in self.requests.iter().zip(trees) {
            let Some((carrier, tree)) = tree else {
                continue;
            };
            if let Ok(wanted) =
                pbps_pg::retained::bind(conn, &pg, &e.target, &e.after, &carrier).await
                && pbps_pg::retained::equal(conn, &tree, &wanted).await == Ok(true)
            {
                equal.push(e.clone());
            }
        }
        Ok(equal)
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn compare(
    scratch: &str,
    target: &str,
    identity: &str,
    before: pbps_diff::Side<'_>,
    after: pbps_diff::Side<'_>,
    live: &Schema,
    hints: &Hints,
    requests: &[RetainedExpression],
    standard: impl Into<Standard>,
    externals: Option<&str>,
    provenance: &Provenance,
) -> Result<Vec<RetainedExpression>, domain::Refused> {
    let comparison = Comparison {
        before,
        after,
        live,
        hints,
        requests,
    };
    domain::run(
        scratch,
        target,
        identity,
        before,
        standard,
        externals,
        provenance,
        Some(&comparison),
    )
    .await
}
