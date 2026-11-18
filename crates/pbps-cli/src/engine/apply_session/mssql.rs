use super::*;

impl ApplySession for pbps_mssql::Mssql {
    type State = Transaction;
    fn state<'s>(&self, state: &'s mut Self::State) -> &'s mut Transaction {
        state
    }
    fn evidence<'s>(&self, state: &'s Self::State) -> &'s Transaction {
        state
    }
    fn retained_comparison<'s>(
        &self,
        _state: &'s Self::State,
    ) -> Option<&'s dyn pbps_dialect::RetainedComparison> {
        None
    }
    async fn hold_retained(
        &self,
        _state: &mut Self::State,
        _conn: &mut Conn,
        changes: &ChangeSet,
        _authored: Option<&Schema>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            pbps_model::retained::expressions(changes).next().is_none(),
            "retained expressions require PostgreSQL"
        );
        Ok(())
    }
    async fn verify_retained(
        &self,
        _state: &mut Self::State,
        _conn: &mut Conn,
        _after: &Schema,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn rebuild_mode(&self, changes: &ChangeSet, staged: bool) -> anyhow::Result<()> {
        require_transactional_rebuilds(Driver::Mssql, changes, staged)
    }
    fn rename_targets(&self, names: &pbps_model::plan_names::PlanNames<'_>) -> Vec<RenameTarget> {
        super::super::rename_targets(Driver::Mssql, names)
    }
}
