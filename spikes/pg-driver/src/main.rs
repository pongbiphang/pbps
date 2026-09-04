//! Re-implementing `pbps-db`'s seam over `tokio-postgres`, to find out what the
//! seam actually costs a second driver. Every deviation from the shape in
//! `crates/pbps-db/src/lib.rs` is marked `SEAM:` and is a finding.

use std::time::Duration;
use tokio_postgres::{Client, NoTls};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("the connection string is malformed: {0}")]
    BadConnectionString(String),
    #[error("{0}")]
    Driver(#[from] tokio_postgres::Error),
    #[error("unexpected row shape: {0}")]
    BadRow(String),
}

impl DbError {
    /// SEAM 1: `pbps-db` returns `Option<u32>` here. PostgreSQL's error code is
    /// a five-character SQLSTATE that is not always numeric (`42P01`), so the
    /// return type cannot hold it. Returning the *string* is the only faithful
    /// option, which makes this a signature change in `pbps-db`.
    pub fn server_error_code(&self) -> Option<String> {
        match self {
            DbError::Driver(e) => e.code().map(|c| c.code().to_owned()),
            DbError::BadConnectionString(_) | DbError::BadRow(_) => None,
        }
    }
}

pub struct Row(tokio_postgres::Row);

pub trait FromColumn<'a>: Sized {
    fn from_column(row: &'a Row, col: &str) -> Result<Option<Self>, DbError>;
    fn from_index(row: &'a Row, idx: usize) -> Result<Option<Self>, DbError>;
}

/// SEAM 2: the blanket implementation carries over unchanged in shape. The one
/// difference is where NULL is expressed: tiberius's `try_get` yields
/// `Option<T>`, tokio-postgres asks for `Option<T>` as the requested type. The
/// seam's own `Result<Option<Self>>` absorbs both.
impl<'a, T: tokio_postgres::types::FromSql<'a>> FromColumn<'a> for T {
    fn from_column(row: &'a Row, col: &str) -> Result<Option<Self>, DbError> {
        Ok(row.0.try_get::<_, Option<T>>(col)?)
    }
    fn from_index(row: &'a Row, idx: usize) -> Result<Option<Self>, DbError> {
        Ok(row.0.try_get::<_, Option<T>>(idx)?)
    }
}

impl Row {
    pub fn try_get<'a, T: FromColumn<'a>>(&'a self, col: &str) -> Result<Option<T>, DbError> {
        T::from_column(self, col)
    }
    pub fn try_get_at<'a, T: FromColumn<'a>>(&'a self, idx: usize) -> Result<Option<T>, DbError> {
        T::from_index(self, idx)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Param<'a> {
    I32(i32),
    I64(i64),
    Str(&'a str),
    OptStr(Option<&'a str>),
}

impl Param<'_> {
    /// SEAM 3: `+ Sync` is required here and not by tiberius. It is satisfied by
    /// every variant, so it costs a bound and nothing else.
    fn as_sql(&self) -> &(dyn tokio_postgres::types::ToSql + Sync) {
        match self {
            Param::I32(v) => v,
            Param::I64(v) => v,
            Param::Str(v) => v,
            Param::OptStr(v) => v,
        }
    }
}

pub struct Conn {
    client: Client,
    /// SEAM 4: tokio-postgres splits into a `Client` and a `Connection` future
    /// that something must drive. `Conn` therefore owns a task handle that has
    /// no counterpart in the tiberius shape.
    _driver: tokio::task::JoinHandle<()>,
}

impl Conn {
    /// SEAM 5: the parameter is a libpq keyword/URI string, not an ADO.NET one.
    /// `Conn::connect(&str)` keeps its signature; what a valid string *is*
    /// becomes dialect knowledge, which the current doc comment places here.
    pub async fn connect(connection_string: &str) -> Result<Self, DbError> {
        let (client, connection) =
            match tokio::time::timeout(CONNECT_TIMEOUT, tokio_postgres::connect(connection_string, NoTls)).await {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => return Err(DbError::Driver(e)),
                Err(_) => return Err(DbError::BadConnectionString("connect timed out".into())),
            };
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Self { client, _driver: driver })
    }

    pub async fn query(&mut self, sql: &str) -> Result<Vec<Row>, DbError> {
        Ok(self.client.query(sql, &[]).await?.into_iter().map(Row).collect())
    }

    /// SEAM 6: `batch_execute` is the analogue of `simple_query` — several
    /// statements, no parameter machinery, sent as the emitter framed them.
    pub async fn execute(&mut self, sql: &str) -> Result<(), DbError> {
        self.client.batch_execute(sql).await?;
        Ok(())
    }

    /// SEAM 7: the placeholders are `$1`, not `@P1`. Every caller of this method
    /// lives in a dialect crate and writes its own SQL, so the spelling does not
    /// cross the seam — but the doc comment on `pbps-db` names `@P1` and would
    /// be wrong for the second driver.
    pub async fn query_with(&mut self, sql: &str, params: &[Param<'_>]) -> Result<Vec<Row>, DbError> {
        let bound: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params.iter().map(Param::as_sql).collect();
        Ok(self.client.query(sql, &bound).await?.into_iter().map(Row).collect())
    }

    pub async fn execute_with(&mut self, sql: &str, params: &[Param<'_>]) -> Result<u64, DbError> {
        let bound: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params.iter().map(Param::as_sql).collect();
        Ok(self.client.execute(sql, &bound).await?)
    }

    /// SEAM 8: `pbps-db` sends `SET XACT_ABORT ON; BEGIN TRANSACTION;` — T-SQL,
    /// in the crate CLAUDE.md says holds none. PostgreSQL needs no equivalent:
    /// any error already dooms the transaction. So the statement differs per
    /// dialect even though the *framing* is shared.
    pub async fn begin(&mut self) -> Result<(), DbError> {
        self.execute("BEGIN;").await
    }
    pub async fn commit(&mut self) -> Result<(), DbError> {
        self.execute("COMMIT;").await
    }
    pub async fn rollback(&mut self) -> Result<(), DbError> {
        self.execute("ROLLBACK;").await
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cs = "host=127.0.0.1 port=55432 user=postgres password=probe dbname=postgres";
    let mut c = Conn::connect(cs).await?;
    println!("connect: ok");

    c.execute("DROP SCHEMA IF EXISTS spike CASCADE; CREATE SCHEMA spike;").await?;
    println!("execute (multi-statement batch): ok");

    let rows = c.query("SELECT 42 AS answer, NULL::text AS nothing").await?;
    let answer: Option<i32> = rows[0].try_get("answer")?;
    let nothing: Option<&str> = rows[0].try_get("nothing")?;
    let by_index: Option<i32> = rows[0].try_get_at(0)?;
    println!("query + FromColumn: answer={answer:?} nothing={nothing:?} by_index={by_index:?}");

    c.execute("CREATE TABLE spike.t (id int primary key, v text)").await?;
    let n = c.execute_with("INSERT INTO spike.t VALUES ($1, $2)", &[Param::I32(1), Param::Str("one")]).await?;
    println!("execute_with: {n} row(s) affected");
    let rows = c.query_with("SELECT v FROM spike.t WHERE id = $1", &[Param::I32(1)]).await?;
    let v: Option<&str> = rows[0].try_get("v")?;
    println!("query_with: v={v:?}");

    // A NULL parameter must reach the server as NULL, not as an empty string —
    // the invariant `an_absent_string_is_not_an_empty_one` guards in pbps-db.
    c.execute_with("INSERT INTO spike.t VALUES ($1, $2)", &[Param::I32(2), Param::OptStr(None)]).await?;
    let rows = c.query("SELECT v IS NULL AS is_null FROM spike.t WHERE id = 2").await?;
    let is_null: Option<bool> = rows[0].try_get("is_null")?;
    println!("absent is not empty: v IS NULL = {is_null:?}");

    // SPEC 11.5: a plan whose second statement fails leaves the first nowhere.
    c.begin().await?;
    c.execute("INSERT INTO spike.t VALUES (10, 'first')").await?;
    let failed = c.execute("INSERT INTO spike.t VALUES (1, 'duplicate key')").await;
    println!("second statement failed: {}", failed.is_err());
    if let Err(e) = &failed {
        println!("  SQLSTATE: {:?}", e.server_error_code());
    }
    c.rollback().await?;
    let rows = c.query("SELECT count(*) AS n FROM spike.t WHERE id = 10").await?;
    let n: Option<i64> = rows[0].try_get("n")?;
    println!("all-or-nothing: rows left by the first statement = {n:?} (must be 0)");

    // SEAM 1, made unarguable: a SQLSTATE that no `u32` can hold.
    let undefined = c.query("SELECT * FROM spike.no_such_table").await;
    if let Err(e) = &undefined {
        let code = e.server_error_code();
        println!("undefined table SQLSTATE: {code:?}, parses as u32: {:?}",
                 code.as_deref().map(|c| c.parse::<u32>().is_ok()));
    }

    c.execute("DROP SCHEMA spike CASCADE").await?;
    Ok(())
}
