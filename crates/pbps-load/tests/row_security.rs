use std::path::Path;

use pbps_model::{PolicyCommand, PolicyRoles};

fn load(extra: &str) -> Result<pbps_load::LoadedTable, Vec<pbps_load::LoadError>> {
    pbps_load::load_table_str(
        Path::new("policy.yml"),
        &format!(
            "table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\nprimary_key: [id]\n{extra}"
        ),
    )
}

#[test]
fn row_security_loader_refuses_invalid_command_and_target_combinations() {
    for (policy, expected) in [
        ("{command: select, with_check: 'true'}", "WITH CHECK cannot"),
        ("{command: delete, with_check: 'true'}", "WITH CHECK cannot"),
        ("{command: insert, using: 'true'}", "only WITH CHECK"),
        ("{to: []}", "at least one role"),
        ("{to: [public, app_user]}", "`public` cannot appear"),
        ("{to: [public]}", "`public` cannot appear"),
        ("{to: [current_user]}", "session-dependent"),
        ("{to: [session_user]}", "session-dependent"),
        ("{to: [current_role]}", "session-dependent"),
        ("{to: app_user}", "scalar `public`"),
        ("{to: null}", "scalar `public`"),
        ("{command: truncate}", "unknown variant"),
        ("{misspelled: 'true'}", "unknown field"),
    ] {
        let errors = load(&format!("row_policies:\n  p: {policy}\n"))
            .expect_err(policy)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(errors.contains(expected), "{policy}: {errors}");
    }
    assert!(load("row_security: {enable: true}\n").is_err());
    assert!(load("row_security: {enabled: null}\n").is_err());
    assert!(load("row_policies: {p: {}, p: {}}\n").is_err());
    for policy in [
        "{}",
        "{command: insert, with_check: 'true'}",
        "{command: update, using: 'true', with_check: 'true'}",
        "{command: select, to: public}",
        "{command: delete, to: [PUBLIC, CURRENT_USER], using: 'true'}",
    ] {
        load(&format!("row_policies:\n  p: {policy}\n")).unwrap();
    }
}

#[test]
fn row_security_policies_refuse_data_even_when_security_is_disabled() {
    let data = "data: {mode: ensure, rows: {1: {}}}\n";
    for security in [
        "row_security: {enabled: true}\n",
        "row_security: {forced: true}\n",
        "row_policies: {p: {}}\n",
    ] {
        let errors = load(&format!("{security}{data}")).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("`data:` cannot")),
            "{errors:?}"
        );
    }
    load(&format!(
        "row_security: {{enabled: false, forced: false}}\nrow_policies: {{}}\n{data}"
    ))
    .unwrap();
}

#[test]
fn row_security_format_round_trips_every_command_mode_and_flag_combination() {
    for (command, predicates) in [
        ("all", "using: 'id > 0', with_check: 'id > 0',"),
        ("select", "using: 'id > 0',"),
        ("insert", "with_check: 'id > 0',"),
        ("update", "using: 'id > 0', with_check: 'id > 0',"),
        ("delete", "using: 'id > 0',"),
    ] {
        for restrictive in [false, true] {
            for enabled in [false, true] {
                for forced in [false, true] {
                    for target in ["public", "[app_user, PUBLIC]"] {
                        let loaded = load(&format!(
                            "row_security: {{enabled: {enabled}, forced: {forced}}}\nrow_policies:\n  p: {{command: {command}, restrictive: {restrictive}, {predicates} to: {target}}}\n"
                        )).unwrap();
                        let policy = &loaded.table.row_policies["p"];
                        assert_eq!(policy.command.as_str(), command);
                        assert_eq!(policy.restrictive, restrictive);
                        assert_eq!(loaded.table.row_security.enabled, enabled);
                        assert_eq!(loaded.table.row_security.forced, forced);
                        let rendered =
                            pbps_load::fmt::render(&loaded.name, &loaded.table, &[], None);
                        let back =
                            pbps_load::load_table_str(Path::new("policy.yml"), &rendered).unwrap();
                        assert_eq!(back, loaded);
                    }
                }
            }
        }
    }
}

#[test]
fn row_security_format_preserves_authored_text_targets_and_omitted_predicates() {
    let mut loaded = load("checks: {p: 'id > 0'}\nrow_security: {forced: true}\nrow_policies:\n  p: {}\n  named: {command: update, restrictive: true, to: [z, PUBLIC, z], using: 'id > 0', with_check: 'id > 0'}\n").unwrap();
    assert_eq!(loaded.table.row_policies["p"].to, PolicyRoles::Public);
    assert_eq!(loaded.table.row_policies["p"].command, PolicyCommand::All);
    let expression = "  id > 0\n-- authored\r\n\t'quoted' \\\u{1b} ";
    loaded.table.row_policies.get_mut("p").unwrap().using = Some(expression.into());
    let output = pbps_load::fmt::render(&loaded.name, &loaded.table, &[], None);
    let back = pbps_load::load_table_str(Path::new("policy.yml"), &output).unwrap();
    assert_eq!(loaded, back);
    assert!(back.table.row_policies["p"].with_check.is_none());
    assert_eq!(
        back.table.row_policies["named"].using,
        back.table.row_policies["named"].with_check
    );
    assert_eq!(
        output,
        pbps_load::fmt::render(&back.name, &back.table, &[], None)
    );

    let plain = load("").unwrap();
    let explicit =
        load("row_security: {enabled: false, forced: false}\nrow_policies: {}\n").unwrap();
    assert_eq!(plain, explicit);
    let ordinary = pbps_load::fmt::render(&plain.name, &plain.table, &[], None);
    assert!(!ordinary.contains("row_security"));
    assert!(!ordinary.contains("row_policies"));
    assert_eq!(
        ordinary,
        pbps_load::fmt::render(&explicit.name, &explicit.table, &[], None)
    );
}
