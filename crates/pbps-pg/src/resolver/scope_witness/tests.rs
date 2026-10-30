use super::*;

fn observation(lower: u64, upper: u64, xmin: u32) -> Witness {
    Witness {
        bounds: Bounds { lower, upper },
        rows: BTreeMap::from([(
            ("pg_namespace".into(), "[\"app\"]".into()),
            Entry {
                token: Token { oid: 41, xmin },
                label: "target catalog row pg_namespace app".into(),
            },
        )]),
        role_versions: false,
        scope: vec!["app".into()],
        principal: ("deploy".into(), "deploy".into()),
    }
}

#[test]
fn an_unchanged_row_survives_unrelated_transaction_progress() {
    let before = observation(1000, 1003, 900);
    let after = observation(1040, 1050, 900);
    assert!(before.differences(&after).unwrap().is_empty());
    assert!(
        before
            .role_history_limitation()
            .unwrap()
            .1
            .contains("checked by value")
    );
}

#[test]
fn changed_versions_and_recreated_identities_are_named_independently() {
    let before = observation(1000, 1000, 900);
    let mut after = observation(1100, 1100, 1001);
    assert_eq!(
        before.differences(&after).unwrap(),
        ["target catalog row pg_namespace app"]
    );
    after.rows.values_mut().next().unwrap().token.xmin = 900;
    after.rows.values_mut().next().unwrap().token.oid = 42;
    assert_eq!(
        before.differences(&after).unwrap(),
        ["target catalog row pg_namespace app"]
    );
}

#[test]
fn a_readable_empty_membership_set_differs_from_a_new_or_removed_member() {
    let before = observation(1000, 1000, 900);
    let mut after = observation(1100, 1100, 900);
    after.rows.insert(
        ("pg_auth_members".into(), "[\"31\",\"32\",\"33\"]".into()),
        Entry {
            token: Token {
                oid: 55,
                xmin: 1001,
            },
            label: "target catalog row pg_auth_members membership".into(),
        },
    );
    assert_eq!(
        before.differences(&after).unwrap(),
        ["target catalog row pg_auth_members membership"]
    );
    assert_eq!(
        after.differences(&observation(1100, 1100, 900)).unwrap(),
        ["target catalog row pg_auth_members membership"]
    );
}

#[test]
fn history_coverage_and_scope_cannot_be_silently_replaced() {
    let before = observation(1000, 1000, 900);
    let mut after = observation(1100, 1100, 900);
    after.role_versions = true;
    assert!(after.role_history_limitation().is_none());
    assert_eq!(
        before.differences(&after).unwrap(),
        ["the target role-history coverage"]
    );
    after.scope.push("elsewhere".into());
    assert_eq!(
        before.differences(&after).unwrap(),
        ["the target catalog history scope or principal"]
    );
}

#[test]
fn a_32_bit_rollover_is_qualified_against_the_full_active_horizon() {
    let bounds = Bounds {
        lower: EPOCH + 20,
        upper: EPOCH + 25,
    };
    assert_eq!(bounds.origin(u32::MAX - 10).unwrap(), Some(EPOCH - 11));
    let before = observation(EPOCH + 20, EPOCH + 25, u32::MAX - 10);
    let after = observation(EPOCH + 30, EPOCH + 50, u32::MAX - 10);
    assert!(before.differences(&after).unwrap().is_empty());
}

#[test]
fn a_possible_active_or_future_alias_is_unknown_instead_of_visible() {
    let bounds = Bounds {
        lower: 1000,
        upper: 1040,
    };
    for raw in [0, 1000, 1001, 1039, 1040, u32::MAX] {
        assert!(bounds.origin(raw).is_err());
    }
    assert_eq!(bounds.origin(999).unwrap(), Some(999));
    assert_eq!(bounds.origin(1).unwrap(), None);
    assert_eq!(bounds.origin(2).unwrap(), None);
}

#[test]
fn half_range_age_and_reversal_end_the_history_guarantee() {
    let boundary = Bounds {
        lower: HALF + 100,
        upper: HALF + 100,
    };
    assert!(boundary.origin(100).is_err());
    let before = observation(HALF + 90, HALF + 90, 100);
    assert!(
        before
            .differences(&observation(HALF + 99, HALF + 99, 100))
            .is_ok()
    );
    assert!(
        before
            .differences(&observation(HALF + 100, HALF + 100, 100))
            .is_err()
    );
    let before = observation(1000, 1050, 900);
    for (lower, upper) in [(999, 1050), (1000, 1049), (1000 + HALF, 1050 + HALF)] {
        assert!(before.differences(&observation(lower, upper, 900)).is_err());
    }
}

#[test]
fn malformed_numbers_and_snapshot_bounds_never_become_empty_observations() {
    for text in ["", "-1", "+1", "01", " 1", "18446744073709551616"] {
        assert!(number(text).is_err());
    }
    assert_eq!(number("0").unwrap(), 0);
    assert_eq!(number("18446744073709551615").unwrap(), u64::MAX);
    for (lower, upper) in [(0, 1), (2, 3), (1000, 999), (1000, 1000 + HALF)] {
        assert!(Bounds { lower, upper }.check().is_err());
    }
}

#[test]
fn quoted_names_cannot_expand_another_catalog_template_marker() {
    let schemas = authorization::setting_literal("@ROLES@'schema");
    let roles = authorization::setting_literal("@SCHEMAS@'role");
    let sql = catalog_sql(&schemas, &roles);
    assert!(sql.contains(&format!("nspname IN ({schemas})")));
    assert!(sql.contains(&format!("rolname IN ({roles})")));
    assert!(!sql.contains("nspname IN ('@SCHEMAS@"));
}
