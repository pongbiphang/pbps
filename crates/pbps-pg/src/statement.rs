//! Execute emitter-owned command boundaries without interpreting declared SQL.
use pbps_db::{DbError, transport::ExecuteConnection};
use pbps_dialect::Statement;

/// The server position belongs to this native command, not the rendered batch.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct Failure {
    #[source]
    pub error: DbError,
    pub command: usize,
}

fn commands(statement: &Statement) -> &[String] {
    statement
        .execution
        .as_ref()
        .map(|e| e.commands.as_slice())
        .unwrap_or_else(|| std::slice::from_ref(&statement.sql))
}

pub(crate) fn submitted(statement: &Statement, command: usize) -> &str {
    commands(statement)
        .get(command)
        .map(String::as_str)
        .or_else(|| {
            statement
                .execution
                .as_ref()
                .and_then(|e| e.cleanup.as_deref())
        })
        .expect("the executor records a submitted command")
}

/// Unlike approved plan execution, binding must never accept a statement tail
/// hidden inside one declaration (DEC-2520.1).
pub async fn execute(
    conn: &mut impl ExecuteConnection,
    statement: &Statement,
) -> Result<(), Failure> {
    let commands = commands(statement);
    let result: Result<(), Failure> = async {
        for (command, sql) in commands.iter().enumerate() {
            conn.execute_extended(sql)
                .await
                .map_err(|error| Failure { error, command })?;
        }
        Ok(())
    }
    .await;
    let closed = match statement
        .execution
        .as_ref()
        .and_then(|e| e.cleanup.as_deref())
    {
        Some(cleanup) => conn
            .execute_extended(cleanup)
            .await
            .map_err(|error| Failure {
                error,
                command: commands.len(),
            }),
        None => Ok(()),
    };
    result?;
    closed
}
