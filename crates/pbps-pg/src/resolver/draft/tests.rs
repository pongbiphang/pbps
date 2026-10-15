use super::*;

/// A target built by hand: what [`read`] would hand [`compose`].
#[derive(Default)]
struct Target {
    facts: Facts,
    managed: BTreeSet<Key>,
}

fn key(catalog: &str, oid: u32) -> Key {
    (catalog.to_owned(), oid)
}

impl Target {
    fn object(&mut self, catalog: &str, oid: u32, schema: &str, described: &str) -> Key {
        let key = key(catalog, oid);
        self.facts.objects.insert(
            key.clone(),
            Object {
                schema: schema.to_owned(),
                described: described.to_owned(),
            },
        );
        key
    }

    fn depend(&mut self, dependent: &Key, referenced: &Key, deptype: char, shape: bool) {
        self.facts.depends.push(Depend {
            dependent: dependent.clone(),
            referenced: referenced.clone(),
            deptype,
            shape,
        });
    }

    /// A relation of `kind` with its row type, carried by it as the engine
    /// records it, and `columns` of the given spellings.
    fn relation(
        &mut self,
        oid: u32,
        kind: char,
        schema: &str,
        name: &str,
        columns: &[(&str, &str)],
    ) -> Key {
        let described = match kind {
            'v' => format!("view {schema}.{name}"),
            _ => format!("table {schema}.{name}"),
        };
        let relation = self.object("pg_class", oid, schema, &described);
        let row_type = self.object("pg_type", oid + 1, schema, &format!("type {schema}.{name}"));
        self.depend(&row_type, &relation, 'i', true);
        self.facts.relations.insert(
            oid,
            Relation {
                kind,
                schema: schema.to_owned(),
                name: name.to_owned(),
                qualified: format!("{schema}.{name}"),
                replica_identity: 'd',
                populated: true,
                ..Relation::default()
            },
        );
        self.facts.columns.insert(
            oid,
            columns
                .iter()
                .zip(1..)
                .map(|((name, spelled), number)| Column {
                    number,
                    name: (*name).to_owned(),
                    spelled: (*spelled).to_owned(),
                    ..Column::default()
                })
                .collect(),
        );
        relation
    }

    fn managed(&mut self, key: &Key) {
        self.managed.insert(key.clone());
    }

    fn draft(&self, schemas: &[&str]) -> Draft {
        let schemas: Vec<String> = schemas.iter().map(|s| (*s).to_owned()).collect();
        compose(&self.facts, &|key| self.managed.contains(key), &schemas)
    }
}

/// `ext.t (id integer PRIMARY KEY, name text)`, its view `ext.v` over it,
/// and the managed view `app.a` reading `ext.v`.
fn reading_a_view() -> (Target, Key, Key, Key) {
    let mut target = Target::default();
    let table = target.relation(
        20000,
        'r',
        "ext",
        "t",
        &[("id", "integer"), ("name", "text")],
    );
    target.facts.keys.insert(
        20000,
        vec![Keyed {
            name: "t_pkey".into(),
            definition: "PRIMARY KEY (id)".into(),
        }],
    );
    target.facts.columns.get_mut(&20000).unwrap()[0].not_null = true;
    let view = target.relation(20010, 'v', "ext", "v", &[("id", "integer")]);
    // The view's query reads the table: no shape.
    target.depend(&view, &table, 'n', false);
    let managed = target.relation(20020, 'v', "app", "a", &[("id", "integer")]);
    target.depend(&managed, &view, 'n', false);
    target.managed(&managed);
    (target, table, view, managed)
}

#[test]
fn a_view_the_managed_set_reads_is_drafted_as_a_shape_view_without_its_query() {
    let (target, _, _, _) = reading_a_view();
    let draft = target.draft(&[]);
    assert!(
        draft
            .sql
            .contains("CREATE VIEW ext.v AS SELECT NULL::integer AS id WHERE false;\n"),
        "{}",
        draft.sql
    );
    assert!(
        draft.sql.contains("CREATE SCHEMA IF NOT EXISTS ext;\n"),
        "{}",
        draft.sql
    );
    // Negative: the table only the view's query reads is not needed for the
    // shape, and the managed view itself is never drafted.
    assert!(!draft.sql.contains("ext.t"), "{}", draft.sql);
    assert!(!draft.sql.contains("app.a"), "{}", draft.sql);
    assert!(draft.listed.is_empty(), "{:?}", draft.listed);
}

#[test]
fn a_table_is_drafted_with_its_columns_in_order_its_not_nulls_and_its_keys() {
    let (mut target, table, _, managed) = reading_a_view();
    // The managed view reads the table directly too.
    target.depend(&managed, &table, 'n', false);
    let draft = target.draft(&[]);
    assert!(
        draft.sql.contains(
            "CREATE TABLE ext.t (\n    id integer NOT NULL,\n    name text\n);\n\
             ALTER TABLE ext.t ADD CONSTRAINT t_pkey PRIMARY KEY (id);\n"
        ),
        "{}",
        draft.sql
    );
}

#[test]
fn a_routine_is_listed_by_signature_and_never_written() {
    let (mut target, _, _, managed) = reading_a_view();
    let routine = target.object("pg_proc", 20030, "ext", "function ext.f(integer)");
    target.depend(&managed, &routine, 'n', false);
    let draft = target.draft(&[]);
    assert_eq!(draft.listed.len(), 1, "{:?}", draft.listed);
    assert!(
        draft.listed[0].starts_with("function ext.f(integer): listed, never written"),
        "{:?}",
        draft.listed
    );
    assert!(
        draft.sql.contains("-- function ext.f(integer): listed"),
        "{}",
        draft.sql
    );
    // Negative: no routine source, in any spelling.
    assert!(
        !draft.sql.to_uppercase().contains("CREATE FUNCTION"),
        "{}",
        draft.sql
    );
    assert!(
        !draft.sql.to_uppercase().contains("CREATE PROCEDURE"),
        "{}",
        draft.sql
    );
}

#[test]
fn a_shape_is_created_before_the_object_whose_shape_names_it() {
    let mut target = Target::default();
    let extension = target.object("pg_extension", 20100, "", "extension citext");
    target.facts.extensions.insert(
        20100,
        Extension {
            name: "citext".into(),
            schema: "public".into(),
            version: "'1.6'".into(),
        },
    );
    let citext = target.object("pg_type", 20101, "public", "type public.citext");
    target.depend(&citext, &extension, 'e', true);
    let labels = target.object("pg_type", 20110, "ext", "type ext.e");
    target.facts.types.insert(
        20110,
        Type {
            kind: 'e',
            qualified: "ext.e".into(),
            ..Type::default()
        },
    );
    target
        .facts
        .labels
        .insert(20110, vec!["'a'".into(), "'b'".into()]);
    let domain = target.object("pg_type", 20120, "ext", "type ext.d");
    target.facts.types.insert(
        20120,
        Type {
            kind: 'd',
            qualified: "ext.d".into(),
            base: "ext.e".into(),
            not_null: true,
            ..Type::default()
        },
    );
    target.depend(&domain, &labels, 'n', true);
    // Named so the description alone would put it first.
    let table = target.relation(
        20130,
        'r',
        "ext",
        "a",
        &[("d", "ext.d"), ("c", "public.citext")],
    );
    target.depend(&table, &domain, 'n', true);
    target.depend(&table, &citext, 'n', true);
    let managed = target.relation(20140, 'v', "app", "a", &[("d", "ext.d")]);
    target.depend(&managed, &table, 'n', false);
    target.managed(&managed);
    let draft = target.draft(&[]);
    let at = |needle: &str| {
        draft
            .sql
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} in {}", draft.sql))
    };
    assert!(
        at("CREATE EXTENSION citext WITH SCHEMA public VERSION '1.6';") < at("CREATE TYPE ext.e")
    );
    assert!(at("CREATE TYPE ext.e AS ENUM ('a', 'b');") < at("CREATE DOMAIN ext.d"));
    assert!(at("CREATE DOMAIN ext.d AS ext.e NOT NULL;") < at("CREATE TABLE ext.a"));
    // Negative: the extension's member is the extension's, never its own
    // statement.
    assert!(
        !draft.sql.contains("CREATE TYPE public.citext"),
        "{}",
        draft.sql
    );
}

#[test]
fn a_shape_that_names_a_managed_object_is_listed_as_a_chain_and_not_written() {
    let mut target = Target::default();
    let managed_table = target.relation(20200, 'r', "app", "m", &[("id", "integer")]);
    target.managed(&managed_table);
    // ext.t has a column of the managed table's row type.
    let external = target.relation(20210, 'r', "ext", "t", &[("m", "app.m")]);
    target.depend(&external, &key("pg_type", 20201), 'n', true);
    // ext.v has a column of ext.t's row type: a chain through it.
    let through = target.relation(20220, 'v', "ext", "v", &[("t", "ext.t")]);
    target.depend(&through, &key("pg_type", 20211), 'n', true);
    let reader = target.relation(20230, 'v', "app", "a", &[("t", "ext.t")]);
    target.managed(&reader);
    target.depend(&reader, &through, 'n', false);
    let draft = target.draft(&[]);
    assert!(
        draft.listed.iter().any(|line| line
            .starts_with("table ext.t cannot be staged: its shape names the managed table app.m")
            && line.contains("adopt table ext.t into the managed set too, or select no resolver")),
        "{:?}",
        draft.listed
    );
    assert!(
        draft
            .listed
            .iter()
            .any(|line| line == "view ext.v cannot be staged: its shape names table ext.t, which cannot be staged either"),
        "{:?}",
        draft.listed
    );
    // Negative: neither is written.
    assert!(!draft.sql.contains("CREATE TABLE ext.t"), "{}", draft.sql);
    assert!(!draft.sql.contains("CREATE VIEW ext.v"), "{}", draft.sql);
}

#[test]
fn what_guards_an_external_table_is_not_followed() {
    let (mut target, table, _, managed) = reading_a_view();
    target.depend(&managed, &table, 'n', false);
    // A trigger's function and a foreign key's table: no shape.
    let function = target.object("pg_proc", 20300, "ext", "function ext.audit()");
    let trigger = key("pg_trigger", 20301);
    target.depend(&trigger, &table, 'a', false);
    target.depend(&trigger, &function, 'n', false);
    let referenced = target.relation(20310, 'r', "ext", "p", &[("id", "integer")]);
    let foreign_key = key("pg_constraint", 20320);
    target.depend(&foreign_key, &table, 'a', false);
    target.depend(&foreign_key, &referenced, 'n', false);
    let draft = target.draft(&[]);
    assert!(draft.sql.contains("CREATE TABLE ext.t"), "{}", draft.sql);
    assert!(!draft.sql.contains("ext.audit"), "{}", draft.sql);
    assert!(!draft.sql.contains("ext.p"), "{}", draft.sql);
    assert!(draft.listed.is_empty(), "{:?}", draft.listed);
}

#[test]
fn a_named_schema_adds_its_unmanaged_objects_and_never_a_managed_one() {
    let (mut target, _, _, _) = reading_a_view();
    // Nothing deployed reads it yet.
    target.relation(20400, 'r', "ext", "later", &[("id", "integer")]);
    let managed = target.relation(20410, 'r', "ext", "mine", &[("id", "integer")]);
    target.managed(&managed);
    let draft = target.draft(&["ext"]);
    assert!(
        draft.sql.contains("CREATE TABLE ext.later"),
        "{}",
        draft.sql
    );
    assert!(draft.sql.contains("CREATE TABLE ext.t"), "{}", draft.sql);
    assert!(!draft.sql.contains("ext.mine"), "{}", draft.sql);
    // Negative: without it, only what the managed set reads.
    let without = target.draft(&[]);
    assert!(!without.sql.contains("ext.later"), "{}", without.sql);
}

#[test]
fn a_class_the_draft_does_not_write_is_listed_for_writing_by_hand() {
    let (mut target, _, _, managed) = reading_a_view();
    let cast = target.object("pg_cast", 20500, "", "cast from ext.e to integer");
    target.depend(&managed, &cast, 'n', false);
    let partitioned = target.relation(20510, 'p', "ext", "events", &[("at", "date")]);
    target.facts.relations.get_mut(&20510).unwrap().structure = "a partitioned table".into();
    target.depend(&managed, &partitioned, 'n', false);
    let draft = target.draft(&[]);
    assert!(
        draft.listed.contains(
            &"cast from ext.e to integer: write it here by hand if a binding needs it; the draft writes no cast"
                .to_owned()
        ),
        "{:?}",
        draft.listed
    );
    assert!(
        draft
            .listed
            .iter()
            .any(|line| line.starts_with("table ext.events: write it here by hand")),
        "{:?}",
        draft.listed
    );
    assert!(
        !draft.sql.contains("CREATE TABLE ext.events"),
        "{}",
        draft.sql
    );
}

#[test]
fn a_serial_and_an_identity_column_are_drafted_with_their_sequences() {
    let mut target = Target::default();
    let table = target.relation(
        20600,
        'r',
        "ext",
        "t",
        &[("id", "integer"), ("k", "bigint")],
    );
    target.facts.columns.get_mut(&20600).unwrap()[1].identity = Some('d');
    for (oid, name, column, how) in [
        (20610, "ext.t_id_seq", 1, 'a'),
        (20620, "ext.t_k_seq", 2, 'i'),
    ] {
        target.facts.sequences.insert(
            oid,
            Sequence {
                qualified: name.into(),
                spelled: "integer".into(),
                start: "1".into(),
                increment: "1".into(),
                min: "1".into(),
                max: "2147483647".into(),
                cache: "1".into(),
                owner: Some((20600, column, how)),
                ..Sequence::default()
            },
        );
        let sequence = target.object("pg_class", oid, "ext", &format!("sequence {name}"));
        target.depend(&sequence, &table, how, true);
    }
    let managed = target.relation(20630, 'v', "app", "a", &[("id", "integer")]);
    target.managed(&managed);
    target.depend(&managed, &table, 'n', false);
    let draft = target.draft(&[]);
    assert!(
        draft.sql.contains(
            "    k bigint GENERATED BY DEFAULT AS IDENTITY (SEQUENCE NAME ext.t_k_seq START WITH 1 \
             INCREMENT BY 1 MINVALUE 1 MAXVALUE 2147483647 CACHE 1 NO CYCLE)\n"
        ),
        "{}",
        draft.sql
    );
    assert!(
        draft.sql.contains(
            "CREATE SEQUENCE ext.t_id_seq AS integer START WITH 1 INCREMENT BY 1 MINVALUE 1 \
             MAXVALUE 2147483647 CACHE 1 NO CYCLE OWNED BY ext.t.id;\n"
        ),
        "{}",
        draft.sql
    );
    // Negative: each sequence once, inside its table's statements.
    assert_eq!(
        draft.sql.matches("ext.t_id_seq").count(),
        1,
        "{}",
        draft.sql
    );
}

#[test]
fn nothing_outside_the_managed_set_drafts_a_file_that_says_so() {
    let mut target = Target::default();
    let managed = target.relation(20700, 'r', "app", "m", &[("id", "integer")]);
    target.managed(&managed);
    let draft = target.draft(&[]);
    assert!(
        draft
            .sql
            .ends_with("-- The managed objects depend on no object outside the managed set.\n"),
        "{}",
        draft.sql
    );
    assert!(!draft.sql.contains("CREATE "), "{}", draft.sql);
}
