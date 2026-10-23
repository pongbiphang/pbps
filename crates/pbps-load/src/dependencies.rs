use std::{collections::BTreeMap, fmt::Write, path::Path};

use pbps_model::dependency::{Dependency, DependencyColumn, DependencyName};

use crate::{LoadError, dto::DependencyDto, fmt::scalar};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedDependency {
    pub name: DependencyName,
    pub declaration: Dependency,
}

pub fn load_dependency_str(path: &Path, text: &str) -> Result<LoadedDependency, Vec<LoadError>> {
    let probe: crate::dto::KindProbe = serde_saphyr::from_str(text).map_err(|e| {
        vec![LoadError::Yaml {
            path: path.into(),
            message: e.to_string(),
        }]
    })?;
    let identity = probe.dependency.unwrap_or_default();
    let fail = |message| {
        vec![LoadError::Yaml {
            path: path.into(),
            message: format!(
                "dependency `{}`: {message}",
                pbps_model::escape::Escaped(&identity)
            ),
        }]
    };
    let dto: DependencyDto = serde_saphyr::from_str(text).map_err(|e| fail(e.to_string()))?;
    let name = dto.dependency;
    if dto.columns.is_empty() {
        return Err(fail(format!(
            "dependency `{}` must declare nonempty columns",
            name.shown()
        )));
    }
    let mut columns = BTreeMap::new();
    for (column, definition) in dto.columns {
        pbps_model::check_segment(column.raw()).map_err(|e| fail(e.to_string()))?;
        let ty = definition
            .ty
            .parse()
            .map_err(|e: pbps_model::TypeParseError| {
                fail(format!(
                    "dependency `{}` column `{}`: {e}",
                    name.shown(),
                    column.shown()
                ))
            })?;
        columns.insert(
            column,
            DependencyColumn {
                ty,
                nullable: definition.nullable,
            },
        );
    }
    Ok(LoadedDependency {
        name,
        declaration: Dependency {
            kind: dto.kind,
            columns,
        },
    })
}

pub fn render_dependency(name: &DependencyName, declaration: &Dependency) -> String {
    let mut out = format!(
        "dependency: {}\nkind: {}\ncolumns:\n",
        scalar(&name.raw()),
        declaration.kind.as_str()
    );
    for (name, column) in &declaration.columns {
        let _ = writeln!(out, "  {}:", scalar(name.raw()));
        let _ = writeln!(out, "    type: {}", scalar(&column.ty.raw().to_string()));
        if !column.nullable {
            out.push_str("    nullable: false\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_and_views_load_with_nullable_defaults_and_round_trip() {
        for kind in ["table", "view"] {
            let text = format!(
                "dependency: otherdb.dbo.customers\nkind: {kind}\ncolumns:\n  id: {{type: INT, nullable: false}}\n  name: {{type: nvarchar(100)}}\n"
            );
            let loaded = load_dependency_str(Path::new("dependency.yml"), &text).unwrap();
            assert!(loaded.declaration.columns["name"].nullable);
            assert!(!loaded.declaration.columns["id"].nullable);
            let rendered = render_dependency(&loaded.name, &loaded.declaration);
            assert_eq!(
                loaded,
                load_dependency_str(Path::new("dependency.yml"), &rendered).unwrap()
            );
        }
    }

    #[test]
    fn invalid_dependency_shapes_are_named_and_refused() {
        for text in [
            "dependency: dbo.t\nkind: table\ncolumns: {id: {type: int}}",
            "dependency: a.b.c.d\nkind: table\ncolumns: {id: {type: int}}",
            "dependency: a.b.c\nkind: procedure\ncolumns: {id: {type: int}}",
            "dependency: a.b.c\nkind: table\ncolumns: {}",
            "dependency: a.b.c\nkind: table\ncolumns: {id: {type: 'int('}}",
            "dependency: a.b.c\nkind: table\ncolumns: {id: {type: int, default: 1}}",
        ] {
            let errors = load_dependency_str(Path::new("dependency.yml"), text).unwrap_err();
            assert!(format!("{errors:?}").contains("dependency"), "{errors:?}");
        }
    }

    #[test]
    fn dependencies_do_not_enter_schema_or_ids_and_duplicates_refuse() {
        let dir =
            std::env::temp_dir().join(format!("pbps-dependencies2322-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = "dependency: otherdb.dbo.t\nkind: table\ncolumns: {id: {type: int}}";
        std::fs::write(dir.join("one.yml"), text).unwrap();
        let loaded = crate::load_schema_dir(&dir).unwrap();
        assert_eq!(loaded.schema, Default::default());
        assert_eq!(loaded.dependencies.len(), 1);
        std::fs::write(dir.join("two.yml"), text).unwrap();
        let error = crate::load_schema_dir(&dir).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(format!("{error:?}").contains("otherdb.dbo.t"));
        assert!(format!("{error:?}").contains("already declared"));
    }

    #[test]
    fn dependencies_and_declared_types_load_together_without_losing_type_intent() {
        let dir = std::env::temp_dir().join(format!(
            "pbps-dependencies-types2322-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dependency = "dependency: otherdb.dbo.t\nkind: table\ncolumns: {id: {type: int}}";
        std::fs::write(dir.join("a-dependency.yml"), dependency).unwrap();
        std::fs::write(
            dir.join("b-enum.yml"),
            "enum: app.mood\nrenamed_from: app.old_mood\nvalues: [active]\n",
        )
        .unwrap();
        std::fs::write(dir.join("c-domain.yml"), "domain: app.label\ntype: text\n").unwrap();
        let loaded = crate::load_schema_dir(&dir).unwrap();
        assert_eq!(loaded.schema.types.len(), 2);
        assert_eq!(loaded.dependencies.len(), 1);
        assert!(loaded.schema.tables.is_empty());
        assert_eq!(
            loaded.intents,
            vec![pbps_model::Intent::RenameType {
                from: "app.old_mood".parse().unwrap(),
                to: "app.mood".parse().unwrap(),
            }]
        );
        std::fs::rename(dir.join("a-dependency.yml"), dir.join("z-dependency.yml")).unwrap();
        assert_eq!(loaded, crate::load_schema_dir(&dir).unwrap());
        std::fs::write(
            dir.join("duplicate-enum.yml"),
            "enum: app.mood\nvalues: [other]\n",
        )
        .unwrap();
        let errors = crate::load_schema_dir(&dir).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(format!("{errors:?}").contains("already declared"));
        assert!(format!("{errors:?}").contains("app.mood"));
    }
}
