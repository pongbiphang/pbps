//! Replays run only in owned companion sandboxes; target inputs are immutable.

use super::{Inputs, Limitation, ObjectName, Verification, catalog, indexes};
use crate::resolver::scratch::{self, ident, literal};
use pbps_db::{Conn, DbError};
use pbps_dialect::Dialect;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

fn local(name: &ObjectName) -> String {
    format!("{}.{}", ident(&name.schema), ident(&name.name))
}

fn invalid(reason: &str) -> DbError {
    DbError::Refused(format!("dependency reconstruction: {reason}"))
}

fn column_sql(column: &catalog::Column, output_shape: bool) -> Result<String, DbError> {
    let name = ident(&column.name);
    if !output_shape
        && (column.generated_always != 0
            || column.hidden
            || column.user_defined
            || column.sparse
            || column.column_set
            || column.graph_type.is_some()
            || column.masked)
    {
        return Err(invalid(
            "generated, hidden, user-defined, sparse, column-set, graph or masked columns have no V4 template",
        ));
    }
    if column.computed && !output_shape {
        let expression = column
            .computed_definition
            .as_deref()
            .ok_or_else(|| invalid("computed expression is unreadable"))?;
        return Ok(format!(
            "{name} AS {expression}{}",
            if column.persisted == Some(true) {
                " PERSISTED"
            } else {
                ""
            }
        ));
    }
    // A fallback keeps output metadata without claiming to reproduce these
    // facets. Alias types need their native base, not an absent scratch type.
    let type_name = if output_shape && column.user_defined {
        column
            .system_type_name
            .as_deref()
            .ok_or_else(|| invalid("the user-defined type's native base is unreadable"))?
    } else {
        &column.type_name
    };
    let ty = crate::introspect::catalog_column_type(
        type_name,
        column.max_length,
        column.precision,
        column.scale,
    )
    .map_err(|error| invalid(&error.to_string()))?;
    let collation = match &column.collation {
        Some(value)
            if !value.is_empty()
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_') =>
        {
            format!(" COLLATE {value}")
        }
        Some(_) => return Err(invalid("invalid catalog collation")),
        None => String::new(),
    };
    let identity = identity_sql(column);
    // Incomplete identity metadata must not prevent an ordinary output shape.
    // The strict replay error remains the dependency's verification limitation.
    let identity = if output_shape {
        identity.unwrap_or_default()
    } else {
        identity?
    };
    Ok(format!(
        "{name} {}{collation}{identity} {}",
        ty.raw(),
        if column.nullable { "NULL" } else { "NOT NULL" }
    ))
}

fn identity_sql(column: &catalog::Column) -> Result<String, DbError> {
    if column.identity {
        let seed = column
            .seed
            .as_deref()
            .ok_or_else(|| invalid("identity seed is unreadable"))?;
        let increment = column
            .increment
            .as_deref()
            .ok_or_else(|| invalid("identity increment is unreadable"))?;
        seed.parse::<i128>()
            .map_err(|_| invalid("invalid identity seed"))?;
        increment
            .parse::<i128>()
            .map_err(|_| invalid("invalid identity increment"))?;
        Ok(format!(
            " IDENTITY({seed},{increment}){}",
            if column.not_for_replication == Some(true) {
                " NOT FOR REPLICATION"
            } else {
                ""
            }
        ))
    } else {
        Ok(String::new())
    }
}

fn table_sql(
    name: &ObjectName,
    relation: &catalog::Relation,
    output_shape: bool,
) -> Result<String, DbError> {
    if relation.columns.is_empty() {
        return Err(invalid("no readable columns"));
    }
    let columns = relation
        .columns
        .iter()
        .map(|c| column_sql(c, output_shape))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!(
        "CREATE TABLE {}({});",
        local(name),
        columns.join(", ")
    ))
}

fn order(relations: &BTreeMap<ObjectName, catalog::Relation>) -> Option<Vec<ObjectName>> {
    let mut result = Vec::new();
    let mut remaining: BTreeSet<_> = relations.keys().cloned().collect();
    while !remaining.is_empty() {
        let next = remaining
            .iter()
            .find(|name| {
                relations[*name]
                    .references
                    .iter()
                    .all(|r| !remaining.contains(r))
            })?
            .clone();
        remaining.remove(&next);
        result.push(next);
    }
    Some(result)
}

async fn create_relation(
    conn: &mut Conn,
    name: &ObjectName,
    relation: &catalog::Relation,
) -> Result<(), DbError> {
    if relation.kind == "U" {
        conn.execute(&table_sql(name, relation, false)?).await?;
    } else {
        conn.execute(&format!(
            "SET ANSI_NULLS {}; SET QUOTED_IDENTIFIER {};",
            if relation.ansi_nulls == Some(true) {
                "ON"
            } else {
                "OFF"
            },
            if relation.quoted_identifier == Some(true) {
                "ON"
            } else {
                "OFF"
            }
        ))
        .await?;
        conn.execute(
            relation
                .definition
                .as_deref()
                .ok_or_else(|| invalid("view definition is unreadable"))?,
        )
        .await?;
    }
    Ok(())
}

async fn replay(
    conn: &mut Conn,
    name: &ObjectName,
    relation: &catalog::Relation,
) -> Result<(), DbError> {
    // A renamed view can retain another CREATE name in sys.sql_modules.
    // Roll back that DDL before fallback rather than leave an untracked view.
    let framing = crate::Mssql.transaction_framing(&pbps_model::SessionZone::utc());
    conn.begin(&framing).await?;
    let result=async {
        create_relation(conn,name,relation).await?;
        let row=scratch::one(conn.query(&format!("SELECT CONVERT(int,CASE WHEN OBJECT_ID({},{}) IS NOT NULL THEN 1 ELSE 0 END) AS present;",literal(&local(name)),literal(&relation.kind))).await?,"replayed relation")?;
        if row.try_get::<i32>("present")? != Some(1) {
            return Err(invalid("the unchanged definition did not create the expected relation"));
        }
        Ok(())
    }.await;
    match result {
        Ok(()) => conn.commit(&framing).await,
        Err(error) => {
            conn.rollback(&framing).await?;
            Err(error)
        }
    }
}

async fn triggers(
    conn: &mut Conn,
    name: &ObjectName,
    relation: &catalog::Relation,
    ordinal: usize,
    token: &str,
) -> Result<(), DbError> {
    for (index, trigger) in relation.triggers.iter().enumerate() {
        if !matches!(trigger.verb.as_str(), "INSERT" | "UPDATE" | "DELETE") {
            return Err(invalid("unknown INSTEAD OF verb"));
        }
        let trigger_name = format!(
            "{}.__pbps_instead_{token}_{ordinal}_{index}",
            ident(&name.schema)
        );
        conn.execute(&format!("CREATE TRIGGER {trigger_name} ON {} INSTEAD OF {} AS BEGIN SET NOCOUNT ON; RETURN; END;", local(name), trigger.verb)).await?;
        if trigger.disabled {
            conn.execute(&format!(
                "DISABLE TRIGGER {trigger_name} ON {};",
                local(name)
            ))
            .await?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Lineage {
    is_hidden: Option<bool>,
    is_updateable: Option<bool>,
    source_database: Option<String>,
    source_schema: Option<String>,
    source_table: Option<String>,
    error_number: Option<i32>,
}

async fn lineage_complete(
    observer: &mut Conn,
    name: &ObjectName,
    relations: &BTreeMap<ObjectName, catalog::Relation>,
) -> Result<bool, DbError> {
    let sql = format!("SELECT * FROM {}.{}", ident(&name.database), local(name));
    let row = scratch::one(observer.query(&format!("SELECT (SELECT is_hidden,is_updateable,source_database,source_schema,source_table,error_number FROM sys.dm_exec_describe_first_result_set({},NULL,1) FOR JSON PATH,INCLUDE_NULL_VALUES) AS payload;", literal(&sql))).await?, "companion lineage")?;
    let rows: Vec<Lineage> = serde_json::from_str(&scratch::text(&row, "payload")?)
        .map_err(|e| invalid(&e.to_string()))?;
    Ok(physical_sources(rows, relations))
}

fn physical_sources(
    rows: Vec<Lineage>,
    relations: &BTreeMap<ObjectName, catalog::Relation>,
) -> bool {
    !rows.is_empty()
        && rows.into_iter().all(|r| {
            if r.error_number.is_some() {
                return false;
            }
            if r.is_hidden == Some(true) || r.is_updateable == Some(false) {
                return true;
            }
            match (r.source_database, r.source_schema, r.source_table) {
                (Some(database), Some(schema), Some(name)) => relations
                    .get(&ObjectName {
                        database,
                        schema,
                        name,
                    })
                    .is_some_and(|r| r.kind == "U"),
                _ => false,
            }
        })
}

fn invalidate(inputs: &mut Inputs, object: &ObjectName, limitation: Limitation) {
    for (name, input) in &mut inputs.values {
        if (ObjectName::from(name) == *object
            || input
                .catalog
                .as_ref()
                .is_some_and(|c| c.relations.contains_key(object)))
            && input.verification == Verification::Verified
        {
            input.verification = Verification::PartiallyUnverified {
                limitation: limitation.clone(),
            };
        }
    }
}

pub(super) async fn verify(inputs: &mut Inputs, observer: &mut Conn) -> Result<(), DbError> {
    let mut relations = BTreeMap::new();
    for input in inputs
        .values
        .values()
        .filter(|i| i.verification == Verification::Verified)
    {
        if let Some(catalog) = &input.catalog {
            relations.extend(
                catalog
                    .relations
                    .iter()
                    .map(|(n, r)| (n.clone(), r.clone())),
            );
        }
    }
    for (name, relation) in &relations {
        let observed = catalog::relation(observer, name).await?;
        let different = match &observed {
            Some(observed) => {
                let mut differences = relation
                    .columns
                    .iter()
                    .filter(|c| !observed.columns.contains(c))
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>();
                differences.extend(
                    observed
                        .columns
                        .iter()
                        .filter(|c| {
                            !relation
                                .columns
                                .iter()
                                .any(|expected| expected.name == c.name)
                        })
                        .map(|c| c.name.clone()),
                );
                differences
            }
            None => relation.columns.iter().map(|c| c.name.clone()).collect(),
        };
        let limitation = if !different.is_empty() {
            Some(Limitation::MetadataMismatch {
                object: name.clone(),
                columns: different,
            })
        } else if let Some(limit) = indexes::difference(
            name,
            &relation.indexes,
            &observed.as_ref().expect("compared columns").indexes,
        ) {
            Some(limit)
        } else if relation.kind == "V" && !lineage_complete(observer, name, &relations).await? {
            Some(Limitation::LineageUnavailable {
                object: name.clone(),
            })
        } else {
            None
        };
        if let Some(limitation) = limitation {
            invalidate(inputs, name, limitation);
        }
    }
    Ok(())
}

pub(super) async fn stage(
    inputs: &mut Inputs,
    sessions: &mut BTreeMap<String, Conn>,
    observer: &mut Conn,
    token: &str,
) -> Result<(), DbError> {
    if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("invalid companion token"));
    }
    if sessions.is_empty() {
        return Ok(());
    }
    let mut relations = BTreeMap::new();
    for input in inputs.values.values() {
        if let Some(catalog) = &input.catalog
            && catalog.limitation.is_none()
        {
            relations.extend(
                catalog
                    .relations
                    .iter()
                    .map(|(n, r)| (n.clone(), r.clone())),
            );
        }
    }
    let mut schemas = BTreeSet::new();
    for name in relations
        .keys()
        .cloned()
        .chain(inputs.values.keys().map(ObjectName::from))
    {
        if name.schema != "dbo" && schemas.insert((name.database.clone(), name.schema.clone())) {
            sessions
                .get_mut(&name.database)
                .ok_or_else(|| invalid("missing companion connection"))?
                .execute(&format!(
                    "CREATE SCHEMA {} AUTHORIZATION dbo;",
                    ident(&name.schema)
                ))
                .await?;
        }
    }
    let sequence = match order(&relations) {
        Some(order) => order,
        None => {
            for name in relations.keys() {
                invalidate(
                    inputs,
                    name,
                    Limitation::UnresolvedReference {
                        object: name.clone(),
                    },
                );
            }
            Vec::new()
        }
    };
    let mut created = Vec::new();
    for name in &sequence {
        let result = replay(
            sessions
                .get_mut(&name.database)
                .ok_or_else(|| invalid("missing companion connection"))?,
            name,
            &relations[name],
        )
        .await;
        match result {
            Ok(()) => {
                created.push(name.clone());
                if let Err(failure) = indexes::create(
                    sessions.get_mut(&name.database).unwrap(),
                    name,
                    &relations[name].indexes,
                )
                .await
                {
                    invalidate(inputs, name, failure.limitation(name));
                }
            }
            Err(error) => invalidate(
                inputs,
                name,
                Limitation::Replay {
                    object: name.clone(),
                    code: error.server_error_code(),
                    reason: error.to_string(),
                },
            ),
        }
    }
    for (ordinal, name) in created.iter().enumerate() {
        let relation = &relations[name];
        if let Err(error) = triggers(
            sessions.get_mut(&name.database).unwrap(),
            name,
            relation,
            ordinal,
            token,
        )
        .await
        {
            invalidate(
                inputs,
                name,
                Limitation::Replay {
                    object: name.clone(),
                    code: error.server_error_code(),
                    reason: error.to_string(),
                },
            );
        }
    }
    verify(inputs, observer).await?;
    // A fallback replaces the public object that other declared roots may use.
    // Propagate that limitation before selecting the final shared closure.
    loop {
        let bad: BTreeSet<_> = inputs
            .values
            .iter()
            .filter(|(_, i)| i.verification != Verification::Verified)
            .map(|(n, _)| ObjectName::from(n))
            .collect();
        let mut changed = false;
        for input in inputs.values.values_mut() {
            if input.verification == Verification::Verified
                && let Some(catalog) = &input.catalog
                && let Some(object) = catalog.relations.keys().find(|n| bad.contains(*n))
            {
                input.verification = Verification::PartiallyUnverified {
                    limitation: Limitation::FallbackInClosure {
                        object: object.clone(),
                    },
                };
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if inputs
        .values
        .values()
        .any(|input| input.verification != Verification::Verified)
    {
        for name in created.iter().rev() {
            let kind = if relations[name].kind == "U" {
                "TABLE"
            } else {
                "VIEW"
            };
            sessions
                .get_mut(&name.database)
                .unwrap()
                .execute(&format!("DROP {kind} {};", local(name)))
                .await?;
        }
        let needed: BTreeSet<_> = inputs
            .values
            .values()
            .filter(|i| i.verification == Verification::Verified)
            .filter_map(|i| i.catalog.as_ref())
            .flat_map(|c| c.relations.keys().cloned())
            .collect();
        for (ordinal, name) in sequence.iter().filter(|n| needed.contains(*n)).enumerate() {
            let conn = sessions.get_mut(&name.database).unwrap();
            replay(conn, name, &relations[name]).await?;
            if let Err(failure) = indexes::create(conn, name, &relations[name].indexes).await {
                let limitation = failure.limitation(name);
                invalidate(inputs, name, limitation.clone());
                return Err(invalid(&format!("{limitation:?}")));
            }
            triggers(conn, name, &relations[name], ordinal, token).await?;
        }
        for (ordinal, (name, input)) in inputs.values.iter_mut().enumerate() {
            if input.verification == Verification::Verified {
                continue;
            }
            let address = ObjectName::from(name);
            let private = format!("__pbps_shape_{token}_{ordinal}");
            let conn = sessions.get_mut(name.database()).unwrap();
            if let Some(catalog) = &input.catalog {
                let table = if catalog.root.kind == "V" {
                    ObjectName {
                        name: private.clone(),
                        ..address.clone()
                    }
                } else {
                    address.clone()
                };
                conn.execute(&table_sql(&table, &catalog.root, true)?)
                    .await?;
                if catalog.root.kind == "V" {
                    let columns = catalog
                        .root
                        .columns
                        .iter()
                        .map(|c| ident(&c.name))
                        .collect::<Vec<_>>()
                        .join(", ");
                    conn.execute(&format!(
                        "CREATE VIEW {}{} AS SELECT {columns} FROM {};",
                        local(&address),
                        if catalog.root.schema_bound == Some(true) {
                            " WITH SCHEMABINDING"
                        } else {
                            ""
                        },
                        local(&table)
                    ))
                    .await?;
                    triggers(conn, &address, &catalog.root, ordinal, token).await?;
                    let failure = match indexes::create(conn, &address, &catalog.root.indexes).await
                    {
                        Err(failure) => Some(failure.limitation(&address)),
                        Ok(()) => {
                            let observed = catalog::relation(observer, &address).await?;
                            indexes::difference(
                                &address,
                                &catalog.root.indexes,
                                &observed
                                    .ok_or_else(|| invalid("fallback view is missing"))?
                                    .indexes,
                            )
                        }
                    };
                    if let Some(mut limitation) = failure {
                        if let Limitation::Index { cause, .. } = &mut limitation
                            && let Verification::PartiallyUnverified {
                                limitation: previous,
                            } = &input.verification
                        {
                            *cause = Some(Box::new(previous.clone()));
                        }
                        input.verification = Verification::PartiallyUnverified { limitation };
                    }
                }
            } else {
                for statement in super::declaration(name, &input.declaration, &private)? {
                    conn.execute(&statement).await?;
                }
            }
        }
    }
    verify(inputs, observer).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column() -> catalog::Column {
        serde_json::from_value(serde_json::json!({
            "name":"c]", "column_id":1,"type_name":"nvarchar","type_schema":"sys","user_defined":false,
            "max_length":100,"precision":0,"scale":0,"collation":"Latin1_General_BIN2","nullable":false,
            "identity":false,"computed":false,"computed_definition":null,"persisted":null,"seed":null,
            "increment":null,"not_for_replication":null,"generated_always":0,"hidden":false,
            "system_type_name":"nvarchar","sparse":false,"column_set":false,"graph_type":null,"masked":false
        })).unwrap()
    }

    #[test]
    fn native_facets_and_computed_expressions_are_preserved_and_incomplete_ones_refuse() {
        let mut c = column();
        assert_eq!(
            column_sql(&c, false).unwrap(),
            "[c]]] nvarchar(50) COLLATE Latin1_General_BIN2 NOT NULL"
        );
        c.type_name = "int".into();
        c.collation = None;
        c.identity = true;
        c.seed = Some("7".into());
        c.increment = Some("2".into());
        assert!(
            column_sql(&c, false)
                .unwrap()
                .contains("IDENTITY(7,2) NOT NULL")
        );
        c.seed = Some("1); DROP TABLE t;".into());
        assert!(column_sql(&c, false).is_err());
        c.identity = false;
        c.computed = true;
        c.computed_definition = Some("([id]*(2))".into());
        c.persisted = Some(true);
        assert_eq!(
            column_sql(&c, false).unwrap(),
            "[c]]] AS ([id]*(2)) PERSISTED"
        );
        c.computed_definition = None;
        assert!(column_sql(&c, false).is_err());
        c.computed = false;
        c.type_name = "timestamp".into();
        assert!(
            column_sql(&c, false)
                .unwrap()
                .contains("timestamp NOT NULL")
        );
        c.hidden = true;
        assert!(column_sql(&c, false).is_err());
    }

    fn ordinary_fallback(c: &catalog::Column, expected: &str) {
        assert!(
            column_sql(c, false).is_err(),
            "strict replay accepted {c:?}"
        );
        assert_eq!(column_sql(c, true).unwrap(), expected);
    }

    #[test]
    fn every_generated_always_kind_and_hidden_column_has_an_ordinary_fallback() {
        for generated in 1..=u8::MAX {
            let mut c = column();
            c.generated_always = generated;
            ordinary_fallback(
                &c,
                "[c]]] nvarchar(50) COLLATE Latin1_General_BIN2 NOT NULL",
            );
        }
        let mut c = column();
        c.hidden = true;
        ordinary_fallback(
            &c,
            "[c]]] nvarchar(50) COLLATE Latin1_General_BIN2 NOT NULL",
        );
    }

    #[test]
    fn sparse_columns_have_an_ordinary_nullable_fallback() {
        let mut c = column();
        c.sparse = true;
        c.nullable = true;
        ordinary_fallback(&c, "[c]]] nvarchar(50) COLLATE Latin1_General_BIN2 NULL");
    }

    #[test]
    fn column_sets_have_an_ordinary_xml_fallback() {
        let mut c = column();
        c.column_set = true;
        c.type_name = "xml".into();
        c.collation = None;
        c.nullable = true;
        ordinary_fallback(&c, "[c]]] xml NULL");
    }

    #[test]
    fn graph_pseudo_columns_do_not_need_a_computed_expression_for_fallback() {
        for graph_type in 1..=u8::MAX {
            let mut c = column();
            c.graph_type = Some(graph_type);
            c.computed = true;
            c.computed_definition = Some("(1)".into());
            ordinary_fallback(
                &c,
                "[c]]] nvarchar(50) COLLATE Latin1_General_BIN2 NOT NULL",
            );
            c.computed_definition = None;
            ordinary_fallback(
                &c,
                "[c]]] nvarchar(50) COLLATE Latin1_General_BIN2 NOT NULL",
            );
        }
    }

    #[test]
    fn masked_columns_keep_their_output_type_and_collation_in_fallback() {
        let mut c = column();
        c.masked = true;
        ordinary_fallback(
            &c,
            "[c]]] nvarchar(50) COLLATE Latin1_General_BIN2 NOT NULL",
        );
    }

    #[test]
    fn alias_columns_use_their_native_base_in_fallback_without_replaying_the_alias() {
        let mut c = column();
        c.user_defined = true;
        c.type_schema = "custom".into();
        c.type_name = "alias2323".into();
        ordinary_fallback(
            &c,
            "[c]]] nvarchar(50) COLLATE Latin1_General_BIN2 NOT NULL",
        );
        c.system_type_name = None;
        assert!(column_sql(&c, true).is_err());
    }

    #[test]
    fn unavailable_identity_facets_fall_back_without_guessing_seed_or_increment() {
        for (seed, increment) in [
            (None, Some("2")),
            (Some("7"), None),
            (Some("1); DROP TABLE t;"), Some("2")),
            (Some("7"), Some("bad")),
        ] {
            let mut c = column();
            c.type_name = "int".into();
            c.collation = None;
            c.identity = true;
            c.seed = seed.map(str::to_owned);
            c.increment = increment.map(str::to_owned);
            ordinary_fallback(&c, "[c]]] int NOT NULL");
        }
        let mut c = column();
        c.type_name = "int".into();
        c.collation = None;
        c.identity = true;
        c.seed = Some("7".into());
        c.increment = Some("2".into());
        assert_eq!(
            column_sql(&c, true).unwrap(),
            "[c]]] int IDENTITY(7,2) NOT NULL"
        );
    }

    #[test]
    fn unreadable_computed_expressions_materialize_only_the_output_metadata() {
        let mut c = column();
        c.computed = true;
        ordinary_fallback(
            &c,
            "[c]]] nvarchar(50) COLLATE Latin1_General_BIN2 NOT NULL",
        );
    }

    #[test]
    fn fallback_still_refuses_unsafe_collations_and_unknown_native_types() {
        let mut c = column();
        c.collation = Some("Latin1_General_BIN2; DROP TABLE t;".into());
        for output_shape in [false, true] {
            assert!(column_sql(&c, output_shape).is_err());
        }
        c.collation = None;
        c.type_name = "unknown2323".into();
        for output_shape in [false, true] {
            assert!(column_sql(&c, output_shape).is_err());
        }
    }

    #[test]
    fn a_view_self_source_is_not_physical_lineage_even_when_updateable() {
        let root = ObjectName {
            database: "db".into(),
            schema: "dbo".into(),
            name: "v".into(),
        };
        let relation:catalog::Relation=serde_json::from_value(serde_json::json!({"kind":"V","readable":true,"definition":"CREATE VIEW dbo.v AS SELECT 1 c","ansi_nulls":true,"quoted_identifier":true,"columns":[],"triggers":[],"indexes":[]})).unwrap();
        let mut relations = BTreeMap::from([(root.clone(), relation)]);
        let rows = || {
            vec![Lineage {
                is_hidden: Some(false),
                is_updateable: Some(true),
                source_database: Some("db".into()),
                source_schema: Some("dbo".into()),
                source_table: Some("v".into()),
                error_number: None,
            }]
        };
        assert!(!physical_sources(rows(), &relations));
        relations.get_mut(&root).unwrap().kind = "U".into();
        assert!(physical_sources(rows(), &relations));
        assert!(!physical_sources(Vec::new(), &relations));
        let mut failure = rows();
        failure[0].error_number = Some(208);
        assert!(!physical_sources(failure, &relations));
    }
}
