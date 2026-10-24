use super::*;

#[cfg(test)]
pub(super) fn successes() -> Vec<Case> {
    let index = json!({"columns":[{"name":"k"}]});
    let keys = json!({"tables":{"app.p":{
        "primary_key":{"columns":["k"],"storage_parameters":{"fillfactor":"80"}},
        "unique":{"pair":{"columns":["k","bucket"]}},
        "indexes":{
            "plain":index, "unique":{"columns":[{"name":"k"}],"unique":true},
            "expression":{"columns":[{"expression":"(bucket + 1)"}],"include":["payload"],"filter":"k >= 0","storage_parameters":{"fillfactor":"70"}},
            "gin":{"columns":[{"name":"doc","opclass":"jsonb_path_ops"}],"method":"gin","storage_parameters":{"fastupdate":"off"}}
        }},"app.p1":{"indexes":{"matching":index}}}});
    let rows = json!([["app.p",[{"k":1,"bucket":2,"payload":"1","doc":{"a":1}},{"k":3,"bucket":4,"payload":"2","doc":{}}]]]);
    let mut cases = vec![
        Case::member(
            "owned-and-unnamed-key",
            keys.clone(),
            json!({}),
            rows.clone(),
        ),
        Case::member(
            "named-key",
            keys.clone(),
            json!({"tables":{"app.p":{"primary_key":{"name":"quoted\"key"}}}}),
            rows.clone(),
        ),
        Case::member(
            "key-switch-and-columns",
            json!({}),
            json!({"tables":{"app.p":{"partition_by":{"columns":["bucket"]},"columns":{"payload":null,"added":{"type":"text","nullable":true}}}}}),
            rows.clone(),
        ),
        Case::member(
            "members",
            json!({}),
            json!({"tables":{"app.p1":null,"app.p2":{"columns":{},"partition_of":{"parent":"app.p","bound":{"range":{"from":[{"value":"0"}],"to":[{"value":"20"}]}}}},"app.p3":{"columns":{},"partition_of":{"parent":"app.p","bound":{"range":{"from":[{"value":"20"}],"to":[{"value":"30"}]}}}}}}),
            rows.clone(),
        ),
        Case::member("empty-leaves", keys.clone(), json!({}), json!([])),
        Case::member(
            "empty-parent",
            json!({"tables":{"app.p1":null,"app.p":keys["tables"]["app.p"]}}),
            json!({}),
            json!([]),
        ),
        Case::member(
            "default",
            json!({"tables":{"app.default":{"columns":{},"partition_of":{"parent":"app.p","bound":"default"}}}}),
            json!({}),
            json!([["app.p",[{"k":1,"bucket":2},{"k":21,"bucket":22},{"k":21,"bucket":22}]]]),
        ),
        Case::member(
            "names-and-locality",
            json!({"tables":{"app.p1":null,"葉.很長的分割表名稱很長的分割表名稱分割表":{"columns":{},"partition_of":{"parent":"app.p","bound":{"range":{"from":[{"value":"0"}],"to":[{"value":"10"}]}}}}}}),
            json!({"tables":{"app.p":{"indexes":{"quoted\"index":index}}}}),
            rows.clone(),
        ),
        Case::member(
            "valid-cast-and-check",
            json!({}),
            json!({"tables":{"app.p":{"columns":{"payload":{"type":"integer"}},"checks":{"positive":{"expression":"k >= 0"}}}}}),
            rows.clone(),
        ),
    ];
    cases.push(Case { name:"external-tree", tree:true, before:json!({}), after:json!({"tables":{"app.parent":{"columns":{"id":{"type":"bigint"}}}}}), rows:json!([["app.lookup",[{"id":1}]],["app.parent",[{"id":1,"code":1,"memo":"x"}]],["app.incoming",[{"id":1,"parent_id":1}]]]), cap:1 });
    cases
}

pub(super) fn failures() -> Vec<FailureCase> {
    let member = |name, after, rows| Case::member(name, json!({}), after, json!([["app.p", rows]]));
    let two = json!([{"k":1,"bucket":1},{"k":2,"bucket":1}]);
    vec![
        (
            member(
                "bad-cast",
                json!({"tables":{"app.p":{"columns":{"payload":{"type":"integer"}}}}}),
                json!([{"k":1,"bucket":1,"payload":"bad"}]),
            ),
            Selector::Copy,
            0,
            Fault::Natural,
            "22P02",
        ),
        (
            member(
                "bounds",
                json!({"tables":{"app.p1":{"partition_of":{"bound":{"range":{"to":[{"value":"2"}]}}}}}}),
                json!([{"k":3,"bucket":3}]),
            ),
            Selector::Copy,
            0,
            Fault::Natural,
            "23514",
        ),
        (
            Case {
                cap: 1,
                ..member("cap", json!({}), two.clone())
            },
            Selector::Copy,
            0,
            Fault::Natural,
            "P0001",
        ),
        (
            member(
                "duplicate",
                json!({"tables":{"app.p":{"primary_key":{"name":"pk","columns":["k"]}}}}),
                json!([{"k":1,"bucket":1},{"k":1,"bucket":1}]),
            ),
            Selector::Index,
            1,
            Fault::Natural,
            "23505",
        ),
        (
            member(
                "check",
                json!({"tables":{"app.p":{"checks":{"positive":{"expression":"k < 2"}}}}}),
                two,
            ),
            Selector::Check,
            0,
            Fault::Natural,
            "23514",
        ),
        (
            successes().remove(0),
            Selector::Index,
            1,
            Fault::Interrupt,
            "22012",
        ),
        (
            successes().remove(0),
            Selector::Swap,
            0,
            Fault::Interrupt,
            "22012",
        ),
        (
            successes().pop().unwrap(),
            Selector::Restore,
            0,
            Fault::Denied,
            "42501",
        ),
    ]
}

// The existing TREE's declared view dependency diamond.
pub(super) fn module_edges() -> ModuleDeps {
    [(
        "app.diamond".parse().unwrap(),
        ["app.left_view", "app.right_view"]
            .into_iter()
            .map(|n| n.parse().unwrap())
            .collect(),
    )]
    .into()
}
