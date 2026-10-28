use super::*;
use crate::engine_effects::facts;
use pbps_model::Column;
use pbps_model::operation::symbolic::*;
use serde_json::json;

mod support;
use support::{change, fixture, rows};

#[test]
fn the_policy_table_covers_statement_kinds_on_pg16_and_pg18() {
    let column = serde_json::to_value(Column::new("integer".parse().unwrap())).unwrap();
    let sequence =
        serde_json::to_value(ValueSource::Sequence(pbps_model::OwnedSequence::serial())).unwrap();
    let none = serde_json::to_value(ValueSource::None).unwrap();
    let type_fields =
        json!({"from":"integer","to":"bigint","from_nullable":true,"to_nullable":true});
    let all = "Presence/Create/MembersAtOperation/Overwrite Name/Create/MembersAtOperation/Overwrite Type/Create/MembersAtOperation/Overwrite Collation/Create/MembersAtOperation/Overwrite Default/Create/MembersAtOperation/Overwrite NotNull/Create/MembersAtOperation/Overwrite";
    let mut serial = Column::new("integer".parse().unwrap());
    serial.value_source = ValueSource::Sequence(pbps_model::OwnedSequence::serial());
    let mut generated = Column::new("integer".parse().unwrap());
    generated.value_source = ValueSource::Generated(pbps_model::Generated {
        expression: "d*2".into(),
        stored: true,
    });
    let cases = [
        (change("add_column",json!({"table":"app.p","name":"d","column":column})), all.to_owned(), None, false),
        (change("drop_column",json!({})), all.replace("Create", "Retire"), None, false),
        (change("rename_column",json!({"table":"app.p","from":"d","to":"e"})), "Name/Retain/MembersAtOperation/PreserveOwnership".into(), None, false),
        (change("alter_column_type",type_fields.clone()), "Type/Retain/MembersAtOperation/PreserveOwnership".into(), None, false),
        (change("alter_column_type",json!({"from":"integer","to":"integer","from_nullable":true,"to_nullable":true,"to_collation":"pg_catalog.C"})), "Collation/Retain/MembersAtOperation/PreserveOwnership".into(), None, false),
        (change("alter_column_nullability",json!({"ty":"integer","to_nullable":false})), "NotNull/Retain/MembersAtOperation/Overwrite".into(), Some("NotNull/Retain/MembersAtOperation/StrengthenNotNull"), false),
        (change("alter_column_nullability",json!({"ty":"integer","to_nullable":true})), "NotNull/Retain/MembersAtOperation/Overwrite".into(), Some("NotNull/Retain/MembersAtOperation/PreserveOwn"), false),
        (change("alter_column_default",json!({"from":"1","to":"7"})), "Default/Retain/MembersAtOperation/Overwrite".into(), None, false),
        (change("alter_column_default",json!({"from":"1","to":null})), "Default/Retain/MembersAtOperation/Overwrite".into(), None, false),
        (change("alter_column_numbering",json!({"ty":"integer","from":none,"to":sequence})), "Default/Retain/MembersAtOperation/Overwrite Numbering/Create/ParentOnly/PreserveOwnership".into(), None, false),
        (change("alter_column_numbering",json!({"ty":"integer","from":sequence,"to":none})), "Default/Retain/MembersAtOperation/Overwrite Numbering/Retire/ParentOnly/PreserveOwnership".into(), None, false),
        (change("alter_column_numbering",json!({"ty":"integer","from":sequence,"to":sequence})), "Numbering/Retain/ParentOnly/PreserveOwnership".into(), None, false),
        (change("rename_table",json!({"uid":"t_aaaaaa","from":"app.p","to":"other.q"})), "Address/Retain/ParentOnly/PreserveOwnership".into(), None, false),
        (change("add_column",json!({"table":"app.p","name":"d","column":serial})), format!("{all} Numbering/Create/ParentOnly/Overwrite"), None, false),
        (change("add_column",json!({"table":"app.p","name":"d","column":generated})), all.into(), None, false),
        (change("drop_column",json!({})), format!("{} Numbering/Retire/ParentOnly/Overwrite", all.replace("Create", "Retire")), None, true),
        (change("alter_column_type",json!({"from":"integer","to":"bigint","from_nullable":true,"to_nullable":false})), "Type/Retain/MembersAtOperation/PreserveOwnership NotNull/Retain/MembersAtOperation/Overwrite".into(), Some("Type/Retain/MembersAtOperation/PreserveOwnership NotNull/Retain/MembersAtOperation/StrengthenNotNull"), false),
        (change("alter_column_type",json!({"from":"integer","to":"integer","from_nullable":true,"to_nullable":true})), "".into(), None, false),
    ];
    for (change, pg16, pg18, sequence_input) in cases {
        assert_eq!(
            change.parent_column_uid().map(|u| u.as_str()),
            (!matches!(change, Change::RenameTable { .. })).then_some("c_aaaaaa")
        );
        for (version, expected) in [(160000, pg16.as_str()), (180000, pg18.unwrap_or(&pg16))] {
            let actual = fixture(&change, Some(version), |f| {
                if sequence_input
                    || matches!(
                        change,
                        Change::AlterColumnNumbering {
                            from: ValueSource::Sequence(_),
                            ..
                        }
                    )
                {
                    for ((_, p), c) in &mut f.base.cells {
                        if matches!(p, Property::Column(_, Attribute::Numbering)) {
                            c.value = facts::definition("owned sequence");
                        }
                    }
                }
            });
            assert_eq!(rows(&actual), expected, "{version}: {change:?}");
        }
    }
}
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
