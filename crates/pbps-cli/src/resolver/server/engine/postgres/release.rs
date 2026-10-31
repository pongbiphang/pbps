//! Supplied cleanup never restores an unconfirmed backend (DEC-1731.1).

use super::{Conn, Standard, described_list};
use pbps_db::DbError;
use pbps_model::escape::Escaped;
use pbps_pg::resolver::{recovery as record, scratch as sql, standard};
use std::collections::BTreeSet;

#[cfg(test)]
use super::for_tests;

fn unconfirmed(database: &str, error: impl std::fmt::Display) -> Vec<String> {
    vec![format!(
        "the supplied release cannot confirm its checked backend for database {} ({error})",
        Escaped(database)
    )]
}

async fn begin_checked(
    admin: &mut Conn,
    backend: &sql::Backend,
    database: &str,
) -> Result<(), Vec<String>> {
    // A failed cleanup statement may have left an aborted transaction. End it
    // before checking the new transaction; no restoration precedes that check.
    let bound = async {
        admin.execute("ROLLBACK").await?;
        admin.execute("BEGIN").await?;
        sql::on_backend(admin, backend).await
    }
    .await;
    if !matches!(bound, Ok(true)) {
        let reason = match bound {
            Ok(false) => {
                "the connection now reaches another backend than the one the run checked".to_owned()
            }
            Err(error) => error.to_string(),
            Ok(true) => unreachable!(),
        };
        let ended = admin.execute("ROLLBACK").await;
        let mut named = unconfirmed(database, reason);
        if let Err(error) = ended {
            named.push(format!(
                "the unconfirmed backend's transaction did not end ({error})"
            ));
        }
        return Err(named);
    }
    Ok(())
}

async fn commit_phase(admin: &mut Conn, database: &str, phase: &str) -> Result<(), Vec<String>> {
    admin.execute("COMMIT").await.map_err(|error| {
        vec![format!(
            "database {}'s {phase} commit is unconfirmed ({error})",
            Escaped(database)
        )]
    })
}

/// The existing helper owns the DROP ROLE SQL and its ROLE_BATCH size. A
/// refused statement aborts our explicit transaction, so individual retries
/// need fresh checked transactions rather than another savepoint in the same
/// growing lock set (DEC-2049.1).
async fn role_batch(
    admin: &mut Conn,
    backend: &sql::Backend,
    database: &str,
    roles: &[pbps_model::RoleName],
) -> Result<bool, Vec<String>> {
    begin_checked(admin, backend, database).await?;
    let failed = pbps_pg::resolver::drop_roles(admin, roles).await;
    if failed.is_empty() {
        #[cfg(test)]
        for_tests::release_role_phase(admin, database, false).await;
        commit_phase(admin, database, "role cleanup").await?;
        #[cfg(test)]
        for_tests::release_role_phase(admin, database, true).await;
        Ok(true)
    } else {
        admin.execute("ROLLBACK").await.map_err(|error| {
            vec![format!(
                "database {}'s role cleanup transaction did not end ({error})",
                Escaped(database)
            )]
        })?;
        Ok(false)
    }
}

async fn cleanup_roles(
    admin: &mut Conn,
    backend: &sql::Backend,
    database: &str,
    roles: &[pbps_model::RoleName],
) -> Result<Vec<pbps_model::RoleName>, Vec<String>> {
    let mut failed = Vec::new();
    for batch in roles.chunks(pbps_pg::resolver::ROLE_BATCH) {
        if role_batch(admin, backend, database, batch).await? {
            continue;
        }
        // drop_roles' internal retry cannot recover an aborted explicit
        // transaction. End that batch before these independent retries; each
        // successful drop releases its locks before trying the next role.
        for role in batch {
            if !role_batch(admin, backend, database, std::slice::from_ref(role)).await? {
                failed.push(role.clone());
            }
        }
    }
    Ok(failed)
}

async fn begin_step(conn: &mut Conn) -> Result<(), DbError> {
    conn.execute("SAVEPOINT pbps_release_step").await
}

/// Recovery must not continue in the transaction a refused statement aborted.
/// The standard's inner savepoints preserve its individual successful repairs;
/// these outer steps also recover unreadable metadata before its commit.
async fn finish_step<T, E>(conn: &mut Conn, result: Result<T, E>) -> Result<Result<T, E>, DbError> {
    if result.is_err() {
        conn.execute("ROLLBACK TO SAVEPOINT pbps_release_step")
            .await?;
    }
    conn.execute("RELEASE SAVEPOINT pbps_release_step").await?;
    Ok(result)
}

/// Every transaction first proves the original backend. Restoration and its
/// verification share one transaction; bounded role cleanup and final claim
/// removal each obtain fresh proofs after the preceding commits.
pub(in crate::resolver::server) async fn supplied(
    admin: &mut Conn,
    backend: &sql::Backend,
    declared: &Standard,
    database: &str,
    defaults: &BTreeSet<(String, String)>,
    acl: &BTreeSet<standard::Entry>,
) -> Result<(), Vec<String>> {
    let cleaned = sql::drop_owned(admin, backend).await;
    #[cfg(test)]
    for_tests::release_after_cleanup(admin, database);
    #[cfg(test)]
    if for_tests::stopped_after_cleanup(database) {
        return Err(vec!["stopped after the cleanup, as a kill would".into()]);
    }
    let mut remaining = match cleaned {
        Err(sql::DropOwnedError::BackendUnconfirmed(error)) => {
            return Err(unconfirmed(database, error));
        }
        Ok((_, 0)) => Vec::new(),
        Ok((named, total)) => vec![format!(
            "{total} object(s) in database {database} owned by a role other than \
             the scratch account ({})",
            described_list(&named)
        )],
        Err(sql::DropOwnedError::Statement(error)) => vec![format!(
            "every object the scratch account owns in database {database} ({error})"
        )],
    };
    begin_checked(admin, backend, database).await?;
    let mut roles: Vec<pbps_model::RoleName> = Vec::new();
    #[cfg(test)]
    for_tests::release_phase(admin, database).await;
    let restored: Result<(), DbError> = async {
        begin_step(admin).await?;
        let result = standard::enforce_in_transaction(admin, declared, Some(acl)).await;
        let put_back = finish_step(admin, result).await?;
        begin_step(admin).await?;
        let result = standard::verify(admin, declared, Some(acl)).await;
        match finish_step(admin, result).await? {
            Ok(left) if left.is_empty() => {}
            Ok(left) => {
                remaining.extend(left.into_iter().map(|difference| {
                    format!("database {database} not back in its standard state: {difference}")
                }));
                match put_back {
                    Ok(failed) => {
                        remaining.extend(failed.into_iter().map(|why| format!("refused: {why}")))
                    }
                    Err(error) => remaining.push(format!("refused: {error}")),
                }
            }
            Err(error) => remaining.push(format!(
                "database {database}'s standard state, unreadable after the run ({error})"
            )),
        }
        begin_step(admin).await?;
        let result = sql::login_defaults(admin).await;
        match finish_step(admin, result).await? {
            Ok(after) => remaining.extend(
                sql::changed_defaults(defaults, &after)
                    .into_iter()
                    .map(|change| format!("the scratch account's own default {change}")),
            ),
            Err(error) => remaining.push(format!(
                "the scratch account's own defaults, unreadable after the run ({error})"
            )),
        }
        begin_step(admin).await?;
        let result = record::read_claim(admin).await;
        match finish_step(admin, result).await? {
            Ok(Some(Ok(claim))) => {
                roles = claim.roles.into_iter().map(Into::into).collect();
            }
            Ok(None | Some(Err(_))) => {}
            Err(error) => remaining.push(format!(
                "the roles in the scratch database's claim, unreadable ({error})"
            )),
        }
        Ok(())
    }
    .await;
    match restored {
        Ok(()) => {
            #[cfg(test)]
            for_tests::release_phase(admin, database).await;
            if let Err(errors) = commit_phase(admin, database, "restoration").await {
                remaining.extend(errors);
                return Err(remaining);
            }
        }
        Err(error) => {
            remaining.push(format!(
                "database {}'s restoration transaction could not finish ({error})",
                Escaped(database)
            ));
            if let Err(error) = admin.execute("ROLLBACK").await {
                remaining.push(format!("the restoration transaction did not end ({error})"));
            }
            return Err(remaining);
        }
    }
    match cleanup_roles(admin, backend, database, &roles).await {
        Ok(failed) => remaining.extend(
            failed
                .into_iter()
                .map(|role| format!("role {}", role.shown())),
        ),
        Err(errors) => {
            remaining.extend(errors);
            return Err(remaining);
        }
    }
    // Restoration and every role batch have committed. The final obligation
    // still needs its own backend proof; a successful earlier batch is not a
    // capability to remove a claim on the connection's next transaction.
    if remaining.is_empty() {
        begin_checked(admin, backend, database).await?;
        match record::remove(admin).await {
            Ok(()) => commit_phase(admin, database, "claim removal").await?,
            Err(error) => {
                remaining.push(format!("the scratch database's claim ({error})"));
                if let Err(error) = admin.execute("ROLLBACK").await {
                    remaining.push(format!(
                        "the claim removal transaction did not end ({error})"
                    ));
                }
            }
        }
    }
    if remaining.is_empty() {
        Ok(())
    } else {
        Err(remaining)
    }
}
