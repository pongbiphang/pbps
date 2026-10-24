//! Physical lowering remains behind the canonical/admission rebuild refusal.
use super::*;
use pbps_model::{
    AnalyzeTableTree, BuildPartitionedIndex, CopyTableRows, PartitionIndexDefinition as Definition,
};

pub(super) fn build_partitioned_index(pg: &Postgres, recipe: &BuildPartitionedIndex) -> Sql {
    recipe
        .validate()
        .map_err(|message| invalid(message.into()))?;
    let (index, clause, kind) = match &recipe.definition {
        Definition::Index(index) => (index.clone(), None, None),
        definition @ (Definition::PrimaryKey(_) | Definition::Unique(_)) => {
            let (columns, parameters, clause, kind) = match definition {
                Definition::PrimaryKey(key) => (
                    &key.columns,
                    &key.storage_parameters,
                    primary_key_clause(key)?,
                    "PRIMARY KEY",
                ),
                Definition::Unique(key) => (
                    &key.columns,
                    &key.storage_parameters,
                    unique_clause(recipe.name.raw(), key)?,
                    "UNIQUE",
                ),
                Definition::Index(_) => unreachable!(),
            };
            let index = Index {
                columns: columns
                    .iter()
                    .map(|name| pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column(name.clone()),
                        descending: false,
                        opclass: None,
                    })
                    .collect(),
                include: Vec::new(),
                unique: true,
                filter: None,
                method: pbps_model::IndexMethod::Btree,
                storage_parameters: parameters.clone(),
            };
            (index, Some(clause), Some(kind))
        }
    };
    let parent = &recipe.parent.name;
    // ONLY also produces a valid index for an empty parent on PG16 and PG18.
    let body = match clause {
        Some(clause) => add_key_sql(parent, &clause, true)?,
        None => create_index_on(parent, recipe.name.raw(), &index, Strategy::default(), true)?,
    };
    let mut out = vec![on(pg, parent, &body)?];
    for clone in &recipe.clones {
        let leaf = &clone.leaf.name;
        out.push(on(
            pg,
            leaf,
            &create_index(leaf, clone.name.raw(), &index, Strategy::default())?,
        )?);
        if let Some(kind) = kind {
            out.push(on(
                pg,
                leaf,
                &add_key_sql(
                    leaf,
                    &format!(
                        "CONSTRAINT {} {kind} USING INDEX {}",
                        quote(clone.name.raw())?,
                        quote(clone.name.raw())?
                    ),
                    true,
                )?,
            )?);
        }
        out.push(on(
            pg,
            parent,
            &format!(
                "ALTER INDEX {}.{} ATTACH PARTITION {}.{};",
                quote(&parent.schema)?,
                quote(recipe.name.raw())?,
                quote(&leaf.schema)?,
                quote(clone.name.raw())?
            ),
        )?);
    }
    Ok(out)
}

pub(super) fn copy_table_rows(pg: &Postgres, copy: &CopyTableRows) -> Sql {
    let mut names = Vec::new();
    let mut values = Vec::new();
    for column in &copy.columns {
        if column.target.table != copy.target.name
            || column
                .source
                .as_ref()
                .is_some_and(|s| s.column.table != copy.source.name)
        {
            return Err(invalid("copy column belongs to another relation".into()));
        }
        names.push(quote(column.target.name.raw())?);
        let source = match &column.source {
            Some(source) => format!("src.{}", quote(source.column.name.raw())?),
            None => "NULL".into(),
        };
        let mut value = format!("CAST({source} AS {})", sql_type(&column.cast)?);
        if let Some(collation) = &column.collation {
            value.push_str(&format!(" COLLATE {}", quote(collation.as_str())?));
        }
        values.push(value);
    }
    if names.is_empty() {
        return Err(invalid("rebuild copy has no destination columns".into()));
    }
    // Statement-atomic cap enforcement, without silently truncating (DEC-2298.11).
    let body = format!(
        "DECLARE copied bigint;\nBEGIN\nINSERT INTO {} ({}) SELECT {} FROM {} AS src;\nGET DIAGNOSTICS copied = ROW_COUNT;\nIF copied > {} THEN\nRAISE EXCEPTION USING MESSAGE = {}, ERRCODE = 'P0001';\nEND IF;\nEND;",
        qualified(&copy.target.name)?,
        names.join(", "),
        values.join(", "),
        qualified(&copy.source.name)?,
        copy.max_rows,
        literal(&format!(
            "rebuild copy exceeds approved max_rows ({})",
            copy.max_rows
        ))
    );
    let tag = dollar_tag(&body);
    one(pg, &copy.target.name, format!("DO {tag}\n{body}\n{tag};"))
}

pub(super) fn analyze_table_tree(pg: &Postgres, tree: &AnalyzeTableTree) -> Sql {
    std::iter::once(&tree.parent)
        .chain(&tree.partitions)
        .map(|relation| {
            on(
                pg,
                &relation.name,
                &format!("ANALYZE {};", qualified(&relation.name)?),
            )
        })
        .collect()
}

#[cfg(test)]
mod native;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "../../../pbps-model/tests/support/rebuild.rs"]
pub(super) mod fixture;
