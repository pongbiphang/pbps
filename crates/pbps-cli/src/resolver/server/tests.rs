use super::*;

#[test]
fn generated_scratch_names_are_unique_and_never_collide_with_each_other() {
    let first = generated_names().unwrap();
    let second = generated_names().unwrap();
    assert!(first.database().starts_with("pbps_scratch_"));
    assert!(first.login().starts_with("pbps_run_"));
    assert_ne!(first.database(), second.database());
    assert_ne!(first.login(), second.login());
    assert_ne!(first.password(), second.password());
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
