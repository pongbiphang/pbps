use super::*;

#[derive(Default)]
pub(crate) struct State {
    transaction: Transaction,
    retained: Option<pbps_pg::retained::Held>,
    verified: Option<pbps_pg::retained::Verified>,
}

impl ApplySession for pbps_pg::Postgres {
    type State = State;
    fn state<'s>(&self, state: &'s mut Self::State) -> &'s mut Transaction {
        &mut state.transaction
    }
    fn evidence<'s>(&self, state: &'s Self::State) -> &'s Transaction {
        &state.transaction
    }
    fn retained_comparison<'s>(
        &self,
        state: &'s Self::State,
    ) -> Option<&'s dyn pbps_dialect::RetainedComparison> {
        state
            .verified
            .as_ref()
            .map(|verified| verified as &dyn pbps_dialect::RetainedComparison)
    }
    /// Retention is a connected engine guard, never a second plan emitter.
    async fn hold_retained(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        changes: &ChangeSet,
        authored: Option<&Schema>,
    ) -> anyhow::Result<()> {
        state.retained = if pbps_model::retained::expressions(changes).next().is_none() {
            None
        } else {
            anyhow::ensure!(
                conn.driver() == pbps_db::Driver::Postgres,
                "retained expressions require PostgreSQL"
            );
            Some(
                pbps_pg::retained::hold(conn, changes, authored)
                    .await
                    .map_err(anyhow::Error::msg)?,
            )
        };
        Ok(())
    }
    async fn verify_retained(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        after: &Schema,
    ) -> anyhow::Result<()> {
        state.verified = match state.retained.as_ref() {
            Some(held) => Some(
                held.verify(conn, &pbps_pg::Postgres::new(), after)
                    .await
                    .map_err(anyhow::Error::msg)?,
            ),
            None => None,
        };
        Ok(())
    }
    fn rebuild_mode(&self, changes: &ChangeSet, staged: bool) -> anyhow::Result<()> {
        require_transactional_rebuilds(Driver::Postgres, changes, staged)
    }
    fn rename_targets(&self, names: &pbps_model::plan_names::PlanNames<'_>) -> Vec<RenameTarget> {
        super::super::rename_targets(Driver::Postgres, names)
    }
}
