use super::*;
use pbps_dialect::{ConversionRequired, ValueProbe};
use pbps_model::{Change, EnumRebuildStep, ValueChange};

fn column(from: &str, to: &str, using: Option<&str>) -> Change {
    Change::AlterColumnType {
        uid: "c_aaaaaa".parse().unwrap(),
        column: pbps_model::TableName::new("app", "t").column("v"),
        from: from.parse().unwrap(),
        to: to.parse().unwrap(),
        from_nullable: true,
        to_nullable: true,
        from_collation: None,
        to_collation: None,
        through_external_partitions: false,
        using: using.map(str::to_owned),
    }
}

#[test]
fn columns_and_rebuilds_share_admission_risk_and_probe_material() {
    let pg = Postgres::new();
    for (from, to, using, refused) in [
        (
            "timestamp",
            "timestamptz",
            None,
            Some(ConversionRequired::SessionTimeZone),
        ),
        (
            "timestamptz",
            "timestamp",
            None,
            Some(ConversionRequired::SessionTimeZone),
        ),
        (
            "varchar(20)[]",
            "varchar(10)[]",
            None,
            Some(ConversionRequired::ArrayElements),
        ),
        (
            "text",
            "integer",
            None,
            Some(ConversionRequired::Incompatible),
        ),
        ("xml", "varchar(10)", None, None),
        ("text", "varchar(10)", Some("v"), None),
        ("text[]", "integer[]", Some("v::integer[]"), None),
        ("smallint", "integer", None, None),
    ] {
        let column = column(from, to, using);
        // Build the rebuild's input independently of the column accessor.
        let source: ColumnType = from.parse().unwrap();
        let target: ColumnType = to.parse().unwrap();
        let values = ValueChange::Cast {
            from: source.element(),
            to: target.element(),
            arrays: usize::from(target.is_array()),
            using: using.map(str::to_owned),
        };
        let rebuild = Change::EnumRebuild {
            uid: "y_000001".parse().unwrap(),
            name: "app.d".parse().unwrap(),
            tmp: "app.tmp".parse().unwrap(),
            step: EnumRebuildStep::Retype {
                column_uid: "c_aaaaaa".parse().unwrap(),
                column: pbps_model::TableName::new("app", "t").column("v"),
                to: "app.d".parse().unwrap(),
                array: false,
                values: Box::new(values),
            },
        };
        assert_eq!(column.value_change(), rebuild.value_change());
        let verdict = pg.value_change(&column.value_change().unwrap(), "\"v\"");
        assert_eq!(
            verdict,
            pg.value_change(&rebuild.value_change().unwrap(), "\"v\"")
        );
        assert_eq!(verdict.needs_using, refused, "{from} -> {to}");
        assert_eq!(
            verdict.risk.risk_class().is_some(),
            pg.change_risks(&column)
                .contains(&pbps_model::RiskClass::Narrowing)
        );
        assert_eq!(
            verdict.risk.risk_class().is_some(),
            pg.change_risks(&rebuild)
                .contains(&pbps_model::RiskClass::Narrowing)
        );
        if from == "xml" {
            assert!(
                matches!(verdict.probe, ValueProbe::Predicate { fails, .. } if fails == "length(rtrim((\"v\")::text, ' ')) > 10")
            );
        }
    }
}

#[test]
fn safe_scalar_changes_and_relabels_have_no_cast_probe() {
    let pg = Postgres::new();
    let verdict = pg.value_change(
        &column("smallint", "integer", None).value_change().unwrap(),
        "v",
    );
    assert_eq!(verdict.needs_using, None);
    assert_eq!(verdict.risk, TypeChangeRisk::Safe);
    assert_eq!(verdict.probe, ValueProbe::None);
    for removed in [vec![], vec!["gone".into()]] {
        let verdict = pg.value_change(&ValueChange::Relabel { removed }, "v");
        assert_eq!(verdict.needs_using, None);
        assert_eq!(verdict.risk, TypeChangeRisk::Safe);
        assert_eq!(verdict.probe, ValueProbe::None);
    }
    // Leaf types cannot describe different array shapes. Keep those full
    // types with the established column conversion path.
    for (from, to) in [("integer[]", "integer"), ("integer", "integer[]")] {
        assert!(column(from, to, None).value_change().is_none());
        assert!(pg.type_change_needs_using(&from.parse().unwrap(), &to.parse().unwrap()));
    }
}
