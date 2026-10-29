use super::*;
use pbps_model::object_ref::{ObjectDeps, ObjectRef};

#[test]
fn legacy_module_targets_keep_their_loader_formatter_and_json_bytes() {
    for name in [
        "dbo.base",
        "app.f(integer)",
        "app.t.tr",
        "table::app.base",
        "x::y.base",
        "表.檢視",
    ] {
        let text = format!(
            "view: app.v\ndepends_on: [{}]\ndefinition: SELECT 1\n",
            render_scalar(name)
        );
        let owner = load_module_str(Path::new("v.yaml"), &text).unwrap();
        let original = [(owner.id.clone(), owner.depends_on.clone())].into();
        let references = ObjectDeps::from_legacy(&original);
        assert_eq!(references.to_legacy().unwrap(), original);
        assert_eq!(
            serde_json::to_vec(&references.to_legacy().unwrap()).unwrap(),
            serde_json::to_vec(&original).unwrap()
        );
        let rendered = render_module(
            &owner.id,
            &owner.module,
            &owner.depends_on,
            owner.public_execute,
        );
        let again = load_module_str(Path::new("v.yaml"), &rendered).unwrap();
        assert_eq!(owner, again);
        assert_eq!(
            render_module(
                &again.id,
                &again.module,
                &again.depends_on,
                again.public_execute
            ),
            rendered
        );
        assert_eq!(
            ObjectRef::legacy(name).unwrap().legacy_spelling().unwrap(),
            owner.depends_on.first().unwrap().raw().to_string()
        );
    }
    for text in [
        "table: app.t\ncolumns: {x: {type: int}}\ndepends_on: [app.v]\n",
        "view: app.v\ndefinition: SELECT 1\ndepends_on: {app.v: [app.f]}\n",
    ] {
        assert!(load_file_str(Path::new("future.yaml"), text).is_err());
    }
    let future = load_module_str(
        Path::new("future.yaml"),
        "view: app.v\ndefinition: SELECT 1\ndepends_on: [table::app.t]\n",
    )
    .unwrap();
    let mut schema = pbps_model::Schema::default();
    schema.modules.insert(future.id.clone(), future.module);
    schema
        .tables
        .insert("app.t".parse().unwrap(), Default::default());
    assert!(
        !pbps_model::module::check_dependencies(&schema, &[(future.id, future.depends_on)].into())
            .is_empty()
    );
}
