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
