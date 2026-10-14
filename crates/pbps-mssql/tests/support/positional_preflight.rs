use super::*;

fn positional_dialect() -> impl pbps_dialect::Dialect {
    Mssql
}
fn positional_schema() -> &'static str {
    "dbo"
}

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../pbps-dialect/tests/support/positional_preflight.rs"
));
