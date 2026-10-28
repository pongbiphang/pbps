//! Declared-type documentation and compatibility with type-free projects.
use pbps_docs::{Format, render};
use pbps_model::{
    Column, IdsFile, ManagedType, Module, ModuleKind, Permission, PrimaryKey, Role, Schema, Table,
    TableName, Tombstone,
};

fn without_types() -> (Schema, IdsFile) {
    let mut table = Table {
        description: Some("Customer records.".into()),
        primary_key: Some(PrimaryKey {
            name: Some("pk_customer".into()),
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        }),
        ..Default::default()
    };
    table.columns.insert(
        "id".into(),
        Column::new("bigint".parse().unwrap()).not_null(),
    );
    let mut retired = Column::new("text".parse().unwrap());
    retired.description = Some("Legacy contact information.".into());
    retired.deprecated = Some("Use the contact service.".into());
    table.columns.insert("retired".into(), retired);
    let mut schema = Schema::default();
    schema
        .tables
        .insert(TableName::new("app", "customer"), table);
    schema.modules.insert(
        "app.active_customer".parse().unwrap(),
        Module {
            kind: ModuleKind::View,
            description: Some("Active customers.".into()),
            definition: "SELECT id FROM app.customer".into(),
            options: Default::default(),
        },
    );
    schema.roles.insert(
        "reporter".into(),
        Role {
            description: Some("Reporting access.".into()),
            grants: [("app.customer".parse().unwrap(), [Permission::Select].into())].into(),
        },
    );
    let mut ids = IdsFile::default();
    ids.tombstones.insert(
        "c_k7x2mq".parse().unwrap(),
        Tombstone {
            require: None,
            was: "app.customer.legacy".into(),
            dropped_at: "2026-01-05".into(),
            reason: "No longer collected.".into(),
            operator: "operator".into(),
        },
    );
    (schema, ids)
}

fn with_types() -> Schema {
    let mut schema = Schema::default();
    schema.types.insert(
        "app.mood".parse().unwrap(),
        ManagedType::Enum {
            values: vec!["zebra".into(), "alpha".into(), "middle".into()],
        },
    );
    schema.types.insert(
        "app.label".parse().unwrap(),
        ManagedType::Domain {
            base: "text".parse().unwrap(),
            not_null: true,
            default: Some("'seed'".into()),
            collation: Some("pg_catalog.\"C\"".parse().unwrap()),
            checks: [
                ("nonempty".into(), "length(VALUE) > 0".into()),
                ("prefix".into(), "VALUE LIKE 'pre%'".into()),
            ]
            .into(),
        },
    );
    schema.types.insert(
        "zeta.optional".parse().unwrap(),
        ManagedType::Domain {
            base: "integer".parse().unwrap(),
            not_null: false,
            default: None,
            collation: None,
            checks: Default::default(),
        },
    );
    schema
}

fn with_metacharacters() -> Schema {
    let mut schema = Schema::default();
    schema.types.insert(
        pbps_model::TypeRef::new("s|<&", "e|<&").unwrap(),
        ManagedType::Enum {
            values: vec!["label|<&".into(), "tick`label".into(), "last".into()],
        },
    );
    schema.types.insert(
        pbps_model::TypeRef::new("s|<&", "d|<&").unwrap(),
        ManagedType::Domain {
            base: pbps_model::ColumnType::reference(
                pbps_model::TypeRef::new("s|<&", "base|<&").unwrap(),
            ),
            not_null: true,
            default: Some("'default|<&'".into()),
            collation: Some("app.\"collation|<&\"".parse().unwrap()),
            checks: [("check|<&".into(), "VALUE <> 'check|<&'".into())].into(),
        },
    );
    schema
}

#[test]
fn markdown_types_keep_declared_order_and_every_domain_attribute() {
    let schema = with_types();
    let out = render(&schema, &IdsFile::default(), Format::Markdown, "Schema");
    for expected in [
        "## Types\n",
        "### `app`\n",
        "#### `app.label`\n",
        "#### `app.mood`\n",
        "### `zeta`\n",
        "#### `zeta.optional`\n",
        "*Enum*",
        "*Domain*",
        "| Base type | `text` |",
        "| NOT NULL | yes |",
        "| Default | `'seed'` |",
        "| Collation | `pg_catalog.\"C\"` |",
        "| `nonempty` | `length(VALUE) > 0` |",
        "| `prefix` | `VALUE LIKE 'pre%'` |",
        "0 table(s), 0 column(s), 0 module(s).",
    ] {
        assert!(out.contains(expected), "missing {expected}: {out}");
    }
    assert_eq!(out.matches("### `app`\n").count(), 1);
    assert!(out.find("#### `app.label`").unwrap() < out.find("#### `app.mood`").unwrap());
    assert!(out.find("### `app`").unwrap() < out.find("### `zeta`").unwrap());
    let positions: Vec<_> = ["zebra", "alpha", "middle"]
        .iter()
        .map(|s| out.find(&format!("| `{s}` |")).unwrap())
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{out}");
    let optional = out.split("#### `zeta.optional`").nth(1).unwrap();
    for expected in ["| NOT NULL | no |", "| Default | — |", "| Collation | — |"] {
        assert!(optional.contains(expected), "{optional}");
    }
    assert!(!optional.contains("| Check |"));
    assert!(!out.contains("]("));
    assert_eq!(
        out,
        render(&schema, &IdsFile::default(), Format::Markdown, "Schema")
    );
}

#[test]
fn html_types_keep_declared_order_and_every_domain_attribute() {
    let schema = with_types();
    let out = render(&schema, &IdsFile::default(), Format::Html, "Schema");
    for expected in [
        "<h2>Types</h2>",
        "<h3><code>app</code></h3>",
        "<h4><code>app.label</code></h4>",
        "<h4><code>app.mood</code></h4>",
        "<h3><code>zeta</code></h3>",
        "<h4><code>zeta.optional</code></h4>",
        ">Enum</p>",
        ">Domain</p>",
        "<tr><td>Base type</td><td><code>text</code></td></tr>",
        "<tr><td>NOT NULL</td><td>yes</td></tr>",
        "<tr><td>Default</td><td><code>&#39;seed&#39;</code></td></tr>",
        "<tr><td>Collation</td><td><code>pg_catalog.&quot;C&quot;</code></td></tr>",
        "<tr><td><code>nonempty</code></td><td><code>length(VALUE) &gt; 0</code></td></tr>",
        "<tr><td><code>prefix</code></td><td><code>VALUE LIKE &#39;pre%&#39;</code></td></tr>",
        "0 table(s), 0 column(s), 0 module(s).",
    ] {
        assert!(out.contains(expected), "missing {expected}: {out}");
    }
    assert_eq!(out.matches("<h3><code>app</code></h3>").count(), 1);
    assert!(out.find("<h4><code>app.label").unwrap() < out.find("<h4><code>app.mood").unwrap());
    assert!(out.find("<h3><code>app").unwrap() < out.find("<h3><code>zeta").unwrap());
    let positions: Vec<_> = ["zebra", "alpha", "middle"]
        .iter()
        .map(|s| {
            out.find(&format!("<tr><td><code>{s}</code></td></tr>"))
                .unwrap()
        })
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{out}");
    let optional = out
        .split("<h4><code>zeta.optional</code></h4>")
        .nth(1)
        .unwrap();
    for expected in [
        "<tr><td>NOT NULL</td><td>no</td></tr>",
        "<tr><td>Default</td><td>&mdash;</td></tr>",
        "<tr><td>Collation</td><td>&mdash;</td></tr>",
    ] {
        assert!(optional.contains(expected), "{optional}");
    }
    assert!(!optional.contains("<th>Check</th>"));
    assert!(!out.contains("href="));
    assert_eq!(
        out,
        render(&schema, &IdsFile::default(), Format::Html, "Schema")
    );
}

#[test]
fn markdown_type_values_stay_inside_their_cells() {
    let out = render(
        &with_metacharacters(),
        &IdsFile::default(),
        Format::Markdown,
        "Schema",
    );
    for escaped in [
        "s\\|<&",
        "e\\|<&",
        "d\\|<&",
        "base\\|<&",
        "collation\\|<&",
        "`label\\|<&`",
        "``tick`label``",
        "`'default\\|<&'`",
        "`check\\|<&`",
        "`VALUE <> 'check\\|<&'`",
    ] {
        assert!(out.contains(escaped), "missing {escaped}: {out}");
    }
    for raw in ["label|<&", "'default|<&'", "VALUE <> 'check|<&'"] {
        assert!(!out.contains(raw), "{out}");
    }
    for line in out.lines().filter(|line| line.starts_with('|')) {
        let cells = line.replace("\\|", "").matches('|').count();
        assert!(matches!(cells, 2 | 3), "malformed row: {line}");
    }
}

#[test]
fn html_type_values_cannot_inject_markup() {
    let out = render(
        &with_metacharacters(),
        &IdsFile::default(),
        Format::Html,
        "Schema",
    );
    for escaped in [
        "s|&lt;&amp;",
        "e|&lt;&amp;",
        "d|&lt;&amp;",
        "base|&lt;&amp;",
        "collation|&lt;&amp;",
        "<code>label|&lt;&amp;</code>",
        "<code>&#39;default|&lt;&amp;&#39;</code>",
        "<code>check|&lt;&amp;</code>",
        "<code>VALUE &lt;&gt; &#39;check|&lt;&amp;&#39;</code>",
    ] {
        assert!(out.contains(escaped), "missing {escaped}: {out}");
    }
    for raw in ["label|<&", "'default|<&'", "VALUE <> 'check|<&'"] {
        assert!(!out.contains(raw), "{out}");
    }
}

#[test]
fn markdown_without_types_is_byte_identical_to_the_original_renderer() {
    let (schema, ids) = without_types();
    let out = render(&schema, &ids, Format::Markdown, "Legacy schema");
    assert!(!out.contains("## Types"));
    assert_eq!(out.as_bytes(), include_bytes!("fixtures/without-types.md"));
}

#[test]
fn html_without_types_is_byte_identical_to_the_original_renderer() {
    let (schema, ids) = without_types();
    let out = render(&schema, &ids, Format::Html, "Legacy schema");
    assert!(!out.contains("<h2>Types</h2>"));
    assert_eq!(
        out.as_bytes(),
        include_bytes!("fixtures/without-types.html")
    );
}
