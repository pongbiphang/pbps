//! Declaration diagnostics and report projection; catalog SQL stays in the engine.

use pbps_model::dependency::{Dependencies, PlannedDependencies};

pub fn problems(
    declarations: &Dependencies,
    dialect: &dyn pbps_dialect::Dialect,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (name, declaration) in declarations {
        if dialect.name() != "mssql" {
            out.push((
                "dialect.rejected",
                format!(
                    "dependency `{}`: cross-database dependencies are supported only on SQL Server",
                    name.shown()
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
