use super::*;
use pbps_model::operation::{BoundInput, Incarnation, ObjectKey};

fn binding(f: &Facts) -> BoundInput {
    BoundInput {
        input: ObjectKey::Table(f.parents.first().unwrap().clone()),
        incarnation: Incarnation::Base,
    }
}
#[test]
fn a_type_reparse_keeps_its_stored_bindings_and_never_borrows_a_default_event() {
    let c = change(
        "alter_column_type",
        json!({"from":"integer","to":"bigint","from_nullable":true,"to_nullable":true}),
    );
    let clauses = fixture(&c, Some(180000), |f| {
        let b = binding(f);
        for ((_, p), cell) in &mut f.base.cells {
            if matches!(p, Property::Column(_, Attribute::Type)) {
                cell.value = Fact::Known(Value {
                    datum: Datum::Definition("stored type".into()),
                    bindings: [b.clone()].into(),
                    ownership: Ownership::Own,
                });
            }
        }
    });
    assert!(
        matches!(&clauses[..], [Clause::Recurse(w)] if w.to.known().unwrap().bindings.len()==1)
    );
    let unread = fixture(&c, Some(180000), |f| {
        for ((_, p), cell) in &mut f.base.cells {
            if matches!(p, Property::Column(_, Attribute::Type)) {
                cell.value = Fact::Unread;
            }
        }
    });
    assert!(matches!(&unread[..], [Clause::Recurse(w)] if w.to==Fact::Unread));
}
#[test]
fn only_the_matching_declared_default_can_supply_a_goal_binding() {
    let c = change("alter_column_default", json!({"from":"1","to":"app.f(1)"}));
    let supply = |f: &mut Facts| {
        let b = binding(f);
        for ((_, p), cell) in &mut f.goal.cells {
            if matches!(p, Property::Column(_, Attribute::Default)) {
                cell.value = Fact::Known(Value {
                    datum: Datum::Definition("f(1)".into()),
                    bindings: [b.clone()].into(),
                    ownership: Ownership::Own,
                });
            }
        }
    };
    let clauses = fixture(&c, Some(180000), supply);
    assert!(
        matches!(&clauses[..], [Clause::Recurse(w)] if w.to.known().unwrap().bindings.len()==1)
    );
    for expression in [Some("app.f(2)"), None] {
        let release = change(
            "alter_column_default",
            json!({"from":"app.f(1)","to":expression}),
        );
        let clauses = fixture(&release, Some(180000), supply);
        let Clause::Recurse(w) = &clauses[0] else {
            panic!()
        };
        if expression.is_some() {
            assert_eq!(w.to, Fact::Unread);
        } else {
            assert_eq!(w.to, facts::definition("no column default"));
        }
    }
}
#[test]
fn numbering_creation_uses_the_statement_address_after_a_parent_rename() {
    let s = pbps_model::OwnedSequence::serial();
    let c = change(
        "alter_column_numbering",
        json!({"column":"other.q.d","ty":"integer","from":ValueSource::None,"to":ValueSource::Sequence(s)}),
    );
    let clauses = fixture(&c, Some(180000), |_| {});
    assert!(
        matches!(&clauses[..], [Clause::Recurse(d),Clause::Recurse(n)] if matches!(&d.to.known().unwrap().datum, Datum::Definition(text) if text.contains("q_d_seq") && !text.contains("p_d_seq")) && n.lifetime==Lifetime::Create)
    );
}

#[test]
fn added_generated_expressions_keep_unread_bindings_distinct_from_known_empty() {
    for version in [160000, 180000] {
        for (expression, unread) in [
            ("app.f(1)", true),
            ("f (1)", true),
            ("\"f\"(1)", true),
            ("'1'::regclass", true),
            ("1 + 2", false),
        ] {
            let mut column = Column::new("integer".parse().unwrap());
            column.value_source = ValueSource::Generated(pbps_model::Generated {
                expression: expression.into(),
                stored: true,
            });
            let change = change(
                "add_column",
                json!({"table":"app.p","name":"d","column":column}),
            );
            let clauses = fixture(&change, Some(version), |_| {});
            let output = |attribute| {
                clauses
                    .iter()
                    .find_map(|clause| {
                        if let Clause::Recurse(w) = clause
                            && matches!(&w.key.property, Property::Column(_, a) if *a == attribute)
                        {
                            Some(&w.to)
                        } else {
                            None
                        }
                    })
                    .unwrap()
            };
            let ty = output(Attribute::Type);
            if unread {
                assert_eq!(*ty, Fact::Unread, "{version}: {expression}");
            } else {
                let Fact::Known(value) = ty else {
                    panic!("a reference-free constant is known")
                };
                assert!(matches!(value.datum, Datum::Definition(_)));
                assert!(value.bindings.is_empty());
            }
            assert_eq!(
                *output(Attribute::Default),
                facts::definition("no column default")
            );
        }
    }
}
