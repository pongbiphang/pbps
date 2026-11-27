//! Declaration diagnostics and report projection; catalog SQL stays in the engine.

use pbps_model::dependency::{Dependencies, PlannedDependencies};

pub fn problems(
    declarations: &Dependencies,
    dialect: &dyn pbps_dialect::Dialect,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (name, declaration) in declarations {
        if !dialect.has_cross_database_dependencies() {
            out.push((
                "dialect.rejected",
                format!(
                    "dependency `{}`: cross-database dependencies are supported only on SQL Server, not `{}`",
                    name.shown(),
                    dialect.name()
                ),
            ));
            continue;
        }
        for (column, definition) in &declaration.columns {
            if let Err(error) = dialect.normalize_type(&definition.ty) {
                out.push((
                    "dialect.rejected",
                    format!(
                        "dependency `{}` column `{}`: {error}",
                        name.shown(),
                        column.shown()
                    ),
                ));
            }
        }
    }
    out
}

pub fn render(dependencies: &PlannedDependencies) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (name, dependency) in dependencies {
        let _ = writeln!(
            out,
            "Dependency `{}` ({}): {}",
            name.shown(),
            dependency.declaration.kind.as_str(),
            dependency.comparison.as_str()
        );
        for (column, definition) in &dependency.declaration.columns {
            let _ = writeln!(
                out,
                "  `{}`: {}, nullable: {}",
                column.shown(),
                definition.ty.shown(),
                definition.nullable
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::dependency::{Dependency, DependencyColumn, DependencyKind, DependencyName};

    #[test]
    fn cross_database_dependencies_are_refused_naming_the_dialect_outside_sql_server() {
        let mut deps = Dependencies::new();
        let name: DependencyName = "other.dbo.t".parse().unwrap();
        deps.insert(
            name,
            Dependency {
                kind: DependencyKind::Table,
                columns: [(
                    "id".into(),
                    DependencyColumn {
                        ty: "int".parse().unwrap(),
                        nullable: false,
                    },
                )]
                .into(),
            },
        );
        for (dialect, expected) in [
            (
                &pbps_sqlite::dialect::Sqlite::default() as &dyn pbps_dialect::Dialect,
                "sqlite",
            ),
            (
                &pbps_pg::Postgres::new() as &dyn pbps_dialect::Dialect,
                "postgres",
            ),
        ] {
            let found = problems(&deps, dialect);
            assert_eq!(found.len(), 1, "{expected}: {found:?}");
            assert_eq!(found[0].0, "dialect.rejected");
            assert!(
                found[0]
                    .1
                    .contains(&format!("only on SQL Server, not `{expected}`")),
                "{found:?}"
            );
        }
        // Negative: SQL Server accepts valid cross-database dependencies.
        assert!(problems(&deps, &pbps_mssql::Mssql).is_empty());
    }
}
