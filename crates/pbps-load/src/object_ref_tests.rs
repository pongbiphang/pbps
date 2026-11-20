#[cfg(test)]
use super::*;
use pbps_model::object_ref::{ObjectDeps, ObjectRef, Root};

fn module_with(depends_on: &str) -> Result<LoadedModule, Vec<LoadError>> {
    let text = format!("view: app.v\ndepends_on: [{depends_on}]\ndefinition: SELECT 1\n");
    load_module_str(Path::new("v.yaml"), &text)
}

#[test]
fn legacy_module_targets_keep_their_loader_formatter_and_json_bytes() {
    for name in ["dbo.base", "app.f(integer)", "app.t.tr", "表.檢視"] {
        let owner = module_with(&fmt::scalar(name)).unwrap();
        let references = owner.depends_on.deps.clone();
        let original = references.modules();
        assert_eq!(ObjectDeps::from_legacy(&original), references);
        assert_eq!(
            serde_json::to_vec(&references.modules()).unwrap(),
            serde_json::to_vec(&original).unwrap()
        );
        let rendered = render_module(
            &owner.id,
            &owner.module,
            &SpelledDeps::alone(&owner.depends_on.deps),
            owner.public_execute,
        );
        let again = load_module_str(Path::new("v.yaml"), &rendered).unwrap();
        assert_eq!(owner, again);
        assert_eq!(
            render_module(
                &again.id,
                &again.module,
                &SpelledDeps::alone(&again.depends_on.deps),
                again.public_execute
            ),
            rendered
        );
        // The bare name is what the file keeps, byte for byte.
        let spelled = ObjectRef::legacy(name).unwrap().legacy_spelling().unwrap();
        assert!(
            rendered.contains(&format!("depends_on: [{}]", fmt::scalar(&spelled))),
            "{rendered}"
        );
    }
}

#[test]
fn a_prefix_names_the_kind_and_a_spelling_with_an_unknown_prefix_is_refused() {
    // `table::app.base` used to be a module in the schema `table::app`; it is
    // now the table, and the module is reached with the `module::` escape.
    let table = module_with("table::app.base").unwrap();
    let expected: ObjectRef = "table::app.base".parse().unwrap();
    assert!(matches!(expected.root, Root::Table(_)));
    assert!(table.depends_on.deps.0[0].1.contains(&expected));
    // A schema whose own name holds `::` is spelled with `module::`, written
    // back that way, and read back as the same module.
    let escaped = module_with("module::x::y.base").unwrap();
    let rendered = render_module(
        &escaped.id,
        &escaped.module,
        &SpelledDeps::alone(&escaped.depends_on.deps),
        false,
    );
    assert!(
        rendered.contains("depends_on: [\"module::x::y.base\"]")
            || rendered.contains("depends_on: [module::x::y.base]"),
        "{rendered}"
    );
    assert_eq!(
        load_module_str(Path::new("v.yaml"), &rendered).unwrap(),
        escaped
    );
    // Negative: the old spelling is refused, never read as something else, and
    // so is a prefix nobody defined.
    for refused in ["x::y.base", "extension::app.postgis", "tabel::app.t"] {
        let errors = module_with(refused).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("unknown reference prefix")),
            "{refused}: {errors:?}"
        );
    }
}
