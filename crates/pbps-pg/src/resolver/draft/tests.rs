use super::*;
use crate::resolver::capture::Managed;

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
    for (oid, name, column, how, spelled) in [
        (20610, "ext.t_id_seq", 1, 'a', "integer"),
        (20620, "ext.t_k_seq", 2, 'i', "bigint"),
    ] {
        target.facts.sequences.insert(
            oid,
            Sequence {
                qualified: name.into(),
                spelled: spelled.into(),
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

#[test]
fn a_shape_that_names_a_foreign_table_is_listed_and_not_written() {
    let mut target = Target::default();
    let foreign = target.relation(20800, 'f', "ext", "remote", &[("id", "integer")]);
    // ext.v has a column of the foreign table's row type.
    let view = target.relation(20810, 'v', "ext", "v", &[("r", "ext.remote")]);
    target.depend(&view, &key("pg_type", 20801), 'n', true);
    let managed = target.relation(20820, 'v', "app", "a", &[("r", "ext.remote")]);
    target.managed(&managed);
    target.depend(&managed, &view, 'n', false);
    target.depend(&managed, &foreign, 'n', false);
    let draft = target.draft(&[]);
    assert!(
        draft.listed.contains(
            &"table ext.remote cannot be staged: the comparison does not qualify a foreign table's properties"
                .to_owned()
        ),
        "{:?}",
        draft.listed
    );
    assert!(
        draft.listed.contains(
            &"view ext.v cannot be staged: its shape names table ext.remote, which cannot be staged either"
                .to_owned()
        ),
        "{:?}",
        draft.listed
    );
    // Negative: the view is not written over a type no file can stage.
    assert!(!draft.sql.contains("CREATE VIEW ext.v"), "{}", draft.sql);
}

#[test]
fn a_table_with_an_invalid_unique_index_is_listed_for_writing_by_hand() {
    let (mut target, table, _, managed) = reading_a_view();
    target.depend(&managed, &table, 'n', false);
    let index = |valid, tablespace| Index {
        name: "t_name".into(),
        definition: "CREATE UNIQUE INDEX t_name ON ext.t USING btree (name)".into(),
        keyed: false,
        clustered: false,
        replica_identity: false,
        valid,
        tablespace,
    };
    target
        .facts
        .indexes
        .insert(20000, vec![index(false, false)]);
    let draft = target.draft(&[]);
    assert!(
        draft.listed.contains(
            &"table ext.t: write it here by hand if a binding needs it; the draft writes no invalid unique index"
                .to_owned()
        ),
        "{:?}",
        draft.listed
    );
    assert!(!draft.sql.contains("CREATE TABLE ext.t"), "{}", draft.sql);
    // One in a tablespace of its own, which scratch need not have.
    target.facts.indexes.insert(20000, vec![index(true, true)]);
    let draft = target.draft(&[]);
    assert!(
        draft.listed.contains(
            &"table ext.t: write it here by hand if a binding needs it; the draft writes no tablespace"
                .to_owned()
        ),
        "{:?}",
        draft.listed
    );
    // Negative: a valid one in the default tablespace is written.
    target.facts.indexes.insert(20000, vec![index(true, false)]);
    let draft = target.draft(&[]);
    assert!(
        draft
            .sql
            .contains("CREATE UNIQUE INDEX t_name ON ext.t USING btree (name);\n"),
        "{}",
        draft.sql
    );
}

impl Target {
    fn fill(&self) -> Fill {
        survey_fill(&self.facts, &|key| self.managed.contains(key))
    }
}

fn file_created(objects: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    objects
        .iter()
        .map(|(catalog, described)| ((*catalog).to_owned(), (*described).to_owned()))
        .collect()
}

#[test]
fn fill_stages_what_the_externals_file_lacks_and_leaves_what_it_holds() {
    let (target, _, _, _) = reading_a_view();
    let fill = target.fill();
    let staging = fill.staging(&BTreeSet::new());
    assert_eq!(
        staging.ahead,
        "CREATE SCHEMA IF NOT EXISTS ext;\n\
         CREATE VIEW ext.v AS SELECT NULL::integer AS id WHERE false;\n"
    );
    assert_eq!(staging.chained, "");
    assert_eq!(staging.filled, BTreeSet::from(["view ext.v".to_owned()]));
    assert!(staging.unfillable.is_empty(), "{:?}", staging.unfillable);
    // Negative: what the externals file created is the file's, never filled
    // over it.
    let held = fill.staging(&file_created(&[("pg_class", "view ext.v")]));
    assert_eq!(held, Staging::default());
}

/// `ext.c (m app.m)`, a table whose column is the managed table `app.m`'s
/// row type, read by the managed view `app.a`.
fn naming_a_managed_row_type() -> Target {
    let mut target = Target::default();
    let managed = target.relation(20200, 'r', "app", "m", &[("id", "integer")]);
    target.managed(&managed);
    let chained = target.relation(20210, 'r', "ext", "c", &[("m", "app.m")]);
    target.depend(&chained, &key("pg_type", 20201), 'n', true);
    let reader = target.relation(20220, 'v', "app", "a", &[("m", "app.m")]);
    target.depend(&reader, &chained, 'n', false);
    target.managed(&reader);
    target
}

#[test]
fn a_shape_naming_a_managed_row_type_is_filled_once_the_managed_tables_exist() {
    let target = naming_a_managed_row_type();
    let fill = target.fill();
    let staging = fill.staging(&BTreeSet::new());
    assert_eq!(staging.ahead, "CREATE SCHEMA IF NOT EXISTS ext;\n");
    assert!(
        staging
            .chained
            .starts_with("CREATE TABLE ext.c (\n    m app.m"),
        "{}",
        staging.chained
    );
    assert_eq!(staging.filled, BTreeSet::from(["table ext.c".to_owned()]));
    // The draft still lists it: a file written before the managed objects
    // cannot stage it.
    assert!(
        target
            .draft(&[])
            .listed
            .iter()
            .any(|line| line.contains("ext.c cannot be staged")),
        "{:?}",
        target.draft(&[]).listed
    );
    // Negative: the externals file's own copy is not filled over, so nothing
    // is created between the compile's two parts.
    let held = fill.staging(&file_created(&[("pg_class", "table ext.c")]));
    assert_eq!(held.chained, "");
    assert!(held.filled.is_empty(), "{:?}", held.filled);
}

#[test]
fn what_fill_cannot_stage_is_named_with_what_names_it() {
    let mut target = Target::default();
    let range = target.object("pg_type", 20300, "ext", "type ext.r");
    target.facts.types.insert(
        20300,
        Type {
            kind: 'r',
            qualified: "ext.r".into(),
            ..Type::default()
        },
    );
    let table = target.relation(20310, 'r', "ext", "t", &[("span", "ext.r")]);
    target.depend(&table, &range, 'n', true);
    let routine = target.object("pg_proc", 20320, "ext", "function ext.f(integer)");
    let reader = target.relation(20330, 'v', "app", "a", &[("span", "ext.r")]);
    target.depend(&reader, &table, 'n', false);
    target.depend(&reader, &routine, 'n', false);
    target.managed(&reader);
    let fill = target.fill();
    let staging = fill.staging(&BTreeSet::new());
    assert_eq!(staging.ahead, "");
    assert!(staging.filled.is_empty(), "{:?}", staging.filled);
    let unfillable = staging.unfillable.join("\n");
    for expected in [
        "function ext.f(integer) cannot be filled: pbps sends no routine source to scratch; \
         write it in the externals file",
        "type ext.r cannot be filled: pbps writes no range type",
        "table ext.t cannot be filled: its shape names type ext.r, which cannot be filled either",
    ] {
        assert!(unfillable.contains(expected), "{unfillable}");
    }
    // Negative: once the externals file holds the range type, the table
    // that names it is filled.
    let held = fill.staging(&file_created(&[("pg_type", "type ext.r")]));
    assert!(
        held.ahead.contains("CREATE TABLE ext.t (\n    span ext.r"),
        "{}",
        held.ahead
    );
    assert_eq!(held.filled, BTreeSet::from(["table ext.t".to_owned()]));
    assert_eq!(held.unfillable.len(), 1, "{:?}", held.unfillable);
}

#[test]
fn an_object_whose_part_the_externals_file_created_is_the_files() {
    let (mut target, table, _, _) = reading_a_view();
    let sequence = target.object("pg_class", 20005, "ext", "sequence ext.t_id_seq");
    target.depend(&sequence, &table, 'a', true);
    let reader = target.relation(20030, 'v', "app", "s", &[("n", "bigint")]);
    target.depend(&reader, &sequence, 'n', false);
    target.managed(&reader);
    let fill = target.fill();
    assert!(
        fill.staging(&BTreeSet::new())
            .filled
            .contains("table ext.t"),
        "the sequence folds into its table"
    );
    let held = fill.staging(&file_created(&[("pg_class", "sequence ext.t_id_seq")]));
    assert!(
        !held.filled.contains("table ext.t") && !held.ahead.contains("ext.t "),
        "{}",
        held.ahead
    );
    assert!(
        held.unfillable.iter().any(|line| line.contains(
            "table ext.t is not filled: the externals file created its sequence ext.t_id_seq"
        )),
        "{:?}",
        held.unfillable
    );
}

/// Negative of the part rule: the table is the file's in part only, so a
/// shape naming its row type is not filled, rather than sent to fail on a
/// type nobody created.
#[test]
fn what_names_a_partly_staged_object_is_not_filled() {
    let (mut target, table, _, _) = reading_a_view();
    let sequence = target.object("pg_class", 20005, "ext", "sequence ext.t_id_seq");
    target.depend(&sequence, &table, 'a', true);
    let reader = target.relation(20030, 'v', "app", "s", &[("n", "bigint")]);
    target.depend(&reader, &sequence, 'n', false);
    target.managed(&reader);
    let shaped = target.relation(20040, 'r', "ext", "c", &[("t", "ext.t")]);
    target.depend(&shaped, &key("pg_type", 20001), 'n', true);
    let other = target.relation(20050, 'v', "app", "o", &[("t", "ext.t")]);
    target.depend(&other, &shaped, 'n', false);
    target.managed(&other);
    let fill = target.fill();
    assert!(
        fill.staging(&BTreeSet::new())
            .filled
            .contains("table ext.c"),
        "filled when the file holds no part of ext.t"
    );
    let held = fill.staging(&file_created(&[("pg_class", "sequence ext.t_id_seq")]));
    assert!(
        !held.filled.contains("table ext.c") && !held.ahead.contains("ext.c "),
        "{}",
        held.ahead
    );
    assert!(
        held.unfillable.iter().any(|line| line
            == "table ext.c cannot be filled: its shape names table ext.t, which cannot be \
                filled either"),
        "{:?}",
        held.unfillable
    );
}

/// An extension's members are its parts: a file that created one of them
/// holds the extension, and fill sends no `CREATE EXTENSION` whose script
/// would create that member again.
#[test]
fn an_extension_whose_member_the_externals_file_created_is_the_files() {
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
    let reader = target.relation(20130, 'v', "app", "a", &[("c", "public.citext")]);
    target.depend(&reader, &citext, 'n', true);
    target.managed(&reader);
    let fill = target.fill();
    assert!(
        fill.staging(&BTreeSet::new())
            .ahead
            .contains("CREATE EXTENSION"),
        "filled when the file holds no member"
    );
    let held = fill.staging(&file_created(&[("pg_type", "type public.citext")]));
    assert!(!held.ahead.contains("CREATE EXTENSION"), "{}", held.ahead);
    assert!(
        held.unfillable.iter().any(|line| line.contains(
            "extension citext is not filled: the externals file created its type public.citext"
        )),
        "{:?}",
        held.unfillable
    );
}

#[test]
fn a_chained_shape_waits_for_every_managed_relation_it_names() {
    let mut target = Target::default();
    let first = target.relation(20300, 'r', "app", "m", &[("id", "integer")]);
    let second = target.relation(20310, 'r', "app", "n", &[("id", "integer")]);
    target.managed(&first);
    target.managed(&second);
    let chained = target.relation(20320, 'r', "ext", "c", &[("m", "app.m"), ("n", "app.n")]);
    target.depend(&chained, &key("pg_type", 20301), 'n', true);
    target.depend(&chained, &key("pg_type", 20311), 'n', true);
    let reader = target.relation(20330, 'v', "app", "a", &[("m", "app.m")]);
    target.depend(&reader, &chained, 'n', false);
    target.managed(&reader);
    let staging = target.fill().staging(&BTreeSet::new());
    assert_eq!(staging.filled, BTreeSet::from(["table ext.c".to_owned()]));
    assert_eq!(
        staging.chained_after,
        BTreeSet::from([
            ("app".to_owned(), "m".to_owned()),
            ("app".to_owned(), "n".to_owned())
        ]),
        "both, not the first the chain kept"
    );
}

/// A chained shape waiting for a managed relation the compile does not
/// create goes, with what names it; one waiting only for relations it
/// creates stays.
#[test]
fn only_the_chains_waiting_for_an_absent_relation_are_dropped() {
    let mut target = Target::default();
    let kept = target.relation(20400, 'r', "app", "m", &[("id", "integer")]);
    let renamed = target.relation(20410, 'r', "app", "n", &[("id", "integer")]);
    target.managed(&kept);
    target.managed(&renamed);
    let on_kept = target.relation(20420, 'r', "ext", "c", &[("m", "app.m")]);
    target.depend(&on_kept, &key("pg_type", 20401), 'n', true);
    let on_renamed = target.relation(20430, 'r', "ext", "d", &[("n", "app.n")]);
    target.depend(&on_renamed, &key("pg_type", 20411), 'n', true);
    let naming = target.relation(20440, 'r', "ext", "x", &[("d", "ext.d")]);
    target.depend(&naming, &key("pg_type", 20431), 'n', true);
    for (oid, name, read) in [(20450, "a", &on_kept), (20460, "b", &naming)] {
        let reader = target.relation(oid, 'v', "app", name, &[("id", "integer")]);
        target.depend(&reader, read, 'n', false);
        target.managed(&reader);
    }
    let mut staging = target.fill().staging(&BTreeSet::new());
    for table in ["ext.c", "ext.d", "ext.x"] {
        assert!(
            staging.chained.contains(&format!("CREATE TABLE {table} ")),
            "{table}"
        );
    }
    let absent = ("app".to_owned(), "n".to_owned());
    staging.keep_chained(|after| !after.contains(&absent));
    assert!(
        staging.chained.contains("CREATE TABLE ext.c "),
        "{}",
        staging.chained
    );
    for table in ["ext.d", "ext.x"] {
        assert!(
            !staging.chained.contains(&format!("CREATE TABLE {table} ")),
            "{table}: {}",
            staging.chained
        );
        assert!(
            !staging.filled.contains(&format!("table {table}")),
            "{table}"
        );
        assert!(
            staging
                .unfillable
                .iter()
                .any(|line| line.starts_with(&format!(
                    "table {table} is not filled: its shape names a managed relation"
                ))),
            "{table}: {:?}",
            staging.unfillable
        );
    }
    assert!(staging.filled.contains("table ext.c"));
    assert_eq!(
        staging.chained_after,
        BTreeSet::from([("app".to_owned(), "m".to_owned())])
    );
}

/// An extension whose member names a managed row type would wait for the
/// managed tables, and its script would then run between two parts of the
/// compile: it is never filled there, and is named. Negative: the same
/// extension naming nothing managed is filled ahead.
#[test]
fn an_extension_never_waits_for_the_managed_tables() {
    let staging = |naming: bool| {
        let mut target = Target::default();
        let managed = target.relation(20600, 'r', "app", "m", &[("id", "integer")]);
        target.managed(&managed);
        let extension = target.object("pg_extension", 20610, "", "extension inhouse");
        target.facts.extensions.insert(
            20610,
            Extension {
                name: "inhouse".into(),
                schema: "ext".into(),
                version: "'1.0'".into(),
            },
        );
        let member = target.object("pg_type", 20620, "ext", "type ext.x");
        target.facts.types.insert(
            20620,
            Type {
                kind: 'e',
                qualified: "ext.x".into(),
                ..Type::default()
            },
        );
        target.depend(&member, &extension, 'e', true);
        if naming {
            target.depend(&member, &key("pg_type", 20601), 'n', true);
        }
        let reader = target.relation(20630, 'v', "app", "a", &[("x", "ext.x")]);
        target.depend(&reader, &member, 'n', true);
        target.managed(&reader);
        target.fill().staging(&BTreeSet::new())
    };
    let ahead = staging(false);
    assert!(
        ahead.ahead.contains("CREATE EXTENSION inhouse"),
        "{}",
        ahead.ahead
    );
    let named = staging(true);
    assert!(
        !named.chained.contains("CREATE EXTENSION"),
        "{}",
        named.chained
    );
    assert!(!named.ahead.contains("CREATE EXTENSION"), "{}", named.ahead);
    assert!(!named.filled.contains("extension inhouse"));
    assert!(
        named.unfillable.iter().any(|line| line.starts_with(
            "extension inhouse cannot be filled: its shape names a managed relation, and an \
             extension's script runs code"
        )),
        "{:?}",
        named.unfillable
    );
}

#[test]
fn catalog_prose_staging_keeps_raw_identity_keys_and_shows_each_refusal_operand() {
    let raw = "a\n\u{1b}\u{202e}\\n\\u{1b}é*/";
    let shown = r"a\n\u{1b}\u{202e}\\n\\u{1b}é*/";
    let object = |catalog: &str, how| Filled {
        catalog: catalog.into(),
        described: raw.into(),
        parts: vec![],
        schemas: vec![],
        how,
        names: vec![],
        after: BTreeSet::new(),
    };
    let fill = Fill {
        objects: vec![object("pg_class", How::Ahead("SELECT 1;".into()))],
    };
    assert_eq!(
        fill.staging(&file_created(&[("pg_class", raw)])),
        Staging::default()
    );
    let wrong_key = fill.staging(&file_created(&[("pg_class", shown)]));
    assert_eq!(wrong_key.filled, BTreeSet::from([raw.to_owned()]));
    assert_eq!(wrong_key.ahead, "SELECT 1;");
    let mut part = object("pg_class", How::Ahead("SELECT 1;".into()));
    part.parts
        .push(("pg_class".into(), format!("sequence {raw}")));
    let fill = Fill {
        objects: vec![part],
    };
    let held = fill.staging(&file_created(&[("pg_class", &format!("sequence {raw}"))]));
    assert_eq!(
        held.unfillable,
        [format!(
            "{shown} is not filled: the externals file created its sequence {shown}; write the whole of it in the externals file if a binding needs it"
        )]
    );
    assert!(held.ahead.is_empty());
    let missing = object("pg_class", How::Not("no SQL".into()));
    let mut dependent = object("pg_class", How::Ahead("SELECT 2;".into()));
    dependent.described = format!("dependent {raw}");
    dependent.names.push(0);
    let messages = Fill {
        objects: vec![missing, dependent],
    }
    .staging(&BTreeSet::new())
    .unfillable;
    assert_eq!(
        messages,
        [
            format!(
                "{shown} cannot be filled: no SQL; write it in the externals file, or select no resolver and keep ADR-0013's conservative rebuild"
            ),
            format!(
                "dependent {shown} cannot be filled: its shape names {shown}, which cannot be filled either"
            ),
        ]
    );
    let extension = Fill {
        objects: vec![object("pg_extension", How::Chained("SELECT 3;".into()))],
    }
    .staging(&BTreeSet::new());
    assert_eq!(
        extension.unfillable,
        [format!(
            "{shown} cannot be filled: its shape names a managed relation, and an extension's script runs code and creates names fill cannot order between two parts of the compile; select no resolver and keep ADR-0013's conservative rebuild"
        )]
    );
    let fill = Fill {
        objects: vec![object("pg_class", How::Chained("SELECT 4;".into()))],
    };
    let mut staging = fill.staging(&BTreeSet::new());
    staging.keep_chained(|_| true);
    assert!(staging.filled.contains(raw) && staging.chained_objects.contains(raw));
    assert!(!staging.filled.contains(shown));
    staging.keep_chained(|_| false);
    assert!(
        staging.filled.is_empty()
            && staging.chained_objects.is_empty()
            && staging.chained.is_empty()
    );
    assert_eq!(
        staging.unfillable,
        [format!(
            "{shown} is not filled: its shape names a managed relation the scratch compile does not create"
        )]
    );
}

#[test]
fn catalog_prose_draft_listings_use_line_comments_and_do_not_reescape_fill_reasons() {
    let raw = "a\n\u{1b}\u{202e}\\n\\u{1b}é*/";
    let shown = r"a\n\u{1b}\u{202e}\\n\\u{1b}é*/";
    for (catalog, expected) in [
        (
            "pg_proc",
            format!(
                "{shown}: listed, never written; pbps sends no routine source to scratch, so add its definition here if a binding needs it"
            ),
        ),
        (
            "pg_cast",
            format!(
                "{shown}: write it here by hand if a binding needs it; the draft writes no cast"
            ),
        ),
    ] {
        let item = item(&Facts::default(), &key(catalog, 20_001), raw);
        let Body::Listed(line) = item.body else {
            panic!("a listing, never executable source")
        };
        assert_eq!(line, expected);
        let sql = comment(&line);
        assert!(sql.lines().all(|line| line.starts_with("-- ")));
        assert!(
            sql.contains("*/"),
            "a terminator is inert inside each -- line"
        );
        assert!(!sql.contains('\u{1b}') && !sql.contains('\u{202e}'));
    }
    let mut target = Target::default();
    let managed = target.object("pg_proc", 20_100, "app", &format!("function {raw}"));
    target.managed(&managed);
    let table = target.relation(20_110, 'r', "ext", "t", &[("id", "integer")]);
    target.facts.objects.get_mut(&table).unwrap().described = format!("table {raw}");
    target.depend(&table, &managed, 'n', true);
    let reader = target.relation(20_120, 'v', "app", "v", &[("id", "integer")]);
    target.depend(&reader, &table, 'n', false);
    target.managed(&reader);
    assert_eq!(
        target.fill().staging(&BTreeSet::new()).unfillable,
        [format!(
            "table {shown} cannot be filled: its shape names the managed function {shown}, and fill orders a shape only after the managed relations it names; write it in the externals file, or select no resolver and keep ADR-0013's conservative rebuild"
        )]
    );
    assert_eq!(
        target.draft(&[]).listed,
        [format!(
            "table {shown} cannot be staged: its shape names the managed function {shown}, which no externals file can stage before the managed objects exist; adopt table {shown} into the managed set too, or select no resolver and keep ADR-0013's conservative rebuild"
        )]
    );
}

#[test]
fn catalog_prose_foreign_and_transitive_draft_findings_show_names_without_changing_order() {
    let raw = "a\n\u{1b}\u{202e}\\n\\u{1b}é*/";
    let shown = r"a\n\u{1b}\u{202e}\\n\\u{1b}é*/";
    let mut target = Target::default();
    let foreign = target.relation(20_200, 'f', "ext", "f", &[("id", "integer")]);
    target.facts.objects.get_mut(&foreign).unwrap().described = format!("foreign table {raw}");
    let through = target.relation(20_210, 'r', "ext", "t", &[("f", "ext.f")]);
    target.facts.objects.get_mut(&through).unwrap().described = format!("table {raw}");
    target.depend(&through, &key("pg_type", 20_201), 'n', true);
    let reader = target.relation(20_220, 'v', "app", "v", &[("id", "integer")]);
    target.depend(&reader, &through, 'n', false);
    target.managed(&reader);
    assert_eq!(
        target.draft(&[]).listed,
        [
            format!(
                "foreign table {shown} cannot be staged: the comparison does not qualify a foreign table's properties"
            ),
            format!(
                "table {shown} cannot be staged: its shape names foreign table {shown}, which cannot be staged either"
            ),
        ]
    );
    assert_eq!(
        target.facts.objects[&foreign].described,
        format!("foreign table {raw}")
    );
    let (mut target, _, _, reader) = reading_a_view();
    let first = target.object("pg_proc", 20_300, "ext", "function a\n()");
    let second = target.object("pg_proc", 20_301, "ext", r"function a\n()");
    target.depend(&reader, &first, 'n', false);
    target.depend(&reader, &second, 'n', false);
    let fill = target.fill();
    let routines: Vec<_> = fill
        .objects
        .iter()
        .filter(|o| o.catalog == "pg_proc")
        .map(|o| o.described.as_str())
        .collect();
    assert_eq!(
        routines,
        ["function a\n()", r"function a\n()"],
        "raw order differs from shown order"
    );
}

/// A managed table with a column of `ext.e` (and of its array), whose enum is
/// `declared` or not. The type is declared by the project when `declared`.
fn reading_an_enum(declared: bool) -> Draft {
    let mut target = Target::default();
    let enum_type = target.object("pg_type", 20400, "ext", "type ext.e");
    target.facts.types.insert(
        20400,
        Type {
            kind: 'e',
            qualified: "ext.e".into(),
            schema: "ext".into(),
            name: "e".into(),
            ..Type::default()
        },
    );
    target.facts.labels.insert(20400, vec!["'a'".into()]);
    let array = target.object("pg_type", 20401, "ext", "type ext.e[]");
    target.facts.types.insert(
        20401,
        Type {
            kind: 'b',
            qualified: "ext._e".into(),
            schema: "ext".into(),
            name: "_e".into(),
            element: 20400,
            ..Type::default()
        },
    );
    target.depend(&array, &enum_type, 'i', true);
    let table = target.relation(20410, 'r', "app", "t", &[("m", "ext.e")]);
    target.depend(&table, &enum_type, 'n', true);
    target.managed(&table);
    let mut managed = Managed::from_schema(&pbps_model::Schema::default());
    if declared {
        let mut schema = pbps_model::Schema::default();
        schema.types.insert(
            "ext.e".parse().unwrap(),
            pbps_model::ManagedType::Enum { values: vec![] },
        );
        managed = Managed::from_schema(&schema);
    }
    for key in [enum_type, array] {
        if declared_type(&target.facts, &key.1, |s, n| managed.declares_type(s, n)) {
            target.managed(&key);
        }
    }
    target.draft(&[])
}

/// A declared enum, and its array type, belong to the project: the draft
/// stages neither. An enum nobody declares is an external object, drafted as
/// it was (#2407).
#[test]
fn a_declared_type_is_managed_and_an_undeclared_one_is_staged_as_external() {
    let external = reading_an_enum(false);
    assert!(
        external.sql.contains("CREATE TYPE ext.e AS ENUM ('a');"),
        "{}",
        external.sql
    );
    let declared = reading_an_enum(true);
    assert!(
        !declared.sql.contains("CREATE TYPE ext.e"),
        "{}",
        declared.sql
    );
}

/// What the managed set says of a type: declared enums and domains, spelled
/// as the engine addresses them, and no other.
#[test]
fn only_a_declared_types_spelling_is_managed() {
    let mut schema = pbps_model::Schema::default();
    for name in ["app.mood", "app.\"Mixed Case\""] {
        schema.types.insert(
            name.parse().unwrap(),
            pbps_model::ManagedType::Enum { values: vec![] },
        );
    }
    let managed = Managed::from_schema(&schema);
    assert!(managed.declares_type("app", "mood"));
    assert!(managed.declares_type_spelled("app.mood"));
    assert!(managed.declares_type_spelled("app.mood[]"));
    assert!(managed.declares_type_spelled("app.\"Mixed Case\""));
    // Negative: another type, another schema, a table of that name.
    assert!(!managed.declares_type("app", "other"));
    assert!(!managed.declares_type_spelled("app.other"));
    assert!(!managed.declares_type_spelled("ext.mood"));
    assert!(!managed.declares_type_spelled("mood"));
    let mut tables = pbps_model::Schema::default();
    tables
        .tables
        .insert("app.mood".parse().unwrap(), pbps_model::Table::default());
    assert!(!Managed::from_schema(&tables).declares_type("app", "mood"));
}

/// An array type is managed through its element, a row type through nothing
/// here, and a type the facts do not hold not at all.
#[test]
fn an_array_is_managed_through_its_declared_element_only() {
    let mut facts = Facts::default();
    let ty = |kind, name: &str, element| Type {
        kind,
        schema: "app".into(),
        name: name.into(),
        element,
        ..Type::default()
    };
    facts.types.insert(1, ty('e', "mood", 0));
    facts.types.insert(2, ty('b', "_mood", 1));
    facts.types.insert(3, ty('b', "_other", 4));
    facts.types.insert(4, ty('e', "other", 0));
    facts.types.insert(5, ty('c', "mood", 0));
    let declares = |s: &str, n: &str| s == "app" && n == "mood";
    assert!(declared_type(&facts, &1, declares));
    assert!(declared_type(&facts, &2, declares));
    assert!(!declared_type(&facts, &3, declares));
    assert!(!declared_type(&facts, &4, declares));
    // A composite (a relation's row type) of that name is not an enum.
    assert!(!declared_type(&facts, &5, declares));
    assert!(!declared_type(&facts, &6, declares));
}

/// A name the engine has to quote (a keyword, a capital, a quote of its own)
/// is compared as the name it is: the catalog gives the parts, and no spelling
/// is built from them (#2407 review).
#[test]
fn a_declared_type_whose_name_needs_quotes_is_recognised_as_it_is_named() {
    let mut facts = Facts::default();
    let ty = |kind, schema: &str, name: &str, element| Type {
        kind,
        schema: schema.into(),
        name: name.into(),
        element,
        ..Type::default()
    };
    facts.types.insert(1, ty('d', "app", "select", 0));
    facts.types.insert(2, ty('b', "app", "_select", 1));
    facts.types.insert(3, ty('e', "select", "Mood", 0));
    facts.types.insert(4, ty('b', "select", "_Mood", 3));
    facts.types.insert(5, ty('d', "app", "select2", 0));
    facts.types.insert(6, ty('d', "app", "Select", 0));
    let declares = |s: &str, n: &str| (s, n) == ("app", "select") || (s, n) == ("select", "Mood");
    for oid in [1, 2, 3, 4] {
        assert!(declared_type(&facts, &oid, declares), "{oid}");
    }
    for oid in [5, 6] {
        assert!(!declared_type(&facts, &oid, declares), "{oid}");
    }
}
