//! CLI lowering keeps the selected dialect's immutable groups through execution.
use pbps_dialect::{Dialect, DialectError, Lexicon, Statement, TransactionFraming, TypeChangeRisk};
use pbps_diff::order::PlanView;
use pbps_model::{Change, ColumnType, PlanMode, SessionZone, Strategy, Table, TableName};

/// This private adapter retains the selected dialect, including its observed
/// capabilities. Its concrete brand cannot be substituted by a caller's emitter.
pub(crate) struct Emitter<'a>(pub(crate) &'a dyn Dialect);
pub(crate) type LoweredPlan<'a> = pbps_dialect::lower::LoweredPlan<Emitter<'a>>;
impl pbps_dialect::PlanDialect for Emitter<'_> {
    type Plan = dyn PlanView<dyn pbps_dialect::Dialect>;
    fn emit(&self, plan: &Self::Plan, position: usize) -> Result<Vec<Statement>, DialectError> {
        let planned = &pbps_dialect::PlanPayload::changes(plan).changes[position];
        self.0.legacy_emit(&planned.change, planned.strategy)
    }
    fn emit_staged(
        &self,
        plan: &Self::Plan,
        position: usize,
    ) -> Result<Vec<Statement>, DialectError> {
        let planned = &pbps_dialect::PlanPayload::changes(plan).changes[position];
        self.0.legacy_emit_staged(&planned.change, planned.strategy)
    }
}
impl Dialect for Emitter<'_> {
    fn name(&self) -> &'static str {
        self.0.name()
    }
    fn legacy_emit(
        &self,
        change: &Change,
        strategy: Strategy,
    ) -> Result<Vec<Statement>, DialectError> {
        self.0.legacy_emit(change, strategy)
    }
    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
        self.0.normalize_type(ty)
    }
    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
        self.0.type_change_risk(from, to)
    }
    fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
        self.0.fold_ident(ident)
    }
    fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
        self.0.quote_ident(ident)
    }
    fn lexicon(&self) -> Lexicon {
        self.0.lexicon()
    }
    fn validate_table(&self, name: &TableName, table: &Table) -> Vec<DialectError> {
        self.0.validate_table(name, table)
    }
    fn transaction_framing(&self, zone: &SessionZone) -> TransactionFraming {
        self.0.transaction_framing(zone)
    }
    fn probe_framing(&self) -> Option<TransactionFraming> {
        self.0.probe_framing()
    }
    fn session_pins(&self, zone: &SessionZone) -> Option<String> {
        self.0.session_pins(zone)
    }
    fn batch_separator(&self) -> Option<&'static str> {
        self.0.batch_separator()
    }
}

pub(crate) fn lower<'a>(
    plan: &(dyn PlanView<dyn pbps_dialect::Dialect> + 'static),
    dialect: &'a dyn Dialect,
) -> anyhow::Result<LoweredPlan<'a>> {
    pbps_dialect::lower::lower_all(plan, &Emitter(dialect))
        .map_err(|error| anyhow::anyhow!("cannot render a change as SQL: {error}"))
}

/// Counts the same private emission for review, without creating a lowered
/// collection from an untrusted preview or pretending that it was admitted.
pub(crate) fn statement_count(
    changes: &pbps_model::ChangeSet,
    dialect: &dyn Dialect,
    mode: PlanMode,
) -> anyhow::Result<usize> {
    changes.changes.iter().try_fold(0, |count, planned| {
        dialect
            .statement_count(&planned.change, planned.strategy, mode)
            .map(|next| count + next)
            .map_err(|error| anyhow::anyhow!("cannot render a change as SQL: {error}"))
    })
}
