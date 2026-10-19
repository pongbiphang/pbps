//! Lowering: an operation becomes its statements, with what the emitter
//! already knows about running them (#1776 slice S1a, DEC-1961.1).
//!
//! [`Dialect::emit`] works one change at a time, so lowering an
//! [`Operation`] is a regrouping, not a second emitter: it is `emit` on the
//! operation's own change, and every fact about a statement is the one the
//! emitter wrote on it. How a statement runs depends on more than its change
//! — whether an index is built concurrently depends on the strategy — and
//! deriving it anywhere else would be a second copy of the emitter's rules
//! that drifts from the first.

use pbps_model::operation::{Operation, OperationId, Operations};
use pbps_model::{Strategy, TableName};

use crate::{Dialect, DialectError, Released, Statement};

/// Whether a statement can run inside the plan's transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transaction {
    Inside,
    /// It cannot ([`Statement::transactional`]): an ordinary plan carrying
    /// one is refused before it runs (SPEC §7.5).
    Outside,
}

/// Whether a statement may share a batch with its neighbours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Batch {
    Shared,
    /// Sent alone ([`Statement::own_batch`]).
    Own,
}

/// How a statement has to be run: two requirements, not one. A batch of its
/// own does not mean outside the transaction. PostgreSQL's concurrent index
/// build needs both, and several SQL Server statements need their own batch
/// inside the transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Execution {
    pub transaction: Transaction,
    pub batch: Batch,
}

impl Execution {
    /// What the emitter wrote on the statement.
    pub fn of(statement: &Statement) -> Self {
        Self {
            transaction: if statement.transactional {
                Transaction::Inside
            } else {
                Transaction::Outside
            },
            batch: if statement.own_batch {
                Batch::Own
            } else {
                Batch::Shared
            },
        }
    }
}

/// One statement of a lowered operation, with the operation it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step<'a> {
    pub operation: OperationId,
    pub statement: &'a Statement,
    pub execution: Execution,
}

/// An operation's statements, in the order the emitter wrote them.
///
/// One `Lowered` is one indivisible group: its statements run in this order
/// with no other operation's between them. A scheduler places operations,
/// never statements, and nothing reorders after emission (#1776), so the
/// group is the type rather than a range a caller could split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lowered {
    operation: OperationId,
    statements: Vec<Statement>,
}

impl Lowered {
    pub fn operation(&self) -> OperationId {
        self.operation
    }

    pub fn statements(&self) -> &[Statement] {
        &self.statements
    }

    pub fn into_statements(self) -> Vec<Statement> {
        self.statements
    }

    /// Each statement with its operation and its execution requirements.
    pub fn steps(&self) -> impl Iterator<Item = Step<'_>> {
        self.statements.iter().map(|statement| Step {
            operation: self.operation,
            statement,
            execution: Execution::of(statement),
        })
    }

    /// The names a table holds between two statements of this operation:
    /// one a statement renames it to and a later one renames it away from,
    /// which is in neither the baseline nor the plan ([`Statement::renames`]).
    pub fn intermediate_tables(&self) -> Vec<&TableName> {
        intermediate(self.statements.iter().map(|s| s.renames.as_slice()))
    }

    /// The same for a role ([`Statement::role_renames`]).
    pub fn intermediate_roles(&self) -> Vec<&pbps_model::RoleName> {
        intermediate(self.statements.iter().map(|s| s.role_renames.as_slice()))
    }

    /// What each statement drops, by its position in the operation
    /// ([`Statement::releases`]).
    pub fn releases(&self) -> impl Iterator<Item = (usize, &Released)> {
        self.statements
            .iter()
            .enumerate()
            .flat_map(|(i, s)| s.releases.iter().map(move |r| (i, r)))
    }
}

/// Every name a rename leads to that a later rename leads away from, in
/// order.
fn intermediate<'s, A: PartialEq + 's>(renames: impl Iterator<Item = &'s [(A, A)]>) -> Vec<&'s A> {
    let all: Vec<&(A, A)> = renames.flatten().collect();
    all.iter()
        .enumerate()
        .filter(|(i, (_, to))| all[i + 1..].iter().any(|(from, _)| from == to))
        .map(|(_, (_, to))| to)
        .collect()
}

/// The statements of one operation, as the dialect emits its change under
/// `strategy`.
pub fn lower(
    operation: &Operation,
    strategy: Strategy,
    dialect: &dyn Dialect,
) -> Result<Lowered, DialectError> {
    Ok(Lowered {
        operation: operation.id(),
        statements: dialect.emit(operation.change(), strategy)?,
    })
}

/// Every operation lowered in order, each under the strategy its own planned
/// change carries.
pub fn lower_all(
    operations: &Operations,
    dialect: &dyn Dialect,
) -> Result<Vec<Lowered>, DialectError> {
    operations
        .iter()
        .map(|op| lower(op, op.planned().strategy, dialect))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MinimalDialect;
    use pbps_model::operation::ModuleEdits;
    use pbps_model::{Change, PlannedChange};

    /// A dialect whose renames go through a parking name, as both engines'
    /// cross-schema moves do, and whose role rename does the same.
    struct Parking;

    impl Dialect for Parking {
        fn name(&self) -> &'static str {
            "parking"
        }
        // A fixture: every change it has no shape for writes one plain
        // statement, which is all these tests read.
        #[allow(clippy::wildcard_enum_match_arm)]
        fn emit(
            &self,
            change: &Change,
            strategy: Strategy,
        ) -> Result<Vec<Statement>, DialectError> {
            Ok(match change {
                Change::RenameTable { from, to, .. } => {
                    let parked = TableName::new(&to.schema, &from.name);
                    vec![
                        Statement::new("MOVE").renaming(from.clone(), parked.clone()),
                        Statement::new("RENAME")
                            .own_batch()
                            .renaming(parked, to.clone()),
                    ]
                }
                Change::RenameRole { from, to, .. } => vec![
                    Statement::new("PARK ROLE").renaming_role(from.clone(), "parked"),
                    Statement::new("NAME ROLE").renaming_role("parked", to.clone()),
                ],
                Change::DropModule { id, .. } => vec![
                    Statement::new("DROP")
                        .non_transactional()
                        .releasing(Released::Module(id.clone())),
                ],
                // Whatever else: run outside the transaction only when told
                // to build online, which is the strategy's whole effect.
                _ if strategy.online => vec![Statement::new("ONLINE").non_transactional()],
                _ => vec![Statement::new("OTHER")],
            })
        }
        fn lexicon(&self) -> crate::Lexicon {
            MinimalDialect.lexicon()
        }
        fn transaction_framing(&self, zone: &pbps_model::SessionZone) -> crate::TransactionFraming {
            MinimalDialect.transaction_framing(zone)
        }
        fn probe_framing(&self) -> Option<crate::TransactionFraming> {
            None
        }
        fn normalize_type(
            &self,
            ty: &pbps_model::ColumnType,
        ) -> Result<pbps_model::ColumnType, DialectError> {
            MinimalDialect.normalize_type(ty)
        }
        fn type_change_risk(
            &self,
            from: &pbps_model::ColumnType,
            to: &pbps_model::ColumnType,
        ) -> crate::TypeChangeRisk {
            MinimalDialect.type_change_risk(from, to)
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
            MinimalDialect.fold_ident(ident)
        }
        fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
            MinimalDialect.quote_ident(ident)
        }
        fn validate_table(&self, name: &TableName, table: &pbps_model::Table) -> Vec<DialectError> {
            MinimalDialect.validate_table(name, table)
        }
    }

    fn t(s: &str) -> TableName {
        s.parse().unwrap()
    }

    #[test]
    fn a_two_step_rename_shows_the_name_it_parks_the_table_under() {
        let rename = |from: &str, to: &str| {
            PlannedChange::new(Change::RenameTable {
                uid: "t_aaaaaa".parse().unwrap(),
                from: t(from),
                to: t(to),
                defaults: vec![],
            })
        };
        let ops = Operations::of_plan(
            &[rename("app.a", "dst.b"), rename("app.c", "app.d")],
            ModuleEdits::InPlace,
        );
        let lowered = lower_all(&ops, &Parking).unwrap();
        assert_eq!(lowered[0].intermediate_tables(), [&t("dst.a")]);
        // The renames' endpoints are not intermediate: the source and the
        // target are the baseline's and the plan's.
        assert!(!lowered[0].intermediate_tables().contains(&&t("dst.b")));
        // Within one operation only: the second rename's parking name is
        // its own, and its first name is nobody's intermediate.
        assert_eq!(lowered[1].intermediate_tables(), [&t("app.c")]);
        let role = Operations::of_plan(
            &[PlannedChange::new(Change::RenameRole {
                uid: "r_aaaaaa".parse().unwrap(),
                from: "old".into(),
                to: "new".into(),
            })],
            ModuleEdits::InPlace,
        );
        let role = lower_all(&role, &Parking).unwrap();
        assert_eq!(
            role[0].intermediate_roles(),
            [&pbps_model::RoleName::from("parked")]
        );
    }

    #[test]
    fn each_statement_carries_its_operation_and_the_emitters_two_requirements() {
        let id: pbps_model::ModuleId = "app.v".parse().unwrap();
        let mut online = PlannedChange::new(Change::CreateRole {
            uid: "r_bbbbbb".parse().unwrap(),
            name: "x".into(),
        });
        online.strategy = Strategy { online: true };
        let ops = Operations::of_plan(
            &[
                PlannedChange::new(Change::RenameTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    from: t("app.a"),
                    to: t("dst.b"),
                    defaults: vec![],
                }),
                PlannedChange::new(Change::DropModule {
                    id: id.clone(),
                    kind: pbps_model::ModuleKind::View,
                }),
                online,
            ],
            ModuleEdits::InPlace,
        );
        let lowered = lower_all(&ops, &Parking).unwrap();
        let steps: Vec<(String, Execution)> = lowered
            .iter()
            .flat_map(Lowered::steps)
            .map(|s| (s.operation.to_string(), s.execution))
            .collect();
        let inside = |batch| Execution {
            transaction: Transaction::Inside,
            batch,
        };
        let outside = |batch| Execution {
            transaction: Transaction::Outside,
            batch,
        };
        assert_eq!(
            steps,
            [
                ("0.0".to_owned(), inside(Batch::Shared)),
                ("0.0".to_owned(), inside(Batch::Own)),
                ("1.0".to_owned(), outside(Batch::Shared)),
                // The strategy the planned change carries reaches the
                // emitter: the same change without it is transactional.
                ("2.0".to_owned(), outside(Batch::Shared)),
            ]
        );
        assert_eq!(
            lowered[1].releases().collect::<Vec<_>>(),
            [(0, &Released::Module(id))]
        );
        assert_eq!(
            lower(&ops.as_slice()[2], Strategy::default(), &Parking)
                .unwrap()
                .steps()
                .map(|s| s.execution)
                .collect::<Vec<_>>(),
            [inside(Batch::Shared)]
        );
    }
}
