//! `depends_on:` on every declaration root that has an address (DEC-2469.8).

use super::*;
use pbps_model::object_ref::{ObjectDeps, ObjectRef, Registry};

const FILES: [(&str, &str); 5] = [
    ("table", "table: app.t\n{dep}columns:\n  x: {type: int}\n"),
    ("view", "view: app.v\n{dep}definition: SELECT 1\n"),
    ("role", "role: reader\n{dep}"),
    ("enum", "enum: app.mood\n{dep}values: [a, b]\n"),
    ("domain", "domain: app.d\n{dep}type: int\n"),
];

fn with(kind: &str, dep: &str) -> String {
    let template = FILES.iter().find(|(k, _)| *k == kind).unwrap().1;
    template.replace("{dep}", dep)
}

fn load(kind: &str, dep: &str) -> Result<LoadedFile, Vec<LoadError>> {
    load_file_str(Path::new("f.yml"), &with(kind, dep))
}

fn reference(text: &str) -> ObjectRef {
    text.parse().unwrap()
}

#[test]
fn depends_on_takes_a_list_or_an_owner_map_on_every_declaration_root() {
    for (kind, _) in FILES {
        // The list form is an edge from the root, whatever the root is.
        let listed = load(kind, "depends_on: [app.f(), table::app.u]\n").unwrap();
        let deps = &listed.depends_on().deps;
        assert_eq!(deps.0.len(), 1, "{kind}");
        assert_eq!(
            deps.0[0].1,
            [reference("app.f()"), reference("table::app.u")].into(),
            "{kind}"
        );
        // The map form names the owner, which may be the root itself.
        let mapped = load(kind, "depends_on:\n  \"role::reader\": [app.f()]\n").unwrap();
        assert_eq!(
            mapped.depends_on().deps.0,
            [(reference("role::reader"), [reference("app.f()")].into())],
            "{kind}"
        );
        // Negative: no key, an empty list and an empty map are no edges.
        for none in ["", "depends_on: []\n", "depends_on: {}\n"] {
            assert!(load(kind, none).unwrap().depends_on().deps.0.is_empty());
        }
        // Negative: nothing but a list of strings or a map of lists reads.
        for refused in [
            "depends_on: app.f()\n",
            "depends_on: 1\n",
            "depends_on: [[app.f()]]\n",
            "depends_on: {k: app.f()}\n",
            "depends_on: {k: {a: b}}\n",
            "depends_on: [{a: b}]\n",
        ] {
            assert!(load(kind, refused).is_err(), "{kind}: {refused}");
        }
    }
}

#[test]
fn a_root_with_no_address_refuses_depends_on() {
    for text in [
        "extension: citext\nschema: public\nversion: '1.4'\n",
        "text_search_dictionary: app.d1\ntemplate: pg_catalog.snowball\n",
        "dependency: other.dbo.t\nkind: table\ncolumns:\n  id: {type: int, nullable: false}\n",
        "sequence: app.order_no\ntype: integer\nstart: 100\nincrement: 5\n",
    ] {
        assert!(load_file_str(Path::new("f.yml"), text).is_ok(), "{text}");
        let refused = load_file_str(Path::new("f.yml"), &format!("{text}depends_on: []\n"));
        let message = format!("{:?}", refused.unwrap_err());
        assert!(message.contains("depends_on"), "{message}");
    }
}

fn project(files: &[(&str, String)]) -> Result<Loaded, Vec<LoadError>> {
    load_schema_files(
        &files
            .iter()
            .map(|(name, text)| (std::path::PathBuf::from(name), text.clone()))
            .collect::<Vec<_>>(),
    )
}

fn problems(errors: &[LoadError]) -> String {
    errors.iter().map(|e| format!("{e:?}")).collect()
}

#[test]
fn an_owner_key_must_be_declared_in_the_same_file() {
    let table = |name: &str, dep: &str| {
        format!("table: app.{name}\n{dep}columns:\n  x: {{type: int, default: '0'}}\n")
    };
    let key = |owner: &str| format!("depends_on:\n  \"{owner}\": [app.f()]\n");
    // Its own part, in its own file.
    let own = project(&[
        ("a.yml", table("a", &key("default::app.a.x"))),
        ("b.yml", table("b", "")),
    ])
    .unwrap();
    assert_eq!(own.hints.annotations.declared.0.len(), 1);
    // Negative: another file's part is refused, naming both files.
    let errors = project(&[
        ("a.yml", table("a", &key("default::app.b.x"))),
        ("b.yml", table("b", "")),
    ])
    .unwrap_err();
    let said = problems(&errors);
    assert!(
        said.contains("not declared in this file") && said.contains("b.yml"),
        "{said}"
    );
    // A partition's parts are written in its parent's file, so the parent's
    // file owns them; another file does not.
    let parent = "table: app.p\n{dep}columns:\n  x: {type: int, nullable: false}\n\
                  partition_by: [x]\npartitions:\n  c1: {from: ['1'], to: ['2']}\n";
    let owned = project(&[("p.yml", parent.replace("{dep}", &key("table::app.c1")))]).unwrap();
    assert_eq!(owned.hints.annotations.declared.0.len(), 1);
    let errors = project(&[
        ("p.yml", parent.replace("{dep}", "")),
        ("a.yml", table("a", &key("table::app.c1"))),
    ])
    .unwrap_err();
    assert!(problems(&errors).contains("p.yml"), "{}", problems(&errors));
    // Negative: an owner nobody declares is not the loader's to refuse; the
    // validation that names every unknown reference does.
    let nobody = project(&[("a.yml", table("a", &key("default::app.nope.x")))]).unwrap();
    assert_eq!(nobody.hints.annotations.declared.0.len(), 1);
    // Negative: the same part written twice, in two spellings, is refused.
    let twice = "depends_on:\n  \"table::app.a\": [app.f()]\n  \
                 'object::{\"root\":{\"table\":\"app.a\"},\"part\":\"whole\"}': [app.g()]\n";
    let errors = project(&[("a.yml", table("a", twice))]).unwrap_err();
    assert!(problems(&errors).contains("twice"), "{}", problems(&errors));
}

#[test]
fn a_typed_spelling_naming_a_declared_module_is_refused_as_ambiguous() {
    let module = "view: table::app.t\ndefinition: SELECT 1\n".to_owned();
    let table = "table: app.t\ncolumns:\n  x: {type: int}\n".to_owned();
    let owner =
        |target: &str| format!("view: app.v\ndepends_on: [{target}]\ndefinition: SELECT 1\n");
    let errors = project(&[
        ("m.yml", module.clone()),
        ("t.yml", table.clone()),
        ("v.yml", owner("table::app.t")),
    ])
    .unwrap_err();
    assert!(
        problems(&errors).contains("ambiguous"),
        "{}",
        problems(&errors)
    );
    // The `module::` escape and the structured form say which was meant.
    for target in [
        "module::table::app.t",
        "'object::{\"root\":{\"table\":\"app.t\"},\"part\":\"whole\"}'",
    ] {
        project(&[
            ("m.yml", module.clone()),
            ("t.yml", table.clone()),
            ("v.yml", owner(target)),
        ])
        .unwrap_or_else(|e| panic!("{target}: {}", problems(&e)));
    }
    // Negative: with no such module, the typed spelling is the table.
    project(&[("t.yml", table), ("v.yml", owner("table::app.t"))]).unwrap();
}

#[test]
fn a_depends_on_error_points_at_the_item() {
    let text = "view: app.v\ndepends_on:\n  - app.ok\n  - nonsense::app.t\ndefinition: SELECT 1\n";
    let errors = load_module_str(Path::new("v.yml"), text).unwrap_err();
    let [LoadError::Semantic(error)] = errors.as_slice() else {
        panic!("{errors:?}");
    };
    assert_eq!(error.line, 4, "{error:?}");
    assert!(error.message.contains("unknown reference prefix"));
    // Negative: the same item in a map value is located the same way.
    let text = "view: app.v\ndepends_on:\n  \"index::module::app.v.i\":\n    - nonsense::app.t\ndefinition: SELECT 1\n";
    let errors = load_module_str(Path::new("v.yml"), text).unwrap_err();
    assert_eq!(errors[0].line(), Some(4), "{errors:?}");
}

/// `pbps fmt` of one file against a project's registry: every declaration kind
/// that has an address, spelled the way the project's own reader reads back.
fn format_file(registry: &Registry<'_>, file: &LoadedFile) -> String {
    match file {
        LoadedFile::Table(t) => fmt::render_partitioned(
            &t.name,
            &t.table,
            &[],
            &[],
            t.strategy.as_ref(),
            &SpelledDeps::new(registry, &t.depends_on.deps),
        ),
        LoadedFile::Module(m) => render_module(
            &m.id,
            &m.module,
            &SpelledDeps::new(registry, &m.depends_on.deps),
            m.public_execute,
        ),
        LoadedFile::Role(r) => render_role(
            &r.name,
            &r.role,
            &[],
            &SpelledDeps::new(registry, &r.depends_on.deps),
        ),
        LoadedFile::Type(t) => render_type_with_value_renames(
            &t.name,
            &t.definition,
            &[],
            &t.value_renames,
            &SpelledDeps::new(registry, &t.depends_on.deps),
        ),
        LoadedFile::Extension(_)
        | LoadedFile::Dependency(_)
        | LoadedFile::Search(_)
        | LoadedFile::Sequence(_) => {
            unreachable!("these roots take no depends_on")
        }
    }
}

#[test]
fn fmt_keeps_depends_on_on_every_root_kind_and_is_idempotent() {
    // One file alone: a project that declares nothing, so no spelling is ambiguous.
    let empty = pbps_model::Schema::default();
    let registry = Registry::new(&empty);
    let render = |file: &LoadedFile| format_file(&registry, file);
    let lists = "depends_on: [table::app.u, app.z(), app.f()]\n";
    let mapped = "depends_on:\n  \"role::reader\": [app.f()]\n  \"table::app.t\": [app.g()]\n";
    for (kind, _) in FILES {
        for dep in [lists, mapped, ""] {
            let first = load(kind, dep).unwrap();
            let written = render(&first);
            let second = load_file_str(Path::new("f.yml"), &written).unwrap();
            assert_eq!(first.depends_on(), second.depends_on(), "{kind}\n{written}");
            assert_eq!(render(&second), written, "{kind}: not idempotent");
            // Negative: a file with no `depends_on:` gains no key.
            assert_eq!(
                written.contains("depends_on"),
                dep.contains("depends_on"),
                "{kind}\n{written}"
            );
        }
    }
    // The list is in the order of the reference type, whatever it was written
    // in: the same order the saved plan uses.
    let written = render(&load("table", lists).unwrap());
    assert!(
        written.contains("depends_on: [\"table::app.u\", app.f(), app.z()]")
            || written.contains("depends_on: [table::app.u, app.f(), app.z()]"),
        "{written}"
    );
    // The structured escape reads as the reference it names and is written
    // in the shorthand when the shorthand says the same.
    let escaped = load(
        "table",
        "depends_on: ['object::{\"root\":{\"table\":\"app.u\"},\"part\":\"whole\"}']\n",
    )
    .unwrap();
    assert_eq!(
        escaped.depends_on().deps,
        ObjectDeps(vec![(
            reference("table::app.t"),
            [reference("table::app.u")].into()
        )])
    );
}

/// Every file of `files` rewritten by `fmt` against the project the files make,
/// as `(name, text)`. The rewritten project must load with the edges it had, and
/// rewriting it again must change nothing.
fn formatted(files: &[(&str, String)]) -> Vec<(String, String)> {
    let before = project(files).unwrap_or_else(|e| panic!("{}", problems(&e)));
    let registry = Registry::new(&before.schema);
    let once: Vec<(String, String)> = files
        .iter()
        .map(|(name, text)| {
            let file = load_file_str(Path::new(name), text).unwrap();
            ((*name).to_owned(), format_file(&registry, &file))
        })
        .collect();
    let named: Vec<_> = once.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
    let after = project(&named)
        .unwrap_or_else(|e| panic!("the rewritten project does not load: {}", problems(&e)));
    assert_eq!(
        after.hints.annotations.declared, before.hints.annotations.declared,
        "{once:?}"
    );
    let registry = Registry::new(&after.schema);
    for (name, text) in &once {
        let file = load_file_str(Path::new(name), text).unwrap();
        assert_eq!(
            &format_file(&registry, &file),
            text,
            "{name}: not idempotent"
        );
    }
    once
}

/// A shorthand is only spelled where the project reads it back as the same
/// reference. `table::app.t` is the table until a view `t` in a schema
/// `table::app` exists, and then the loader refuses it (DEC-2469.8). `fmt` once
/// rewrote the structured escape to the shorthand, as a target and as the
/// table's own owner key, and left a project that did not load.
#[test]
fn fmt_spells_a_reference_the_way_the_project_reads_it_back() {
    let escape = |text: &str| {
        format!(
            "object::{}",
            serde_json::to_string(&reference(text)).unwrap()
        )
    };
    let files = |with_view: bool| {
        // The table's file names two owners, itself and a column, so its own
        // owner key is written in the map form, where it is spelled too.
        let mut files = vec![
            (
                "t.yml",
                format!(
                    "table: app.t\ndepends_on:\n  '{}': [app.f()]\n  'column::app.t.x': [app.f()]\n\
                     columns:\n  x: {{type: int}}\n",
                    escape("table::app.t"),
                ),
            ),
            (
                "v.yml",
                format!(
                    "view: app.v\ndepends_on: ['{}']\ndefinition: SELECT 1\n",
                    escape("table::app.t")
                ),
            ),
        ];
        if with_view {
            files.push(("m.yml", "view: table::app.t\ndefinition: SELECT 1\n".into()));
        }
        files
    };
    // The shorthand reads as the view here, so each spelling stays the escape:
    // the owner key and the target of the table's file, and the view's target.
    let ambiguous = formatted(&files(true));
    for name in ["t.yml", "v.yml"] {
        let text = &ambiguous.iter().find(|(n, _)| n == name).unwrap().1;
        assert_eq!(text.matches("object::").count(), 1, "{name}\n{text}");
        assert!(!text.contains("table::app.t"), "{name}\n{text}");
    }
    // Negative: with no such view, the same annotations are written as the
    // shorthand, which is shorter and says the same.
    for (name, text) in formatted(&files(false)) {
        if name != "m.yml" {
            assert!(!text.contains("object::"), "{name}\n{text}");
            assert!(text.contains("table::app.t"), "{name}\n{text}");
        }
    }
}
