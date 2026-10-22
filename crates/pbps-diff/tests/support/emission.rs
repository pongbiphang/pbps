// Test-only emitter observations from the real singleton route. No certificate
// factory or test-support feature is shipped by any crate.
use pbps_dialect::{Dialect, DialectError, PlanDialect, Statement};
use pbps_model::{Change, PlannedChange, Strategy};
// Shared by fixtures that exercise either emission mode.
#[allow(dead_code)]
pub trait FixtureEmission: Dialect {
    fn fixture_emit(&self, change: &Change, strategy: Strategy) -> Result<Vec<Statement>, DialectError>;
    fn fixture_emit_staged(&self, change: &Change, strategy: Strategy) -> Result<Vec<Statement>, DialectError>;
}
impl<D: PlanDialect<Plan = dyn pbps_diff::order::PlanView<D>> + 'static> FixtureEmission for D {
    fn fixture_emit(
        &self,
        change: &Change,
        strategy: Strategy,
    ) -> Result<Vec<Statement>, DialectError> {
        let mut part = PlannedChange::new(change.clone());
        part.strategy = strategy;
        self.emit(
            &pbps_diff::order::singleton(part, pbps_model::PlanMode::Transactional, self),
            0,
        )
    }
    fn fixture_emit_staged(
        &self,
        change: &Change,
        strategy: Strategy,
    ) -> Result<Vec<Statement>, DialectError> {
        let mut part = PlannedChange::new(change.clone());
        part.strategy = strategy;
        self.emit_staged(
            &pbps_diff::order::singleton(part, pbps_model::PlanMode::Staged, self),
            0,
        )
    }
}
