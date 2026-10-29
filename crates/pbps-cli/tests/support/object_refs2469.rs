use super::{Demo, ONE_COLUMN, code, stderr};

#[test]
fn since_validates_dependencies_in_unchanged_declarations() {
    let d = Demo::new("object-refs2469-since");
    d.table(ONE_COLUMN);
    d.module(
        "dbo.v.yml",
        "view: dbo.v\ndefinition: SELECT id FROM dbo.t\n",
    );
    let out = d.run(&["plan"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    d.commit();
    d.module(
        "dbo.v.yml",
        "view: dbo.v\ndepends_on: [dbo.missing]\ndefinition: SELECT id FROM dbo.t\n",
    );
    d.commit();
    d.table(&format!(
        "{ONE_COLUMN}\ndescription: changed independently\n"
    ));
    let out = d.run(&["validate", "--since", "HEAD"]);
    assert_ne!(code(&out), 0);
    assert!(
        stderr(&out).contains("not a declared module"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("dbo.missing"), "{}", stderr(&out));
    d.module(
        "dbo.v.yml",
        "view: dbo.v\ndefinition: SELECT id FROM dbo.t\n",
    );
    let out = d.run(&["validate", "--since", "HEAD"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}
