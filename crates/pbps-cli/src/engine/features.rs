//! Per-engine feature declarations (DEC-2798.19).
//!
//! Each engine implements [`Features`] in full, naming either its real
//! implementation or a feature module's `Absent` marker. Associated types have
//! no defaults in stable Rust, so adding a feature stops compilation at every
//! engine until it answers.

pub(crate) mod data_triggers;

/// The optional features an engine implements or declares absent.
///
/// `PlanningDialect` supplies each engine's refusal display name (`E::NAME`,
/// pinned equal to `engine::of(E::DRIVER).name()`), so every feature's
/// `Absent` refusal names the engine as the rest of the CLI does without
/// duplicating a name table.
pub(crate) trait Features:
    crate::emission::Engine + pbps_cli::resolver::server::PlanningDialect
{
    type DataTriggers: data_triggers::Feature<Self>;
}

impl Features for pbps_pg::Postgres {
    type DataTriggers = data_triggers::Postgres;
}

impl Features for pbps_mssql::Mssql {
    type DataTriggers = data_triggers::Absent;
}

impl Features for pbps_sqlite::dialect::Sqlite {
    type DataTriggers = data_triggers::Absent;
}
