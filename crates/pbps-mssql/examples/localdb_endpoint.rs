//! Prints the connection string a `(localdb)` selector resolves to now.
//!
//! The Windows LocalDB job (`scripts/localdb_qualify.py`) gives the shared SQL
//! Server suites resolved strings, and this is how it gets them: through the
//! same discovery the resolver runs, so a discovery failure fails the job
//! where the suites would otherwise have been handed a string by other means.
//! The string holds a pipe name and no secret: LocalDB authenticates the
//! current Windows user.

use pbps_mssql::resolver::localdb;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let Some(selector) = std::env::args().nth(1) else {
        eprintln!("usage: localdb_endpoint '<connection string with Server=(localdb)\\name>'");
        return std::process::ExitCode::from(2);
    };
    match localdb::resolve(
        &selector,
        localdb::helper().as_deref(),
        pbps_db::CONNECT_TIMEOUT,
    )
    .await
    {
        Ok(Some(found)) => {
            println!("{}", found.connection());
            std::process::ExitCode::SUCCESS
        }
        Ok(None) => {
            eprintln!("the string names no `(localdb)` instance");
            std::process::ExitCode::FAILURE
        }
        Err(failure) => {
            eprintln!("{failure}");
            std::process::ExitCode::FAILURE
        }
    }
}
