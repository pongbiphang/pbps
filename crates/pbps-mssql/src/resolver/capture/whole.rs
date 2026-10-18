//! The catalog views read whole, filed under what each row describes and
//! translated into compared properties (#2153).
//!
//! A row becomes a map of its columns. A column a rule skips is left out, an
//! id is replaced by the logical identity it names, and any other column is
//! kept as the engine reports it (`columns.rs`). These maps are what a
//! capture compares; the typed reads beside them are what its logic reads.

use super::Uncovered;
use super::columns::{self, Rule};
use super::read::{Catalog, WholeRow};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The views whose rows each describe one object, keyed by `object_id`.
const PER_OBJECT: &[&str] = &[
    "periods",
    "change_tracking_tables",
    "filetables",
    "fulltext_indexes",
    "edge_constraints",
    "security_policies",
    "objects",
    "tables",
    "views",
    "procedures",
    "sql_modules",
    "synonyms",
    "sequences",
    "check_constraints",
    "default_constraints",
    "key_constraints",
    "foreign_keys",
    "triggers",
];

/// The views whose rows each describe one column.
const PER_COLUMN: &[&str] = &[
    "columns",
    "computed_columns",
    "identity_columns",
    "masked_columns",
];

/// The views whose rows list something of one object, keyed by `object_id`,
/// each with the column that orders them.
const OBJECT_LISTS: &[(&str, &str)] = &[
    ("trigger_events", "type"),
    ("fulltext_index_columns", "column_id"),
    ("edge_constraint_clauses", "clause_number"),
    ("security_predicates", "security_predicate_id"),
];

/// The views whose rows describe one index, keyed by `(object_id,
/// index_id)`: one row each, or a list in the order of the column given.
const INDEX_ROWS: &[(&str, Option<&str>)] = &[
    ("xml_indexes", None),
    ("spatial_indexes", None),
    ("json_indexes", None),
    ("vector_indexes", None),
    ("hash_indexes", None),
    ("spatial_index_tessellations", None),
    ("selective_xml_index_paths", Some("path_id")),
    ("selective_xml_index_namespaces", Some("uri")),
    ("json_index_paths", Some("path")),
    ("partitions", Some("partition_number")),
];

fn number(row: &WholeRow, column: &str) -> Option<i64> {
    row.get(column)
        .and_then(Option::as_deref)
        .and_then(|value| value.parse().ok())
}

pub(super) struct Whole<'a> {
    catalog: &'a Catalog,
    /// The object ids the rows translated since the last take named.
    referenced: std::cell::RefCell<std::collections::BTreeSet<i64>>,
    per_object: BTreeMap<&'static str, BTreeMap<i64, &'a WholeRow>>,
    per_column: BTreeMap<&'static str, BTreeMap<(i64, i64), &'a WholeRow>>,
    indexes: BTreeMap<i64, Vec<&'a WholeRow>>,
    object_lists: BTreeMap<&'static str, BTreeMap<i64, Vec<&'a WholeRow>>>,
    index_rows: BTreeMap<&'static str, BTreeMap<(i64, i64), Vec<&'a WholeRow>>>,
    index_columns: BTreeMap<(i64, i64), Vec<&'a WholeRow>>,
    foreign_key_columns: BTreeMap<i64, Vec<&'a WholeRow>>,
    parameters: BTreeMap<i64, Vec<&'a WholeRow>>,
    types: BTreeMap<&'static str, BTreeMap<i64, &'a WholeRow>>,
    column_names: BTreeMap<(i64, i64), &'a str>,
    /// An index's name, `None` for a heap, which has none: any text could
    /// also be an index's name, so a heap gets no stand-in.
    index_names: BTreeMap<(i64, i64), Option<String>>,
    /// A table type's own table object, by its id: the type's `user_type_id`.
    table_objects: BTreeMap<i64, i64>,
}

impl<'a> Whole<'a> {
    pub(super) fn new(catalog: &'a Catalog) -> Self {
        let rows = |view: &str| catalog.whole.get(view).map(Vec::as_slice).unwrap_or(&[]);
        let mut whole = Self {
            catalog,
            referenced: Default::default(),
            per_object: BTreeMap::new(),
            per_column: BTreeMap::new(),
            indexes: BTreeMap::new(),
            object_lists: BTreeMap::new(),
            index_rows: BTreeMap::new(),
            index_columns: BTreeMap::new(),
            foreign_key_columns: BTreeMap::new(),
            parameters: BTreeMap::new(),
            types: BTreeMap::new(),
            column_names: BTreeMap::new(),
            index_names: BTreeMap::new(),
            table_objects: BTreeMap::new(),
        };
        for &view in PER_OBJECT {
            let filed = whole.per_object.entry(view).or_default();
            for row in rows(view) {
                if let Some(id) = number(row, "object_id") {
                    filed.insert(id, row);
                }
            }
        }
        for &view in PER_COLUMN {
            let filed = whole.per_column.entry(view).or_default();
            for row in rows(view) {
                if let (Some(object), Some(column)) =
                    (number(row, "object_id"), number(row, "column_id"))
                {
                    filed.insert((object, column), row);
                }
            }
        }
        for row in rows("columns") {
            if let (Some(object), Some(column), Some(Some(name))) = (
                number(row, "object_id"),
                number(row, "column_id"),
                row.get("name"),
            ) {
                whole.column_names.insert((object, column), name.as_str());
            }
        }
        for row in rows("indexes") {
            if let (Some(object), Some(index)) = (number(row, "object_id"), number(row, "index_id"))
            {
                whole.indexes.entry(object).or_default().push(row);
                let name = row.get("name").cloned().flatten();
                whole.index_names.insert((object, index), name);
            }
        }
        for &(view, _) in OBJECT_LISTS {
            let filed = whole.object_lists.entry(view).or_default();
            for row in rows(view) {
                if let Some(object) = number(row, "object_id") {
                    filed.entry(object).or_default().push(row);
                }
            }
        }
        for &(view, _) in INDEX_ROWS {
            let filed = whole.index_rows.entry(view).or_default();
            for row in rows(view) {
                if let (Some(object), Some(index)) =
                    (number(row, "object_id"), number(row, "index_id"))
                {
                    filed.entry((object, index)).or_default().push(row);
                }
            }
        }
        for row in rows("index_columns") {
            if let (Some(object), Some(index)) = (number(row, "object_id"), number(row, "index_id"))
            {
                whole
                    .index_columns
                    .entry((object, index))
                    .or_default()
                    .push(row);
            }
        }
        for row in rows("foreign_key_columns") {
            if let Some(constraint) = number(row, "constraint_object_id") {
                whole
                    .foreign_key_columns
                    .entry(constraint)
                    .or_default()
                    .push(row);
            }
        }
        for row in rows("parameters") {
            if let Some(object) = number(row, "object_id") {
                whole.parameters.entry(object).or_default().push(row);
            }
        }
        for view in ["types", "table_types"] {
            let filed = whole.types.entry(view).or_default();
            for row in rows(view) {
                if let Some(id) = number(row, "user_type_id") {
                    filed.insert(id, row);
                }
            }
        }
        for row in rows("table_types") {
            if let (Some(object), Some(id)) = (
                number(row, "type_table_object_id"),
                number(row, "user_type_id"),
            ) {
                whole.table_objects.insert(object, id);
            }
        }
        whole
    }

    fn unknown(view: &str) -> Uncovered {
        Uncovered::class(
            &format!("sys.{view}"),
            "a catalog row names a row the capture cannot identify",
        )
    }

    fn object_identity(
        &self,
        view: &str,
        id: i64,
        index: &super::select::Lookup<'_>,
    ) -> Result<Value, Uncovered> {
        if id == 0 {
            return Ok(Value::Null);
        }
        if let Some(found) = index.object_by_id(id) {
            return Ok(json!(found?));
        }
        if let Some(user_type) = self.table_objects.get(&id) {
            return Ok(json!(index.type_identity(*user_type)?));
        }
        // A constraint on a table type's own table object, such as a column's
        // DEFAULT, is the engine's and named by it: it stands as its type's,
        // whose structure holds its definition.
        if let Some((user_type, _)) = self
            .catalog
            .table_type_objects
            .iter()
            .find(|(_, objects)| objects.contains(&id))
        {
            return Ok(json!({"table_type_constraint": index.type_identity(*user_type)?}));
        }
        Err(Self::unknown(view))
    }

    /// One row as compared properties.
    pub(super) fn translate(
        &self,
        view: &str,
        row: &WholeRow,
        index: &super::select::Lookup<'_>,
    ) -> Result<Value, Uncovered> {
        let mut out = Map::new();
        for (column, value) in row {
            let Some(rule) = columns::rule(view, column) else {
                if !columns::comparable_raw(column) {
                    return Err(Uncovered::class(
                        &format!("sys.{view}.{column}"),
                        "a catalog column holds an id no rule translates",
                    ));
                }
                out.insert(column.clone(), json!(value));
                continue;
            };
            match rule {
                Rule::Skip(_) => continue,
                Rule::AsReported(_) => {
                    out.insert(column.clone(), json!(value));
                    continue;
                }
                Rule::Principal
                | Rule::Schema
                | Rule::Object
                | Rule::Type
                | Rule::Column(_)
                | Rule::Index(_)
                | Rule::DataSpace
                | Rule::XmlCollection
                | Rule::EncryptionKey
                | Rule::FullText(_)
                | Rule::ExecuteAs => {}
            }
            let Some(text) = value.as_deref() else {
                // NULL names nothing, except as a module's `EXECUTE AS`.
                let translated = if rule == Rule::ExecuteAs {
                    json!("caller")
                } else {
                    Value::Null
                };
                out.insert(column.clone(), translated);
                continue;
            };
            let id: i64 = text.parse().map_err(|_| Self::unknown(view))?;
            let translated = match rule {
                Rule::Skip(_) | Rule::AsReported(_) => unreachable!("handled above"),
                Rule::Principal => json!(index.principal(id)?),
                Rule::Schema => json!(index.schema_identity(id)?),
                Rule::Object => {
                    self.referenced.borrow_mut().insert(id);
                    self.object_identity(view, id, index)?
                }
                Rule::Type => json!(index.type_identity(id)?),
                Rule::Column(source) => match (id, number(row, source)) {
                    (0, _) => Value::Null,
                    (_, Some(object)) => json!(
                        *self
                            .column_names
                            .get(&(object, id))
                            .ok_or_else(|| Self::unknown(view))?
                    ),
                    (_, None) => return Err(Self::unknown(view)),
                },
                Rule::Index(source) => {
                    let object = number(row, source).ok_or_else(|| Self::unknown(view))?;
                    json!(
                        self.index_names
                            .get(&(object, id))
                            .ok_or_else(|| Self::unknown(view))?
                    )
                }
                Rule::DataSpace => match id {
                    0 => Value::Null,
                    _ => json!(
                        self.catalog
                            .data_spaces
                            .get(&id)
                            .ok_or_else(|| Self::unknown(view))?
                    ),
                },
                Rule::XmlCollection => match id {
                    0 => Value::Null,
                    _ => json!(
                        self.catalog
                            .xml_collections
                            .get(&id)
                            .ok_or_else(|| Self::unknown(view))?
                    ),
                },
                Rule::FullText(kind) => match id {
                    // A stoplist's 0 is the system one, and NULL is none
                    // (`STOPLIST = OFF`): two different indexes.
                    0 if kind == "stoplist" => json!({"system": true}),
                    0 => Value::Null,
                    _ => json!(
                        self.catalog
                            .fulltext
                            .get(&(kind, id))
                            .ok_or_else(|| Self::unknown(view))?
                    ),
                },
                Rule::EncryptionKey => json!(
                    self.catalog
                        .encryption_keys
                        .get(&id)
                        .ok_or_else(|| Self::unknown(view))?
                ),
                Rule::ExecuteAs => match id {
                    -2 => json!("owner"),
                    _ => json!(index.principal(id)?),
                },
            };
            out.insert(column.clone(), translated);
        }
        Ok(Value::Object(out))
    }

    fn ordered(
        &self,
        view: &str,
        rows: Option<&Vec<&WholeRow>>,
        order: &str,
        index: &super::select::Lookup<'_>,
    ) -> Result<Value, Uncovered> {
        let mut rows: Vec<&WholeRow> = rows.cloned().unwrap_or_default();
        rows.sort_by_key(|row| number(row, order));
        Ok(Value::Array(
            rows.into_iter()
                .map(|row| self.translate(view, row, index))
                .collect::<Result<_, _>>()?,
        ))
    }

    /// The indexes of one object, by name, each with its columns in order.
    fn indexes_of(
        &self,
        object: i64,
        index: &super::select::Lookup<'_>,
    ) -> Result<Value, Uncovered> {
        // A list in name order, the heap (no name) first: a map keyed by a
        // stand-in name could collide with a real index of that name.
        let mut out = Vec::new();
        for row in self.indexes.get(&object).into_iter().flatten() {
            let id = number(row, "index_id").ok_or_else(|| Self::unknown("indexes"))?;
            let name = self
                .index_names
                .get(&(object, id))
                .ok_or_else(|| Self::unknown("indexes"))?;
            out.push((
                name.clone(),
                json!({
                    "name": name,
                    "kinds": self.index_kinds(object, id, index)?,
                    "index": self.translate("indexes", row, index)?,
                    "columns": self.ordered(
                        "index_columns",
                        self.index_columns.get(&(object, id)),
                        "index_column_id",
                        index,
                    )?,
                }),
            ));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Value::Array(
            out.into_iter().map(|(_, value)| value).collect(),
        ))
    }

    /// What the per-index views hold about one index, by view.
    fn index_kinds(
        &self,
        object: i64,
        id: i64,
        index: &super::select::Lookup<'_>,
    ) -> Result<Value, Uncovered> {
        let mut out = Map::new();
        for &(view, order) in INDEX_ROWS {
            let Some(rows) = self
                .index_rows
                .get(view)
                .and_then(|rows| rows.get(&(object, id)))
            else {
                continue;
            };
            let value = match order {
                Some(order) => self.ordered(view, Some(rows), order, index)?,
                None => match rows.as_slice() {
                    [row] => self.translate(view, row, index)?,
                    _ => return Err(Self::unknown(view)),
                },
            };
            out.insert(format!("sys.{view}"), value);
        }
        Ok(Value::Object(out))
    }

    /// The object ids named by the rows translated since the last call.
    pub(super) fn take_referenced(&self) -> std::collections::BTreeSet<i64> {
        std::mem::take(&mut *self.referenced.borrow_mut())
    }

    /// The security policies whose predicates name `object` as their target:
    /// they filter or block what its rows return, so they join its closure.
    pub(super) fn policies_on(&self, object: i64) -> Vec<i64> {
        self.object_lists
            .get("security_predicates")
            .into_iter()
            .flat_map(|rows| rows.iter())
            .filter(|(_, rows)| {
                rows.iter()
                    .any(|row| number(row, "target_object_id") == Some(object))
            })
            .map(|(policy, _)| *policy)
            .collect()
    }

    /// Everything the whole views hold about one object, by view.
    pub(super) fn object(
        &self,
        id: i64,
        index: &super::select::Lookup<'_>,
    ) -> Result<BTreeMap<String, Value>, Uncovered> {
        let mut out = BTreeMap::new();
        for &view in PER_OBJECT {
            if let Some(row) = self.per_object.get(view).and_then(|rows| rows.get(&id)) {
                out.insert(format!("sys.{view}"), self.translate(view, row, index)?);
            }
        }
        if self.indexes.contains_key(&id) {
            out.insert("sys.indexes".into(), self.indexes_of(id, index)?);
        }
        for &(view, order) in OBJECT_LISTS {
            if let Some(rows) = self.object_lists.get(view).and_then(|rows| rows.get(&id)) {
                out.insert(
                    format!("sys.{view}"),
                    self.ordered(view, Some(rows), order, index)?,
                );
            }
        }
        if self.parameters.contains_key(&id) {
            out.insert(
                "sys.parameters".into(),
                self.ordered(
                    "parameters",
                    self.parameters.get(&id),
                    "parameter_id",
                    index,
                )?,
            );
        }
        if self.foreign_key_columns.contains_key(&id) {
            out.insert(
                "sys.foreign_key_columns".into(),
                self.ordered(
                    "foreign_key_columns",
                    self.foreign_key_columns.get(&id),
                    "constraint_column_id",
                    index,
                )?,
            );
        }
        Ok(out)
    }

    /// The `default_object_id` and `rule_object_id` a column's or a type's
    /// row names, each with its column, when nonzero.
    pub(super) fn bound_by(&self, row: &WholeRow) -> Vec<(&'static str, i64)> {
        ["default_object_id", "rule_object_id"]
            .into_iter()
            .filter_map(|column| {
                number(row, column)
                    .filter(|id| *id != 0)
                    .map(|id| (column, id))
            })
            .collect()
    }

    /// The column row of `(object, column)` in `sys.columns`.
    pub(super) fn column_row(&self, object: i64, column: i64) -> Option<&'a WholeRow> {
        self.per_column
            .get("columns")
            .and_then(|rows| rows.get(&(object, column)))
            .copied()
    }

    /// The row of a type in `sys.types`.
    pub(super) fn type_row(&self, id: i64) -> Option<&'a WholeRow> {
        self.types
            .get("types")
            .and_then(|rows| rows.get(&id))
            .copied()
    }

    /// Everything the whole views hold about one column, by view.
    pub(super) fn column(
        &self,
        object: i64,
        column: i64,
        index: &super::select::Lookup<'_>,
    ) -> Result<BTreeMap<String, Value>, Uncovered> {
        let mut out = BTreeMap::new();
        for &view in PER_COLUMN {
            if let Some(row) = self
                .per_column
                .get(view)
                .and_then(|rows| rows.get(&(object, column)))
            {
                out.insert(format!("sys.{view}"), self.translate(view, row, index)?);
            }
        }
        Ok(out)
    }

    /// Everything the whole views hold about one type, by view: a table
    /// type's own table object's columns and indexes included.
    pub(super) fn type_rows(
        &self,
        id: i64,
        index: &super::select::Lookup<'_>,
    ) -> Result<BTreeMap<String, Value>, Uncovered> {
        let mut out = BTreeMap::new();
        for view in ["types", "table_types"] {
            if let Some(row) = self.types.get(view).and_then(|rows| rows.get(&id)) {
                out.insert(format!("sys.{view}"), self.translate(view, row, index)?);
            }
        }
        if let Some((&object, _)) = self.table_objects.iter().find(|(_, user)| **user == id) {
            let mut columns: Vec<_> = self
                .column_names
                .keys()
                .filter(|(owner, _)| *owner == object)
                .map(|(_, column)| *column)
                .collect();
            columns.sort();
            let columns = columns
                .into_iter()
                .map(|column| Ok(json!(self.column(object, column, index)?)))
                .collect::<Result<Vec<_>, Uncovered>>()?;
            out.insert("columns".into(), Value::Array(columns));
            out.insert("sys.indexes".into(), self.indexes_of(object, index)?);
        }
        Ok(out)
    }
}
