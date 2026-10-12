//! Permission questions follow recorded identities, not a rename destination's occupant.

use pbps_model::{ColumnRef, IdsFile, IdsIndex, ObjectName};

/// Asked once per declared table and column, so both files are indexed once
/// rather than scanned per question (#1870).
pub(super) struct Identity<'a> {
    project: IdsIndex<'a>,
    recorded: &'a IdsFile,
    recorded_names: IdsIndex<'a>,
}

pub(super) enum Table {
    /// This environment has recorded the identity under this physical name.
    Recorded(ObjectName),
    /// No recorded identity establishes whether this name exists yet.
    Unrecorded(ObjectName),
    /// Another recorded identity still occupies the name this declaration reuses.
    Future,
}

impl Table {
    pub fn name(&self) -> Option<&ObjectName> {
        match self {
            Self::Recorded(name) | Self::Unrecorded(name) => Some(name),
            Self::Future => None,
        }
    }
}

impl<'a> Identity<'a> {
    pub fn new(project: &'a IdsFile, recorded: &'a IdsFile) -> Self {
        Self {
            project: project.index(),
            recorded,
            recorded_names: recorded.index(),
        }
    }

    pub fn table(&self, declared: &ObjectName) -> Table {
        if let Some(recorded) = self
            .project
            .table_uid(declared)
            .and_then(|uid| self.recorded.tables.get(uid))
        {
            return Table::Recorded(recorded.clone());
        }
        if self.recorded_names.table_uid(declared).is_some() {
            Table::Future
        } else {
            Table::Unrecorded(declared.clone())
        }
    }

    pub fn column(
        &self,
        declared: &ObjectName,
        current: &ObjectName,
        name: &str,
    ) -> Option<String> {
        let wanted = ColumnRef {
            table: declared.clone(),
            name: name.to_owned(),
        };
        if let Some(recorded) = self
            .project
            .column_uid(&wanted)
            .and_then(|uid| self.recorded.columns.get(uid))
        {
            return (&recorded.table == current).then(|| recorded.name.clone());
        }
        // A newly declared identity cannot borrow the old column's ACL just
        // because the old identity is about to vacate its name.
        if self
            .recorded_names
            .column_uid(&current.column(name))
            .is_some()
        {
            None
        } else {
            Some(name.to_owned())
        }
    }
}
