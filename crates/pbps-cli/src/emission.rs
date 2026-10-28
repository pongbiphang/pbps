//! Concrete engine routing retains the lowering brand through CLI execution.
use pbps_dialect::{Dialect, Statement};
use pbps_diff::order::PlanView;
use pbps_model::{PlanMode, SessionZone};

/// The already selected engine, including its immutable configuration.
#[derive(Clone)]
pub enum SelectedDialect {
    Postgres(pbps_pg::Postgres),
    Mssql(pbps_mssql::Mssql),
}
impl AsRef<dyn Dialect> for SelectedDialect {
    fn as_ref(&self) -> &(dyn Dialect + 'static) {
        match self {
            Self::Postgres(dialect) => dialect,
            Self::Mssql(dialect) => dialect,
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
    pub trait Script {}
    impl Script for super::LoweredPlan {}
    impl Script for super::BootstrapScript {}
}

/// Concrete dispatch finishes before certification. A semantic trait object
/// cannot lower a plan or erase the actual engine's certificate brand.
pub trait Engine:
    sealed::Engine + pbps_dialect::PlanDialect<Plan = dyn PlanView<Self>> + Clone + Sized + 'static
{
    fn as_ref(&self) -> &Self {
        self
    }
    fn lower(&self, plan: &Self::Plan) -> Result<LoweredPlan, pbps_dialect::DialectError>;
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
    fn lower(&self, plan: &Self::Plan) -> Result<LoweredPlan, pbps_dialect::DialectError> {
        Ok(LoweredPlan {
            inner: EnginePlan::Postgres {
                dialect: self.clone(),
                plan: pbps_dialect::lower::lower_all(plan, self)?,
            },
        })
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
    fn lower(&self, plan: &Self::Plan) -> Result<LoweredPlan, pbps_dialect::DialectError> {
        Ok(LoweredPlan {
            inner: EnginePlan::Mssql {
                dialect: *self,
                plan: pbps_dialect::lower::lower_all(plan, self)?,
            },
        })
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

/// Keep the selected concrete type in the rest of the existing route without
/// moving any stage, query or refusal across the selection point.
#[doc(hidden)]
#[macro_export]
macro_rules! with_plan_dialect {
    ($selected:expr, |$dialect:ident| $body:block) => {
        match $selected {
            $crate::emission::SelectedDialect::Postgres($dialect) => $body,
            $crate::emission::SelectedDialect::Mssql($dialect) => $body,
        }
    };
    ($selected:expr, |mut $dialect:ident| $body:block) => {
        match $selected {
            $crate::emission::SelectedDialect::Postgres(mut $dialect) => $body,
            $crate::emission::SelectedDialect::Mssql(mut $dialect) => $body,
        }
    };
}

#[derive(Clone)]
enum EnginePlan {
    Postgres {
        dialect: pbps_pg::Postgres,
        plan: pbps_dialect::lower::LoweredPlan<pbps_pg::Postgres>,
    },
    Mssql {
        dialect: pbps_mssql::Mssql,
        plan: pbps_dialect::lower::LoweredPlan<pbps_mssql::Mssql>,
    },
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
    inner: EnginePlan,
}
impl LoweredPlan {
    pub fn statements(&self) -> &[Statement] {
        match &self.inner {
            EnginePlan::Postgres { plan, .. } => plan.statements(),
            EnginePlan::Mssql { plan, .. } => plan.statements(),
        }
    }
    pub fn groups(&self) -> &[pbps_dialect::lower::Lowered] {
        match &self.inner {
            EnginePlan::Postgres { plan, .. } => plan.groups(),
            EnginePlan::Mssql { plan, .. } => plan.groups(),
        }
    }
    pub fn render(&self, zone: &SessionZone) -> Result<String, pbps_dialect::DialectError> {
        match &self.inner {
            EnginePlan::Postgres { dialect, plan } => {
                pbps_dialect::render_script(plan, dialect, zone)
            }
            EnginePlan::Mssql { dialect, plan } => pbps_dialect::render_script(plan, dialect, zone),
        }
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

pub enum BootstrapScript {
    Postgres {
        dialect: pbps_pg::Postgres,
        script: pbps_dialect::BootstrapScript<pbps_pg::Postgres>,
    },
    Mssql {
        dialect: pbps_mssql::Mssql,
        script: pbps_dialect::BootstrapScript<pbps_mssql::Mssql>,
    },
}
impl Script for BootstrapScript {
    fn render(&self, zone: &SessionZone) -> Result<String, pbps_dialect::DialectError> {
        match self {
            Self::Postgres { dialect, script } => script.render(dialect, zone),
            Self::Mssql { dialect, script } => script.render(dialect, zone),
        }
    }
}
impl LoweredPlan {
    pub fn bootstrap_script(
        &self,
        schema: &pbps_model::Schema,
        trusted: &std::collections::BTreeSet<pbps_model::RoleName>,
    ) -> anyhow::Result<BootstrapScript> {
        Ok(match &self.inner {
            EnginePlan::Postgres { dialect, plan } => BootstrapScript::Postgres {
                dialect: dialect.clone(),
                script: pbps_dialect::prepare_bootstrap_script(plan, dialect, schema, trusted)?,
            },
            EnginePlan::Mssql { dialect, plan } => {
                if !trusted.is_empty() {
                    anyhow::bail!("trusted_roles is supported only on PostgreSQL");
                }
                BootstrapScript::Mssql {
                    dialect: *dialect,
                    script: pbps_dialect::prepare_bootstrap_script(plan, dialect, schema, trusted)?,
                }
            }
        })
    }
}
