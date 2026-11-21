use super::*;
use pbps_dialect::{DialectError, MinimalDialect, TypeChangeRisk};
use pbps_model::PartitionChange;
use pbps_model::SearchChange;
struct Engine {
    enabled: bool,
    label: &'static str,
}
impl Dialect for Engine {
    fn name(&self) -> &'static str {
        self.label
    }
    fn partition_meetings(&self) -> bool {
        self.enabled
    }
    fn lexicon(&self) -> pbps_dialect::Lexicon {
        MinimalDialect.lexicon()
    }
    fn transaction_framing(&self, z: &pbps_model::SessionZone) -> pbps_dialect::TransactionFraming {
        MinimalDialect.transaction_framing(z)
    }
    fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
        None
    }
    fn normalize_type(&self, t: &ColumnType) -> Result<ColumnType, DialectError> {
        MinimalDialect.normalize_type(t)
    }
    fn type_change_risk(&self, a: &ColumnType, b: &ColumnType) -> TypeChangeRisk {
        MinimalDialect.type_change_risk(a, b)
    }
    fn fold_ident<'a>(&self, i: &'a str) -> std::borrow::Cow<'a, str> {
        MinimalDialect.fold_ident(i)
    }
    fn quote_ident(&self, i: &str) -> Result<String, DialectError> {
        MinimalDialect.quote_ident(i)
    }
    fn validate_table(&self, n: &TableName, t: &Table) -> Vec<DialectError> {
        MinimalDialect.validate_table(n, t)
    }
    fn intermediate_tables(
        &self,
        c: &Change,
        strategy: pbps_model::Strategy,
    ) -> Result<Vec<TableName>, DialectError> {
        MinimalDialect.intermediate_tables(c, strategy)
    }
}
#[test]
fn pending_shape_ignores_annotations_but_keeps_every_column_fact() {
    let old = Table {
        columns: [(
            "d".into(),
            pbps_model::Column::new("integer".parse().unwrap()),
        )]
        .into(),
        ..Default::default()
    };
    let mut annotated = old.clone();
    annotated.columns.get_mut("d").unwrap().description = Some("description".into());
    annotated.columns.get_mut("d").unwrap().deprecated = Some("reason".into());
    assert!(!parent_shape_changes(&old, &annotated));
    for property in 0..6 {
        let mut new = old.clone();
        let c = new.columns.get_mut("d").unwrap();
        match property {
            0 => c.ty = "bigint".parse().unwrap(),
            1 => c.nullable = false,
            2 => c.collation = Some(pbps_model::Collation::new("other")),
            3 => c.value_source = ValueSource::Default("1".into()),
            4 => {
                new.columns.swap_remove("d");
            }
            _ => {
                let c = new.columns.shift_remove("d").unwrap();
                new.columns.insert("e".into(), c);
            }
        }
        assert!(parent_shape_changes(&old, &new), "property {property}");
    }
}
#[test]
fn meeting_addresses_follow_capability_and_uid() {
    use pbps_model::{Column, PartitionBound, PartitionBy, PartitionOf};
    let p: TableName = "app.p".parse().unwrap();
    let q: TableName = "app.q".parse().unwrap();
    let a: TableName = "app.a".parse().unwrap();
    let parent = Table {
        partition_by: Some(PartitionBy {
            columns: vec!["k".into()],
            strategy: Default::default(),
            external: false,
        }),
        columns: [("k".into(), Column::new("integer".parse().unwrap()))].into(),
        ..Default::default()
    };
    let mut plain = parent.clone();
    plain.partition_by = None;
    let part = Table {
        partition_of: Some(PartitionOf {
            parent: p.clone(),
            bound: PartitionBound::Default,
            columns: Default::default(),
        }),
        ..Default::default()
    };
    for attaching in [false, true] {
        let mut base = Schema::default();
        base.tables.insert(p.clone(), parent.clone());
        base.tables.insert(
            a.clone(),
            if attaching {
                plain.clone()
            } else {
                part.clone()
            },
        );
        let mut goal = Schema::default();
        goal.tables.insert(q.clone(), parent.clone());
        let mut final_part = part.clone();
        final_part.partition_of.as_mut().unwrap().parent = q.clone();
        goal.tables.insert(
            a.clone(),
            if attaching { final_part } else { plain.clone() },
        );
        let mut ids = IdsFile::default();
        ids.tables.insert("t_aaaaaa".parse().unwrap(), p.clone());
        ids.tables.insert("t_bbbbbb".parse().unwrap(), a.clone());
        ids.columns
            .insert("c_aaaaaa".parse().unwrap(), p.column("k"));
        let mut after = ids.clone();
        after.tables.insert("t_aaaaaa".parse().unwrap(), q.clone());
        after
            .columns
            .insert("c_aaaaaa".parse().unwrap(), q.column("k"));
        for label in ["postgres", "other"] {
            for enabled in [false, true] {
                let engine = Engine { label, enabled };
                let d = diff_partial(
                    Side {
                        schema: &base,
                        ids: &ids,
                    },
                    Side {
                        schema: &goal,
                        ids: &after,
                    },
                    &engine,
                    &Hints::default(),
                );
                assert_eq!(
                    d.errors.is_empty(),
                    enabled,
                    "{label}/{enabled}: {:?}",
                    d.errors
                );
                let boundary = d.changes.changes.iter().find_map(|p| match &p.change {
                    Change::Partition(PartitionChange::AttachPartition { parent, .. })
                    | Change::Partition(PartitionChange::DetachPartition { parent, .. }) => {
                        Some(parent)
                    }
                    _ => None,
                });
                if enabled {
                    assert_eq!(boundary, Some(&q));
                } else if attaching {
                    assert!(boundary.is_none());
                } else {
                    assert_eq!(boundary, Some(&p));
                }
            }
        }
    }
}
/// A text search object the declarations hold is compared with the target's by recorded
/// identity (DEC-2728.2): one the target lacks is created, one the declarations gave up
/// is dropped, and a change of a standing one's template is refused by name. An identical one
/// is no difference, and one only the target has, with no identity, is not this
/// project's.
#[test]
fn text_search_objects_are_created_and_dropped_by_identity_and_a_new_template_is_refused() {
    use pbps_model::{SearchKind, SearchObject, SearchRef, Uid, UidKind};
    let name = |kind, text: &str| SearchRef::new(kind, text.parse().unwrap());
    let dictionary = |template: &str| SearchObject::Dictionary {
        template: template.parse().unwrap(),
        options: Default::default(),
    };
    let (mut base, mut declared) = (Schema::default(), Schema::default());
    let (mut base_ids, mut declared_ids) = (IdsFile::default(), IdsFile::default());
    let hold = |schema: &mut Schema, ids: &mut IdsFile, uid: &Uid, n: &SearchRef, d| {
        schema.search.insert(n.clone(), d);
        ids.search.insert(uid.clone(), n.clone());
    };
    let same = name(SearchKind::Dictionary, "app.same");
    let uid = Uid::generate(UidKind::Search);
    hold(
        &mut base,
        &mut base_ids,
        &uid,
        &same,
        dictionary("pg_catalog.simple"),
    );
    hold(
        &mut declared,
        &mut declared_ids,
        &uid,
        &same,
        dictionary("pg_catalog.simple"),
    );
    // Only the target has it, and records no identity for it: not this project's.
    base.search.insert(
        name(SearchKind::Config, "app.theirs"),
        SearchObject::Config {
            parser: "pg_catalog.\"default\"".parse().unwrap(),
            mappings: Default::default(),
        },
    );
    let diffed = |base: &Schema, base_ids: &IdsFile, declared: &Schema, declared_ids: &IdsFile| {
        let mut errs = Vec::new();
        let changes = diff_search(
            Side {
                schema: base,
                ids: base_ids,
            },
            Side {
                schema: declared,
                ids: declared_ids,
            },
            &Hints {
                target_read: true,
                ..Hints::default()
            },
            &mut errs,
        );
        (
            changes,
            errs.iter().map(ToString::to_string).collect::<Vec<_>>(),
        )
    };
    let (changes, errs) = diffed(&base, &base_ids, &declared, &declared_ids);
    assert!(
        changes.is_empty() && errs.is_empty(),
        "{changes:?} {errs:?}"
    );
    // A declared object the target lacks is created under its recorded uid.
    let new = name(SearchKind::Dictionary, "app.new");
    let new_uid = Uid::generate(UidKind::Search);
    hold(
        &mut declared,
        &mut declared_ids,
        &new_uid,
        &new,
        dictionary("pg_catalog.simple"),
    );
    let (changes, errs) = diffed(&base, &base_ids, &declared, &declared_ids);
    assert!(errs.is_empty(), "{errs:?}");
    assert!(matches!(changes.as_slice(),
        [Change::Search(SearchChange::CreateSearch { uid, name, .. })] if uid == &new_uid && name == &new));
    // A declaration the baseline holds, then gives up, is dropped.
    declared.search.remove(&same);
    declared_ids.search.remove(&uid);
    let (changes, _) = diffed(&base, &base_ids, &declared, &declared_ids);
    assert!(changes.iter().any(|c| matches!(c,
        Change::Search(SearchChange::DropSearch { uid: u, name, .. }) if u == &uid && name == &same)));
    // The target's own object with no identity is never dropped.
    assert!(
        !changes.iter().any(
            |c| matches!(c, Change::Search(SearchChange::DropSearch { name, .. }) if name.name.name() == "theirs")
        )
    );
    // A standing difference, a missing identity and a rename are refused by name.
    declared
        .search
        .insert(same.clone(), dictionary("pg_catalog.synonym"));
    declared_ids.search.insert(uid.clone(), same.clone());
    let (changes, errs) = diffed(&base, &base_ids, &declared, &declared_ids);
    assert!(
        !changes
            .iter()
            .any(|c| matches!(c, Change::Search(SearchChange::DropSearch { .. })))
    );
    assert!(
        errs.iter()
            .any(|m| m.contains("app.same") && m.contains("template differs")),
        "{errs:?}"
    );
    let loose = name(SearchKind::Dictionary, "app.loose");
    declared
        .search
        .insert(loose, dictionary("pg_catalog.simple"));
    let (_, errs) = diffed(&base, &base_ids, &declared, &declared_ids);
    assert!(
        errs.iter()
            .any(|m| m.contains("app.loose") && m.contains("no recorded identity")),
        "{errs:?}"
    );
    let renamed = name(SearchKind::Dictionary, "app.renamed");
    declared.search.remove(&same);
    declared
        .search
        .insert(renamed.clone(), dictionary("pg_catalog.simple"));
    declared_ids.search.insert(uid.clone(), renamed);
    let (changes, errs) = diffed(&base, &base_ids, &declared, &declared_ids);
    assert!(
        errs.iter()
            .any(|m| m.contains("app.renamed") && m.contains("renaming one")),
        "{errs:?}"
    );
    assert!(!changes.iter().any(
        |c| matches!(c, Change::Search(SearchChange::CreateSearch { name, .. }) if name.name.name() == "renamed")
    ));
    // Two kinds of one name are two objects: the configuration is absent though the
    // dictionary of that name is held.
    let config = name(SearchKind::Config, "app.same");
    let config_uid = Uid::generate(UidKind::Search);
    hold(
        &mut declared,
        &mut declared_ids,
        &config_uid,
        &config,
        SearchObject::Config {
            parser: "pg_catalog.\"default\"".parse().unwrap(),
            mappings: Default::default(),
        },
    );
    let (changes, _) = diffed(&base, &base_ids, &declared, &declared_ids);
    assert!(changes.iter().any(
        |c| matches!(c, Change::Search(SearchChange::CreateSearch { name, .. }) if name == &config)
    ));
}

/// A standing text search object whose mappings or options differ is altered in place
/// (DEC-2728.3), and only against the database, and only when nothing stands on it that
/// would keep the old behaviour. Refused by name, each with its remedy: a changed parser
/// or template, an offline plan or a census that was not read (absent is not empty), a
/// dependent of the object or of a configuration that maps the dictionary, a thesaurus
/// that names the dictionary, and a mapping to a dictionary nobody declares. Negatives
/// beside each: an empty census plans it, and an object that did not change plans nothing.
#[test]
fn a_standing_text_search_object_is_altered_in_place_unless_something_stale_stands_on_it() {
    use pbps_model::{SearchKind, SearchObject, SearchRef, Uid, UidKind};
    let name = |kind, text: &str| SearchRef::new(kind, text.parse().unwrap());
    let words = name(SearchKind::Dictionary, "app.words");
    let docs = name(SearchKind::Config, "app.docs");
    let dictionary = |template: &str, language: &str| SearchObject::Dictionary {
        template: template.parse().unwrap(),
        options: [("language".to_owned(), language.to_owned())].into(),
    };
    let config = |parser: &str, mapped: &[&str]| SearchObject::Config {
        parser: parser.parse().unwrap(),
        mappings: [(
            "word".to_owned(),
            mapped.iter().map(|d| d.parse().unwrap()).collect(),
        )]
        .into(),
    };
    let default = "pg_catalog.\"default\"";
    let (words_uid, docs_uid) = (
        Uid::generate(UidKind::Search),
        Uid::generate(UidKind::Search),
    );
    let mut ids = IdsFile::default();
    ids.search.insert(words_uid.clone(), words.clone());
    ids.search.insert(docs_uid.clone(), docs.clone());
    let held = |dictionary: SearchObject, config: SearchObject| {
        let mut schema = Schema::default();
        schema.search.insert(words.clone(), dictionary);
        schema.search.insert(docs.clone(), config);
        schema
    };
    let base = held(
        dictionary("pg_catalog.snowball", "english"),
        config(default, &["app.words"]),
    );
    let diffed = |declared: &Schema, hints: &Hints| {
        let mut errs = Vec::new();
        let changes = diff_search(
            Side {
                schema: &base,
                ids: &ids,
            },
            Side {
                schema: declared,
                ids: &ids,
            },
            hints,
            &mut errs,
        );
        (
            changes,
            errs.iter().map(ToString::to_string).collect::<Vec<_>>(),
        )
    };
    let read = |entries: &[(&SearchRef, &[&str])]| Hints {
        target_read: true,
        search_dependents: entries
            .iter()
            .map(|(n, d)| {
                (
                    (*n).clone(),
                    pbps_model::SearchCensus {
                        stored: d.iter().map(ToString::to_string).collect(),
                        opaque: Vec::new(),
                    },
                )
            })
            .collect(),
        ..Hints::default()
    };
    let read_opaque = |name: &SearchRef, stored: &[&str], opaque: &[&str]| Hints {
        target_read: true,
        search_dependents: [(
            name.clone(),
            pbps_model::SearchCensus {
                stored: stored.iter().map(ToString::to_string).collect(),
                opaque: opaque.iter().map(ToString::to_string).collect(),
            },
        )]
        .into(),
        ..Hints::default()
    };
    // Nothing changed: nothing planned, whatever the census says.
    let (changes, errs) = diffed(&base, &Hints::default());
    assert!(
        changes.is_empty() && errs.is_empty(),
        "{changes:?} {errs:?}"
    );
    // A new mapping list: an alteration of the configuration alone.
    let remapped = held(
        dictionary("pg_catalog.snowball", "english"),
        config(default, &["app.words", "pg_catalog.simple"]),
    );
    let (changes, errs) = diffed(&remapped, &read(&[(&docs, &[])]));
    assert!(errs.is_empty(), "{errs:?}");
    assert!(matches!(changes.as_slice(),
        [Change::Search(SearchChange::AlterSearchMapping { uid, name, before, after, opaque })]
            if uid == &docs_uid && name == &docs && **before != **after && opaque.is_empty()));
    // A new option: an alteration of the dictionary alone.
    let reworded = held(
        dictionary("pg_catalog.snowball", "german"),
        config(default, &["app.words"]),
    );
    let (changes, errs) = diffed(&reworded, &read(&[(&words, &[])]));
    assert!(errs.is_empty(), "{errs:?}");
    assert!(matches!(changes.as_slice(),
        [Change::Search(SearchChange::AlterSearchDictionary { uid, name, .. })]
            if uid == &words_uid && name == &words));
    // The census names what would stay stale, and the remedy; nothing is planned.
    let (changes, errs) = diffed(
        &reworded,
        &read(&[(&words, &["index app.t_gin", "constraint k on table app.t"])]),
    );
    assert!(changes.is_empty(), "{changes:?}");
    assert!(
        errs.iter().any(|m| m.contains("app.words")
            && m.contains("index app.t_gin, constraint k on table app.t")
            && m.contains("new name")),
        "{errs:?}"
    );
    // What the catalog cannot see into is carried by the change, listed and gated by the
    // `stale-results` class, and is not a refusal. Negative: with none listed, the change
    // carries no such class.
    let behind =
        "index app.t_gin (behind function app.f(text), whose body the catalog cannot read)";
    let (changes, errs) = diffed(&reworded, &read_opaque(&words, &[], &[behind]));
    assert!(errs.is_empty(), "{errs:?}");
    let [change @ Change::Search(SearchChange::AlterSearchDictionary { opaque, .. })] =
        changes.as_slice()
    else {
        panic!("{changes:?}")
    };
    assert_eq!(opaque, &[behind.to_owned()]);
    assert!(
        change
            .intrinsic_risks()
            .contains(&pbps_model::RiskClass::StaleResults)
    );
    let (changes, _) = diffed(&reworded, &read(&[(&words, &[])]));
    assert!(
        !changes[0]
            .intrinsic_risks()
            .contains(&pbps_model::RiskClass::StaleResults)
    );
    // What the catalog proves wins: the same alteration is refused, whatever else is opaque.
    let (changes, errs) = diffed(
        &reworded,
        &read_opaque(&words, &["index app.t_gin"], &[behind]),
    );
    assert!(changes.is_empty(), "{changes:?}");
    assert!(
        errs.iter().any(|m| m.contains("index app.t_gin")),
        "{errs:?}"
    );
    // Not read is not empty: offline, and a census with no entry for the object.
    for (what, hints) in [
        ("offline", Hints::default()),
        ("an object not read", read(&[(&docs, &[])])),
        (
            "an entry from before the database was read",
            Hints {
                target_read: false,
                ..read(&[(&words, &[])])
            },
        ),
    ] {
        let (changes, errs) = diffed(&reworded, &hints);
        assert!(changes.is_empty(), "{what}: {changes:?}");
        assert!(
            errs.iter()
                .any(|m| m.contains("app.words") && m.contains("--db")),
            "{what}: {errs:?}"
        );
    }
    // No `ALTER` form changes a parser or a template: refused by name, in place of a drop
    // and a create.
    for (what, declared, needle) in [
        (
            "a parser",
            held(
                dictionary("pg_catalog.snowball", "english"),
                config("pg_catalog.other", &["app.words"]),
            ),
            "parser differs",
        ),
        (
            "a template",
            held(
                dictionary("pg_catalog.simple", "english"),
                config(default, &["app.words"]),
            ),
            "template differs",
        ),
    ] {
        let (changes, errs) = diffed(&declared, &read(&[(&words, &[]), (&docs, &[])]));
        assert!(changes.is_empty(), "{what}: {changes:?}");
        assert!(errs.iter().any(|m| m.contains(needle)), "{what}: {errs:?}");
    }
    // A mapping to a dictionary the project does not declare and the target does not hold
    // is refused against the database, as a create's is.
    let missing = held(
        dictionary("pg_catalog.snowball", "english"),
        config(default, &["app.words", "app.unknown"]),
    );
    let (changes, errs) = diffed(&missing, &read(&[(&docs, &[])]));
    assert!(changes.is_empty(), "{changes:?}");
    assert!(
        errs.iter()
            .any(|m| m.contains("app.docs") && m.contains("app.unknown")),
        "{errs:?}"
    );
    // A reference the standing object already makes is the target's, resolved: changing
    // something else alters, and only a reference the alteration adds is checked.
    let standing = held(
        dictionary("pg_catalog.snowball", "english"),
        config(default, &["app.words", "app.unknown"]),
    );
    let from_standing = |declared: &Schema| {
        let mut errs = Vec::new();
        let changes = diff_search(
            Side {
                schema: &standing,
                ids: &ids,
            },
            Side {
                schema: declared,
                ids: &ids,
            },
            &read(&[(&words, &[]), (&docs, &[])]),
            &mut errs,
        );
        (changes, errs)
    };
    let (changes, errs) = from_standing(&held(
        dictionary("pg_catalog.snowball", "english"),
        config(default, &["app.words", "app.unknown", "pg_catalog.simple"]),
    ));
    assert!(errs.is_empty(), "{errs:?}");
    assert!(
        matches!(
            changes.as_slice(),
            [Change::Search(SearchChange::AlterSearchMapping { .. })]
        ),
        "{changes:?}"
    );
    // Negative: a new reference beside it is still refused.
    let (changes, errs) = from_standing(&held(
        dictionary("pg_catalog.snowball", "english"),
        config(default, &["app.words", "app.unknown", "app.other"]),
    ));
    assert!(changes.is_empty(), "{changes:?}");
    let errs: Vec<_> = errs.iter().map(ToString::to_string).collect();
    assert!(
        errs.iter()
            .any(|m| m.contains("app.other") && !m.contains("app.unknown")),
        "{errs:?}"
    );
}

/// The differ from nothing is the one the rehearsal and the resolver build a base
/// with, and it builds what a plan can build: a text search object, declared or held,
/// is left out of it rather than refused by name (#2855, DEC-2855.1). The ordinary
/// differ still refuses a declared one, so no plan can claim to have made it.
#[test]
fn the_differ_from_nothing_does_not_refuse_a_text_search_object_the_ordinary_one_does() {
    let mut declared = Schema::default();
    declared.search.insert(
        pbps_model::SearchRef::new(pbps_model::SearchKind::Dictionary, "app.d".parse().unwrap()),
        pbps_model::SearchObject::Dictionary {
            template: "pg_catalog.simple".parse().unwrap(),
            options: Default::default(),
        },
    );
    let ids = IdsFile::default();
    let empty = Schema::default();
    let side = |schema| Side { schema, ids: &ids };
    let hints = Hints::default();
    let ordinary = diff(side(&empty), side(&declared), &MinimalDialect, &hints);
    assert!(ordinary.is_err(), "refused by the ordinary differ");
    let built = bootstrap(side(&empty), side(&declared), &MinimalDialect, &hints);
    assert!(built.is_ok(), "{:?}", built.err());
}

/// A standalone sequence the declarations hold is compared with the target's and every
/// difference is refused by name, since nothing plans them yet (DEC-2721.1): one the
/// target lacks, one it holds differently, said attribute by attribute. An identical
/// one is no difference, a spelling of the same type is not one, and one only the
/// target has is not this project's.
#[test]
fn every_difference_in_a_standalone_sequence_is_refused_by_name_and_an_undeclared_one_is_left_alone()
 {
    let sequence = |ty: &str, start: i64, increment: i64| pbps_model::Sequence {
        ty: ColumnType::simple(ty),
        start,
        increment,
        options: Default::default(),
    };
    let mut base = Schema::default();
    let mut declared = Schema::default();
    for (name, held) in [
        ("app.same", sequence("bigint", 1, 1)),
        // Only the target has it: not this project's.
        ("app.theirs", sequence("bigint", 1, 1)),
    ] {
        base.sequences.insert(name.parse().unwrap(), held);
    }
    declared
        .sequences
        .insert("app.same".parse().unwrap(), sequence("bigint", 1, 1));
    let mut errs = Vec::new();
    diff_sequences(&base, &declared, &MinimalDialect, &mut errs);
    assert!(errs.is_empty(), "{errs:?}");
    // Changed in two attributes, and absent from the target.
    base.sequences
        .insert("app.changed".parse().unwrap(), sequence("integer", 1, 1));
    let mut wanted = sequence("integer", 100, 1);
    wanted.options.cycle = true;
    declared
        .sequences
        .insert("app.changed".parse().unwrap(), wanted);
    declared
        .sequences
        .insert("app.new".parse().unwrap(), sequence("bigint", 1, 1));
    diff_sequences(&base, &declared, &MinimalDialect, &mut errs);
    let said: Vec<String> = errs.iter().map(ToString::to_string).collect();
    assert_eq!(said.len(), 2, "{said:?}");
    assert!(
        said.iter().any(|m| m.contains("sequence `app.changed`")
            && m.contains("start: declared 100, target has 1")
            && m.contains("cycle: declared true, target has false")
            && !m.contains("type:")
            && !m.contains("increment:")),
        "{said:?}"
    );
    assert!(
        said.iter()
            .any(|m| m.contains("sequence `app.new`") && m.contains("creating one")),
        "{said:?}"
    );
    // Negative: nothing about the counter can differ, because no field holds it.
    assert!(said.iter().all(|m| !m.contains("last_value")), "{said:?}");
}
