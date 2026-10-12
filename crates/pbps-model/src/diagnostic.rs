//! Rejected declaration sources travel beside the recovery schema, never in
//! checked columns or persisted state. Consumers still match `ValueSource`
//! exhaustively; this view only supplies the information recovery retained.

use std::collections::BTreeMap;

use crate::{Column, TableName, ValueSource};

pub type SourceConflicts = BTreeMap<TableName, BTreeMap<String, Vec<ValueSource>>>;

/// The sources actually supplied for each column of one diagnostic table.
/// With no recovery entry, the checked column supplies its sole source.
#[derive(Clone, Copy, Debug, Default)]
pub struct ColumnSources<'a> {
    conflicts: Option<&'a BTreeMap<String, Vec<ValueSource>>>,
}

impl<'a> ColumnSources<'a> {
    pub fn new(conflicts: Option<&'a BTreeMap<String, Vec<ValueSource>>>) -> Self {
        Self { conflicts }
    }

    pub fn get<'b>(&'b self, name: &str, column: &'b Column) -> &'b [ValueSource] {
        self.conflicts
            .and_then(|conflicts| conflicts.get(name))
            .map(Vec::as_slice)
            .unwrap_or_else(|| std::slice::from_ref(&column.value_source))
    }

    pub fn conflicted_columns(self) -> impl Iterator<Item = &'a str> {
        self.conflicts
            .into_iter()
            .flat_map(|c| c.keys().map(String::as_str))
    }
}
