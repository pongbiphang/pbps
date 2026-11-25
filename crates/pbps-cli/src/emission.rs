//! Concrete engine routing retains the lowering brand through CLI execution.
use pbps_dialect::{Dialect, Statement};
use pbps_diff::order::PlanView;
use pbps_model::{PlanMode, SessionZone};

/// The already selected engine, including its immutable configuration.
#[derive(Clone)]
pub enum SelectedDialect {
    Postgres(pbps_pg::Postgres),
    Mssql(pbps_mssql::Mssql),
    Sqlite(pbps_sqlite::dialect::Sqlite),
}
impl AsRef<dyn Dialect> for SelectedDialect {
    fn as_ref(&self) -> &(dyn Dialect + 'static) {
        match self {
            Self::Postgres(dialect) => dialect,
            Self::Mssql(dialect) => dialect,
            Self::Sqlite(dialect) => dialect,
        }
    }
}

impl std::ops::Deref for SelectedDialect {
    type Target = dyn Dialect;
    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

mod sealed {
    pub trait Engine {}
    impl Engine for pbps_pg::Postgres {}
    impl Engine for pbps_mssql::Mssql {}
    impl Engine for pbps_sqlite::dialect::Sqlite {}
    pub trait Script {}
    impl Script for super::LoweredPlan {}
    impl Script for super::BootstrapScript {}
}

/// Concrete dispatch finishes before certification. A semantic trait object
/// cannot lower a plan or erase the actual engine's certificate brand.
pub trait Engine:
    sealed::Engine
    + pbps_dialect::PlanDialect<Plan = dyn PlanView<Self>>
    + Clone
    + Send
    + Sync
    + Sized
    + 'static
{
    fn as_ref(&self) -> &Self {
        self
    }
    /// The one place a lowered collection is made: the engine that lowered
    /// the plan stays with its statements, behind [`LoweredPlan`].
    fn lower(&self, plan: &Self::Plan) -> Result<LoweredPlan, pbps_dialect::DialectError> {
        Ok(LoweredPlan {
            inner: std::sync::Arc::new(Branded {
                dialect: self.clone(),
                plan: pbps_dialect::lower::lower_all(plan, self)?,
            }),
        })
    }
    /// Whether bootstrap may be given `trusted_roles`. An engine that answers
    /// no is refused them before its guard is prepared.
    fn trusts_roles(&self) -> bool;
    /// The connected seam asks at the old refusal point. PostgreSQL's module
    /// expansion has always used the empty write path and observed version.
    fn with_planning_version(&self, version: i64) -> Self;
    /// Keep observed attach names on the selected concrete planning engine.
    fn with_attach_constraint_names(
        &self,
        names: std::collections::BTreeMap<
            pbps_model::Uid,
            std::collections::BTreeSet<pbps_model::ObjectLocalName>,
        >,
    ) -> Self;
}
impl Engine for pbps_pg::Postgres {
    fn trusts_roles(&self) -> bool {
        true
    }
    fn with_planning_version(&self, version: i64) -> Self {
        Self::new().with_server_version(version)
    }
    fn with_attach_constraint_names(
        &self,
        names: std::collections::BTreeMap<
            pbps_model::Uid,
            std::collections::BTreeSet<pbps_model::ObjectLocalName>,
        >,
    ) -> Self {
        self.clone().with_attach_constraint_names(names)
    }
}
impl Engine for pbps_mssql::Mssql {
    fn trusts_roles(&self) -> bool {
        false
    }
    fn with_planning_version(&self, _version: i64) -> Self {
        *self
    }
    fn with_attach_constraint_names(
        &self,
        _names: std::collections::BTreeMap<
            pbps_model::Uid,
            std::collections::BTreeSet<pbps_model::ObjectLocalName>,
        >,
    ) -> Self {
        *self
    }
}
impl Engine for pbps_sqlite::dialect::Sqlite {
    fn trusts_roles(&self) -> bool {
        false
    }
    /// PostgreSQL's server version number has no meaning on SQLite; the
    /// project's version floor is already carried by `self`.
    fn with_planning_version(&self, _version: i64) -> Self {
        *self
    }
    /// SQLite has no partitions or staged-attach scan CHECKs.
    fn with_attach_constraint_names(
        &self,
        _names: std::collections::BTreeMap<
            pbps_model::Uid,
            std::collections::BTreeSet<pbps_model::ObjectLocalName>,
        >,
    ) -> Self {
        *self
    }
}

/// Keep the selected concrete type in the rest of the existing route without
/// moving any stage, query or refusal across the selection point.
#[doc(hidden)]
#[macro_export]
macro_rules! with_plan_dialect {
    ($selected:expr, |$dialect:ident| $body:block) => {
        match $selected {
            $crate::emission::SelectedDialect::Postgres($dialect) => $body,
            $crate::emission::SelectedDialect::Mssql($dialect) => $body,
            $crate::emission::SelectedDialect::Sqlite($dialect) => $body,
        }
    };
    ($selected:expr, |mut $dialect:ident| $body:block) => {
        match $selected {
            $crate::emission::SelectedDialect::Postgres(mut $dialect) => $body,
            $crate::emission::SelectedDialect::Mssql(mut $dialect) => $body,
            $crate::emission::SelectedDialect::Sqlite(mut $dialect) => $body,
        }
    };
}

/// A lowered plan with the engine that lowered it. The brand `E` is what
/// certifies the statements, so it is kept here and never erased from the
/// value itself: only the handle the CLI passes around forgets which engine
/// it is.
struct Branded<E: Engine> {
    dialect: E,
    plan: pbps_dialect::lower::LoweredPlan<E>,
}

/// What the CLI does with a lowered plan, whichever engine lowered it. One
/// generic implementation serves every engine, so a new one adds no arm here.
trait Lowered: Send + Sync {
    fn statements(&self) -> &[Statement];
    fn groups(&self) -> &[pbps_dialect::lower::Lowered];
    fn render(&self, zone: &SessionZone) -> Result<String, pbps_dialect::DialectError>;
    fn bootstrap_script(
        &self,
        schema: &pbps_model::Schema,
        trusted: &std::collections::BTreeSet<pbps_model::RoleName>,
    ) -> anyhow::Result<BootstrapScript>;
}

impl<E: Engine> Lowered for Branded<E> {
    fn statements(&self) -> &[Statement] {
        self.plan.statements()
    }
    fn groups(&self) -> &[pbps_dialect::lower::Lowered] {
        self.plan.groups()
    }
    fn render(&self, zone: &SessionZone) -> Result<String, pbps_dialect::DialectError> {
        pbps_dialect::render_script(&self.plan, &self.dialect, zone)
    }
    fn bootstrap_script(
        &self,
        schema: &pbps_model::Schema,
        trusted: &std::collections::BTreeSet<pbps_model::RoleName>,
    ) -> anyhow::Result<BootstrapScript> {
        if !trusted.is_empty() && !self.dialect.trusts_roles() {
            anyhow::bail!("trusted_roles is supported only on PostgreSQL");
        }
        Ok(BootstrapScript {
            inner: Box::new(Prelude {
                dialect: self.dialect.clone(),
                script: pbps_dialect::prepare_bootstrap_script(
                    &self.plan,
                    &self.dialect,
                    schema,
                    trusted,
                )?,
            }),
        })
    }
}

/// A bootstrap script with the engine that prepared it, as [`Branded`] is for
/// a lowered plan.
struct Prelude<E: Engine> {
    dialect: E,
    script: pbps_dialect::BootstrapScript<E>,
}

trait Prepared: Send + Sync {
    fn render(&self, zone: &SessionZone) -> Result<String, pbps_dialect::DialectError>;
}

impl<E: Engine> Prepared for Prelude<E> {
    fn render(&self, zone: &SessionZone) -> Result<String, pbps_dialect::DialectError> {
        self.script.render(&self.dialect, zone)
    }
}

/// The collection accepted by CLI transaction, staged/resume and rehearsal
/// execution. Its actual engine and immutable operation groups stay together.
/// A raw vector or a caller-defined emitter cannot manufacture this value.
/// ```compile_fail
/// let raw: Vec<pbps_dialect::Statement> = Vec::new();
/// let _: pbps_cli::emission::LoweredPlan = raw.into();
/// ```
/// ```compile_fail
/// fn replace(plan: &mut pbps_cli::emission::LoweredPlan) { plan.reverse(); }
/// ```
/// ```compile_fail
/// fn fake<E: ?Sized>(plan: pbps_dialect::lower::LoweredPlan<E>) {
///     let _: pbps_cli::emission::LoweredPlan = plan.into();
/// }
/// ```
/// ```compile_fail
/// let _ = serde_json::from_str::<pbps_cli::emission::LoweredPlan>("{}");
/// ```
/// ```compile_fail
/// let _ = serde_json::from_str::<pbps_diff::order::OrderedPlan<pbps_pg::Postgres>>("{}");
/// ```
/// ```compile_fail
/// let _ = serde_json::from_str::<pbps_diff::order::AdmittedPlan<pbps_pg::Postgres>>("{}");
/// ```
#[derive(Clone)]
pub struct LoweredPlan {
    /// Shared, never mutated: a clone is the same certified collection.
    inner: std::sync::Arc<dyn Lowered>,
}
impl LoweredPlan {
    pub fn statements(&self) -> &[Statement] {
        self.inner.statements()
    }
    pub fn groups(&self) -> &[pbps_dialect::lower::Lowered] {
        self.inner.groups()
    }
    pub fn render(&self, zone: &SessionZone) -> Result<String, pbps_dialect::DialectError> {
        self.inner.render(zone)
    }
}
impl std::ops::Deref for LoweredPlan {
    type Target = [Statement];
    fn deref(&self) -> &[Statement] {
        self.statements()
    }
}
impl<'a> IntoIterator for &'a LoweredPlan {
    type Item = &'a Statement;
    type IntoIter = std::slice::Iter<'a, Statement>;
    fn into_iter(self) -> Self::IntoIter {
        self.statements().iter()
    }
}

/// Counts the same private emission for review, without creating a lowered
/// collection from an untrusted preview or pretending that it was admitted.
pub fn statement_count(
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

/// Only certified lowered output or its fixed bootstrap prelude can be rendered.
///
/// ```compile_fail
/// struct Raw;
/// impl pbps_cli::emission::Script for Raw {
///     fn render(&self, _: &pbps_model::SessionZone) -> String {
///         "SELECT 1;".to_owned()
///     }
/// }
/// ```
pub trait Script: sealed::Script {
    fn render(&self, zone: &SessionZone) -> Result<String, pbps_dialect::DialectError>;
}
impl Script for LoweredPlan {
    fn render(&self, zone: &SessionZone) -> Result<String, pbps_dialect::DialectError> {
        LoweredPlan::render(self, zone)
    }
}

pub struct BootstrapScript {
    inner: Box<dyn Prepared>,
}
impl Script for BootstrapScript {
    fn render(&self, zone: &SessionZone) -> Result<String, pbps_dialect::DialectError> {
        self.inner.render(zone)
    }
}
impl LoweredPlan {
    pub fn bootstrap_script(
        &self,
        schema: &pbps_model::Schema,
        trusted: &std::collections::BTreeSet<pbps_model::RoleName>,
    ) -> anyhow::Result<BootstrapScript> {
        self.inner.bootstrap_script(schema, trusted)
    }
}

#[cfg(test)]
mod tests {
    use super::Engine;

    /// The one thing a lowered plan still asks its engine by itself: every
    /// other question is answered by the generic implementation (#2950).
    #[test]
    fn only_postgresql_may_be_given_trusted_roles_at_bootstrap() {
        assert!(pbps_pg::Postgres::new().trusts_roles());
        // Negative: SQL Server and SQLite are refused them before any guard is prepared.
        assert!(!pbps_mssql::Mssql.trusts_roles());
        assert!(!pbps_sqlite::dialect::Sqlite::default().trusts_roles());
    }

    #[test]
    fn sqlite_engine_keeps_its_floor_through_server_only_configuration_hooks() {
        let custom = pbps_sqlite::dialect::Sqlite::with_floor(pbps_sqlite::Version::new(3, 40, 0));
        assert_eq!(custom.with_planning_version(170_000), custom);
        assert_eq!(
            custom.with_attach_constraint_names(Default::default()),
            custom
        );
        // Negative: a different floor is not folded into the default.
        assert_ne!(custom, pbps_sqlite::dialect::Sqlite::default());
    }
}
