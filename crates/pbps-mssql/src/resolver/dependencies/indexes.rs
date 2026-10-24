//! Readable view indexes retain name-hint and NOEXPAND binding (DEC-2323.1).

use super::{Limitation, ObjectName};
use crate::resolver::scratch::ident;
use pbps_db::{Conn, DbError};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Index {
    pub id: i32,
    pub name: Option<String>,
    pub kind: u8,
    pub unique: bool,
    pub disabled: bool,
    pub has_filter: bool,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(super) struct Column {
    pub name: Option<String>,
    pub ordinal: u8,
    pub descending: bool,
    pub included: bool,
}

impl Index {
    fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("index_id {}", self.id))
    }
    fn same_shape(&self, other: &Self) -> bool {
        self.name == other.name
            && self.kind == other.kind
            && self.unique == other.unique
            && self.disabled == other.disabled
            && self.has_filter == other.has_filter
            && self.columns == other.columns
    }
}

pub(super) struct Failure {
    index: String,
    error: DbError,
}

impl Failure {
    pub fn limitation(self, object: &ObjectName) -> Limitation {
        Limitation::Index {
            object: object.clone(),
            index: self.index,
            code: self.error.server_error_code(),
            reason: self.error.to_string(),
            cause: None,
        }
    }
}

fn sql(object: &ObjectName, index: &Index) -> Result<String, DbError> {
    let refuse = |why: &str| DbError::Refused(format!("view index reconstruction: {why}"));
    let name = index
        .name
        .as_deref()
        .ok_or_else(|| refuse("index name is unreadable"))?;
    let kind = match index.kind {
        1 => "CLUSTERED",
        2 => "NONCLUSTERED",
        _ => return Err(refuse("index kind is not a B-tree")),
    };
    if index.has_filter {
        return Err(refuse(
            "filtered view indexes have no reconstruction template",
        ));
    }
    let mut keys = Vec::new();
    let mut included = Vec::new();
    for column in &index.columns {
        let name = ident(
            column
                .name
                .as_deref()
                .ok_or_else(|| refuse("index column is unreadable"))?,
        );
        if column.included {
            included.push(name);
        } else {
            if usize::from(column.ordinal) != keys.len() + 1 {
                return Err(refuse("index key order is incomplete"));
            }
            keys.push(format!(
                "{name} {}",
                if column.descending { "DESC" } else { "ASC" }
            ));
        }
    }
    if keys.is_empty() {
        return Err(refuse("index keys are unreadable"));
    }
    Ok(format!(
        "CREATE {}{kind} INDEX {} ON {}.{}({}){};",
        if index.unique { "UNIQUE " } else { "" },
        ident(name),
        ident(&object.schema),
        ident(&object.name),
        keys.join(", "),
        if included.is_empty() {
            String::new()
        } else {
            format!(" INCLUDE ({})", included.join(", "))
        }
    ))
}

pub(super) async fn create(
    conn: &mut Conn,
    object: &ObjectName,
    indexes: &[Index],
) -> Result<(), Failure> {
    let mut ordered: Vec<_> = indexes.iter().collect();
    ordered.sort_by_key(|i| (!(i.kind == 1 && i.unique), i.kind, &i.name));
    for index in &ordered {
        let result = async {
            conn.execute("SET ANSI_NULLS ON; SET QUOTED_IDENTIFIER ON; SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; SET ARITHABORT ON; SET CONCAT_NULL_YIELDS_NULL ON; SET NUMERIC_ROUNDABORT OFF;").await?;
            conn.execute(&sql(object,index)?).await
        }.await;
        if let Err(error) = result {
            return Err(Failure {
                index: index.label(),
                error,
            });
        }
    }
    // A disabled clustered view index would prevent later nonclustered
    // creation, so replay the disabled state after all indexes exist.
    for index in ordered.into_iter().filter(|i| i.disabled) {
        let statement = format!(
            "ALTER INDEX {} ON {}.{} DISABLE;",
            ident(index.name.as_deref().expect("created named index")),
            ident(&object.schema),
            ident(&object.name)
        );
        if let Err(error) = conn.execute(&statement).await {
            return Err(Failure {
                index: index.label(),
                error,
            });
        }
    }
    Ok(())
}

pub(super) fn difference(
    object: &ObjectName,
    expected: &[Index],
    observed: &[Index],
) -> Option<Limitation> {
    let differing = expected
        .iter()
        .find(|i| i.name.is_none() || !observed.iter().any(|o| i.same_shape(o)))
        .or_else(|| {
            observed
                .iter()
                .find(|o| !expected.iter().any(|i| i.same_shape(o)))
        });
    differing.map(|index| Limitation::Index {
        object: object.clone(),
        index: index.label(),
        code: None,
        reason: "the companion view index set differs from the readable catalog".into(),
        cause: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        serde_json::from_value(serde_json::json!({"id":7,"name":"i]","kind":2,"unique":true,"disabled":false,"has_filter":false,
            "columns":[{"name":"a]","ordinal":1,"descending":true,"included":false},{"name":"b","ordinal":0,"descending":false,"included":true}]})).unwrap()
    }
    #[test]
    fn index_names_keys_and_includes_bind_while_incomplete_shapes_refuse() {
        let object = ObjectName {
            database: "db".into(),
            schema: "s]".into(),
            name: "v]".into(),
        };
        let mut i = index();
        assert_eq!(
            sql(&object, &i).unwrap(),
            "CREATE UNIQUE NONCLUSTERED INDEX [i]]] ON [s]]].[v]]]([a]]] DESC) INCLUDE ([b]);"
        );
        assert!(difference(&object, &[i.clone()], &[]).is_some());
        let mut other = i.clone();
        other.id = 1;
        assert!(difference(&object, &[i.clone()], &[other.clone()]).is_none());
        other.columns[0].descending = false;
        assert!(difference(&object, &[i.clone()], &[other]).is_some());
        i.name = None;
        assert!(sql(&object, &i).is_err());
        i = index();
        i.columns.clear();
        assert!(sql(&object, &i).is_err());
        i = index();
        i.columns[0].ordinal = 2;
        assert!(sql(&object, &i).is_err());
    }
}
