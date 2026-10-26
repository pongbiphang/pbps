use super::*;

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
    for (driver, text) in [
        (
            pbps_db::Driver::Mssql,
            "Server=db,1433;User Id=sa;Password=secret",
        ),
        (pbps_db::Driver::Postgres, "host=db user=x"),
        // PostgreSQL has no LocalDB: the text is not read for a selector.
        (pbps_db::Driver::Postgres, r"Server=(localdb)\x"),
    ] {
        assert_eq!(engine::endpoint(driver, text).await.unwrap(), text);
    }
}

#[cfg(not(windows))]
#[tokio::test]
async fn a_localdb_selector_off_windows_is_an_acquire_failure_that_repeats_no_secret() {
    let text = r"Server=(localdb)\x;Integrated Security=SSPI;Encrypt=true";
    let error = engine::endpoint(pbps_db::Driver::Mssql, text)
        .await
        .unwrap_err();
    let ProduceError::Acquire(why) = &error else {
        panic!("{error}");
    };
    assert!(
        why.contains("LocalDB") && why.contains("only available on Windows"),
        "{why}"
    );
    let unsupported = engine::endpoint(
        pbps_db::Driver::Mssql,
        r"Server=(localdb)\.\SHARED;Integrated Security=SSPI;Password=hunter2",
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(!unsupported.contains("hunter2"), "{unsupported}");
}
