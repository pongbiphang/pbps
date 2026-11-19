use super::*;
use crate::resolver::contract::ResolverEngine;

async fn endpoint<D: PlanningDialect>(text: &str) -> Result<String, ProduceError> {
    <D::Resolver as ResolverEngine>::endpoint(text).await
}

#[test]
fn a_runs_names_carry_its_token_and_two_runs_never_share_one() {
    let token = |n: u128| format!("{n:032x}");
    let (a, b) = (token(1), token(2));
    let first = names_for(&a).unwrap();
    let second = names_for(&b).unwrap();
    assert_eq!(first.database(), format!("pbps_scratch_{a}"));
    assert_eq!(first.login(), format!("pbps_run_{a}"));
    assert_eq!(
        names_for_reference(&a).unwrap().database(),
        format!("pbps_reference_{a}")
    );
    assert_ne!(first.database(), second.database());
    assert_ne!(first.login(), second.login());
    assert_ne!(first.password(), second.password());
    assert!(names_for("short").is_err());
}

#[tokio::test]
async fn an_ordinary_scratch_string_is_its_own_endpoint_for_both_engines() {
    let text = "Server=db,1433;User Id=sa;Password=secret";
    assert_eq!(endpoint::<pbps_mssql::Mssql>(text).await.unwrap(), text);
    for text in [
        "host=db user=x",
        // PostgreSQL has no LocalDB: the text is not read for a selector.
        r"Server=(localdb)\x",
    ] {
        assert_eq!(endpoint::<pbps_pg::Postgres>(text).await.unwrap(), text);
    }
}

#[cfg(not(windows))]
#[tokio::test]
async fn a_localdb_selector_off_windows_is_an_acquire_failure_that_repeats_no_secret() {
    let text = r"Server=(localdb)\x;Integrated Security=SSPI;Encrypt=true";
    let error = endpoint::<pbps_mssql::Mssql>(text).await.unwrap_err();
    let ProduceError::Acquire(why) = &error else {
        panic!("{error}");
    };
    assert!(
        why.contains("LocalDB") && why.contains("only available on Windows"),
        "{why}"
    );
    let unsupported = endpoint::<pbps_mssql::Mssql>(
        r"Server=(localdb)\.\SHARED;Integrated Security=SSPI;Password=hunter2",
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(!unsupported.contains("hunter2"), "{unsupported}");
}
