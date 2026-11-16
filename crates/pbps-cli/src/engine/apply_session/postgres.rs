use super::*;

impl ApplySession for pbps_pg::Postgres {
    type State = Transaction;
    fn state<'s>(&self, state: &'s mut Self::State) -> &'s mut Transaction {
        state
    }
    fn evidence<'s>(&self, state: &'s Self::State) -> &'s Transaction {
        state
    }
    fn rebuild_mode(&self, changes: &ChangeSet, staged: bool) -> anyhow::Result<()> {
        require_transactional_rebuilds(Driver::Postgres, changes, staged)
    }
    fn rename_targets(&self, names: &pbps_model::plan_names::PlanNames<'_>) -> Vec<RenameTarget> {
        super::super::rename_targets(Driver::Postgres, names)
    }
}
