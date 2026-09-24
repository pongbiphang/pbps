//! Fixed catalog SQL for the owned capture. Identifier interpolation comes
//! only from the qualified adapter layouts, never from retained SQL or a
//! target-supplied object name. All source-bearing results stay in memory.

use super::properties::{self, Field, Uncovered};
use super::render::Selection;

type Result<T> = std::result::Result<T, Uncovered>;

pub(super) fn batch(major: u32, selection: Option<&Selection>) -> Result<String> {
    let mut parts = Vec::new();
    for &class in properties::CLASSES {
        let selected = selection.map_or_else(
            || "false".to_owned(),
            |selection| selection.predicate(class),
        );
        let mut columns = Vec::new();
        let mut definitions = Vec::new();
        for &(name, kind) in properties::fields(class, major)? {
            let qualified = format!("c.{name}");
            let field = properties::field(class, name, kind)?;
            let expression = match field {
                Field::Acl => format!(
                    "CASE WHEN {qualified} IS NULL THEN NULL ELSE (SELECT COALESCE(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object('grantor', a.grantor::bigint, 'grantee', a.grantee::bigint, 'privilege_type', a.privilege_type, 'is_grantable', a.is_grantable)), '[]'::jsonb) FROM pg_catalog.aclexplode({qualified}) a) END"
                ),
                // regproc's JSON output is a live-cache name. Read the number
                // and resolve it against this snapshot's pg_proc instead.
                _ if matches!(kind, "oid" | "regproc") => format!("{qualified}::oid::bigint"),
                _ if matches!(kind, "oidvector" | "_oid") => {
                    format!("{qualified}::oid[]::bigint[]")
                }
                _ if kind == "int2vector" => format!("{qualified}::smallint[]"),
                // A non-null missing-value array may call a user-defined
                // output routine. The first pass records presence only; its
                // element type must qualify before rendering the second pass.
                _ if kind == "anyarray" => {
                    format!(
                        "CASE WHEN {qualified} IS NULL THEN NULL WHEN {selected} THEN pg_catalog.to_jsonb({qualified}) ELSE 'true'::jsonb END"
                    )
                }
                Field::Scalar
                | Field::Reference(_)
                | Field::References(_)
                | Field::Columns(_)
                | Field::Column(_)
                | Field::Definition
                | Field::Physical
                | Field::Address => qualified.clone(),
            };
            columns.push(format!("'{name}', {expression}"));
            if field == Field::Definition {
                let definition = if selection.is_some() {
                    let renderer = renderer(class, name)?;
                    format!(
                        "CASE WHEN {qualified} IS NULL OR {qualified}::text = '<>' OR NOT ({selected}) THEN NULL ELSE {renderer} END"
                    )
                } else {
                    "NULL".to_owned()
                };
                definitions.push(format!("'{name}', {definition}"));
            }
        }
        if let Some(function) = provider_version(class) {
            // The provider's current version is outside MVCC. False is the
            // raw/unselected marker; NULL is a qualified provider absence.
            let actual = if selection.is_some() {
                format!(
                    "CASE WHEN {selected} THEN pg_catalog.to_jsonb(pg_catalog.{function}(c.oid)) ELSE 'false'::jsonb END"
                )
            } else {
                "'false'::jsonb".into()
            };
            definitions.push(format!("'actual_provider_version', {actual}"));
        }
        if let Some(definition) = complete_definition(class) {
            let definition = if selection.is_some() {
                format!("CASE WHEN {selected} THEN {definition} ELSE NULL END")
            } else {
                "NULL".into()
            };
            definitions.push(format!("'complete', {definition}"));
        }
        columns.push(format!(
            "'__definitions', pg_catalog.jsonb_build_object({})",
            definitions.join(", ")
        ));
        let row = format!("pg_catalog.jsonb_build_object({})", columns.join(", "));
        // MVCC tuple coordinates are a same-database, same-capture guard only.
        // They never enter fingerprints or target/scratch identity matching.
        let witness = if class == "pg_roles" {
            // pg_roles exposes no xmin, and requiring pg_authid would turn a
            // source read into access to password verifiers. Snapshot-based
            // role names are used throughout; renderer role-valued constants
            // need a separate qualified representation.
            "NULL::jsonb".to_owned()
        } else {
            "pg_catalog.jsonb_build_array(c.ctid::text, c.xmin::text)".to_owned()
        };
        let filter = filter(class);
        // A marker represents an empty but successfully read catalog. Each
        // following row is fetched through the same snapshot cursor; a large
        // unrelated catalog must not become one oversized JSON value.
        parts.push(format!("SELECT '{class}' AS part, NULL::text AS body, NULL::text AS witness UNION ALL SELECT '{class}', {row}::text, {witness}::text FROM pg_catalog.{class} c {filter}"));
    }
    Ok(parts.join("\nUNION ALL\n"))
}

pub(super) fn witness() -> String {
    properties::CLASSES.iter().filter(|&&class| class != "pg_roles").map(|class| {
        format!("SELECT '{class}' AS part, NULL::text AS witness UNION ALL SELECT '{class}', pg_catalog.jsonb_build_array(c.ctid::text, c.xmin::text)::text FROM pg_catalog.{class} c {}", filter(class))
    }).collect::<Vec<_>>().join("\nUNION ALL\n")
}

fn filter(class: &str) -> &'static str {
    match class {
        "pg_database" => "WHERE c.datname = current_database()",
        "pg_db_role_setting" => {
            "WHERE c.setdatabase = 0 OR c.setdatabase = (SELECT oid FROM pg_catalog.pg_database WHERE datname = current_database())"
        }
        "pg_shdepend" => {
            "WHERE c.dbid = (SELECT oid FROM pg_catalog.pg_database WHERE datname = current_database()) OR (c.dbid = 0 AND (c.classid <> 'pg_catalog.pg_database'::regclass OR c.objid = (SELECT oid FROM pg_catalog.pg_database WHERE datname = current_database())))"
        }
        _ => "",
    }
}

fn renderer(class: &str, field: &str) -> Result<&'static str> {
    Ok(match (class, field) {
        ("pg_class", "relpartbound") => "pg_catalog.pg_get_expr(c.relpartbound, c.oid, false)",
        ("pg_type", "typdefaultbin") => "pg_catalog.pg_get_expr(c.typdefaultbin, 0, false)",
        ("pg_proc", "proargdefaults") => "pg_catalog.pg_get_expr(c.proargdefaults, 0, false)",
        ("pg_proc", "prosqlbody") => "pg_catalog.pg_get_functiondef(c.oid)",
        ("pg_rewrite", "ev_action" | "ev_qual") => "pg_catalog.pg_get_ruledef(c.oid, false)",
        ("pg_attrdef", "adbin") => "pg_catalog.pg_get_expr(c.adbin, c.adrelid, false)",
        ("pg_constraint", "conbin") => "pg_catalog.pg_get_constraintdef(c.oid, false)",
        ("pg_index", "indexprs" | "indpred") => {
            "pg_catalog.pg_get_indexdef(c.indexrelid, 0, false)"
        }
        ("pg_partitioned_table", "partexprs") => {
            "pg_catalog.pg_get_expr(c.partexprs, c.partrelid, false)"
        }
        ("pg_policy", "polqual") => "pg_catalog.pg_get_expr(c.polqual, c.polrelid, false)",
        ("pg_policy", "polwithcheck") => {
            "pg_catalog.pg_get_expr(c.polwithcheck, c.polrelid, false)"
        }
        ("pg_trigger", "tgqual") => "pg_catalog.pg_get_triggerdef(c.oid, false)",
        _ => return Err(Uncovered::Definition),
    })
}

fn complete_definition(class: &str) -> Option<&'static str> {
    match class {
        "pg_proc" => Some(
            "CASE WHEN c.prokind IN ('f', 'p') THEN pg_catalog.pg_get_functiondef(c.oid) ELSE NULL END",
        ),
        "pg_rewrite" => Some("pg_catalog.pg_get_ruledef(c.oid, false)"),
        "pg_constraint" => Some("pg_catalog.pg_get_constraintdef(c.oid, false)"),
        "pg_index" => Some("pg_catalog.pg_get_indexdef(c.indexrelid, 0, false)"),
        "pg_trigger" => Some("pg_catalog.pg_get_triggerdef(c.oid, false)"),
        _ => None,
    }
}

fn provider_version(class: &str) -> Option<&'static str> {
    match class {
        "pg_collation" => Some("pg_collation_actual_version"),
        "pg_database" => Some("pg_database_collation_actual_version"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_expression_property_has_an_engine_renderer_and_unknown_versions_refuse() {
        for major in [16, 18] {
            assert!(batch(major, None).is_ok());
            assert!(batch(major, Some(&Selection::default())).is_ok());
        }
        assert_eq!(
            batch(17, Some(&Selection::default())),
            Err(Uncovered::Version)
        );
        assert_eq!(
            renderer("pg_proc", "future_tree"),
            Err(Uncovered::Definition)
        );
    }
}
