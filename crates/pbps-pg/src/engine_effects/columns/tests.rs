use super::*;
use crate::engine_effects::facts;
use pbps_model::Column;
use pbps_model::operation::symbolic::*;
use serde_json::json;

use crate::engine_effects::production_tests::policy_support::{change, fixture};

#[test]
fn missing_facts_versions_and_identity_capabilities_are_obligations() {
    let default = change("alter_column_default", json!({"from":"1","to":"7"}));
    assert!(
        matches!(&fixture(&default,Some(180000),|f| f.base.cells.clear())[..], [Clause::Require(Fact::Unsupported{reason,..})] if reason.contains("input") && reason.contains("separate plans"))
    );
    assert!(fixture(&default, Some(180000), |f| f.parents.clear()).is_empty());
    let nn = change(
        "alter_column_nullability",
        json!({"ty":"integer","to_nullable":false}),
    );
    for version in [None, Some(150000)] {
        assert_eq!(
            fixture(&nn, version, |_| {}),
            [Clause::Require(Fact::Unread)]
        );
    }
    let identity = ValueSource::Identity(pbps_model::Identity {
        seed: 1,
        increment: 1,
        options: Default::default(),
    });
    let mut column = Column::new("integer".parse().unwrap());
    column.value_source = identity.clone();
    let add = change(
        "add_column",
        json!({"table":"app.p","name":"d","column":column}),
    );
    for version in [160000, 180000] {
        assert!(
            matches!(&fixture(&add,Some(version),|_|{})[..], [Clause::Require(Fact::Unsupported{sqlstate,reason})] if sqlstate.as_deref()==(version<180000).then_some("42P16") && reason.contains("identity"))
        );
    }
    for (from, to, reason) in [
        (identity.clone(), ValueSource::None, "identity"),
        (ValueSource::None, identity, "identity"),
        (ValueSource::None, ValueSource::None, "no owned sequence"),
    ] {
        let c = change(
            "alter_column_numbering",
            json!({"ty":"integer","from":from,"to":to}),
        );
        assert!(
            matches!(&fixture(&c,Some(180000),|_|{})[..], [Clause::Require(Fact::Unsupported{reason:r,..})] if r.contains(reason) && r.contains("separate plans"))
        );
    }
}
#[test]
fn an_unread_expression_binding_does_not_turn_into_an_empty_binding_set() {
    let c = change("alter_column_default", json!({"from":"1","to":"app.f(1)"}));
    assert!(
        matches!(&fixture(&c,Some(180000),|_|{})[..], [Clause::Recurse(w)] if w.to == Fact::Unread)
    );
}
#[test]
fn property_lifetimes_and_binding_reads_do_not_depend_on_an_endpoint_child_union() {
    let c = change("drop_column", json!({}));
    let empty = fixture(&c, Some(180000), |_| {});
    let with_members = fixture(&c, Some(180000), |f| {
        let p = f.parents.first().unwrap().clone();
        f.base.members.insert(
            p.clone(),
            Membership {
                parent: p,
                incarnation: Version::Base,
                aliases: Default::default(),
            },
        );
    });
    assert_eq!(empty, with_members);
    let unread = fixture(&c, Some(180000), |f| {
        for ((_, p), v) in &mut f.base.cells {
            if matches!(p, Property::Column(_, Attribute::Numbering)) {
                v.value = Fact::Unread;
            }
        }
    });
    assert!(unread.contains(&Clause::Require(Fact::Unread)));
}

mod normalization;
