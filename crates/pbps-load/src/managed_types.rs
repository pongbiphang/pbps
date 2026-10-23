use std::{collections::BTreeSet, fmt::Write, path::Path};

use pbps_model::{ColumnType, ManagedType, TypeRef};

use crate::{
    LoadError,
    dto::{DomainTypeDto, EnumTypeDto, KindProbe},
    fmt::scalar,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedType {
    pub name: TypeRef,
    pub definition: ManagedType,
}

pub fn load_type_str(path: &Path, text: &str) -> Result<LoadedType, Vec<LoadError>> {
    let fail = |message: String| {
        vec![LoadError::Yaml {
            path: path.into(),
            message,
        }]
    };
    let probe: KindProbe = serde_saphyr::from_str(text).map_err(|e| fail(e.to_string()))?;
    let (name, definition) = match (probe.enumeration, probe.domain) {
        (Some(_), None) => {
            let dto: EnumTypeDto = serde_saphyr::from_str(text).map_err(|e| fail(e.to_string()))?;
            let mut seen = BTreeSet::new();
            for value in &dto.values {
                if !seen.insert(value) {
                    return Err(fail(format!(
                        "enum `{}` declares value `{}` twice",
                        pbps_model::escape::Escaped(&dto.name),
                        pbps_model::escape::Escaped(value)
                    )));
                }
            }
            (dto.name, ManagedType::Enum { values: dto.values })
        }
        (None, Some(_)) => {
            let dto: DomainTypeDto =
                serde_saphyr::from_str(text).map_err(|e| fail(e.to_string()))?;
            let base = dto
                .base
                .parse::<ColumnType>()
                .map_err(|e| fail(e.to_string()))?;
            let checks = dto
                .checks
                .into_iter()
                .map(|(name, expression)| (name.into(), expression))
                .collect();
            let collation = dto
                .collation
                .map(|name| name.parse())
                .transpose()
                .map_err(|e: pbps_model::TypeParseError| fail(e.to_string()))?;
            (
                dto.name,
                ManagedType::Domain {
                    base,
                    not_null: dto.not_null,
                    default: dto.default,
                    checks,
                    collation,
                },
            )
        }
        _ => {
            return Err(fail(
                "a type file declares exactly one of `enum:` or `domain:`".into(),
            ));
        }
    };
    let name = name
        .parse()
        .map_err(|e: pbps_model::TypeParseError| fail(e.to_string()))?;
    Ok(LoadedType { name, definition })
}

pub fn render_type(name: &TypeRef, definition: &ManagedType) -> String {
    let mut out = format!(
        "{}: {}\n",
        definition.kind(),
        scalar(&name.raw().to_string())
    );
    match definition {
        ManagedType::Enum { values } => {
            let values: Vec<_> = values.iter().map(|v| scalar(v)).collect();
            let _ = writeln!(out, "values: [{}]", values.join(", "));
        }
        ManagedType::Domain {
            base,
            not_null,
            default,
            checks,
            collation,
        } => {
            let _ = writeln!(out, "type: {}", scalar(&base.raw().to_string()));
            if *not_null {
                out.push_str("not_null: true\n");
            }
            if let Some(name) = collation {
                let _ = writeln!(out, "collation: {}", scalar(&name.raw().to_string()));
            }
            if let Some(value) = default {
                let _ = writeln!(out, "default: {}", scalar(value));
            }
            if !checks.is_empty() {
                out.push_str("checks:\n");
                for (name, expression) in checks {
                    let _ = writeln!(out, "  {}: {}", scalar(name.raw()), scalar(expression));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_types_round_trip_ordered_labels_and_verbatim_domain_text() {
        for text in [
            "enum: app.mood\nvalues: [sad, 'no', 'null', '0x1f', '', \"line\\nend\"]\n",
            "domain: app.phone\ntype: varchar(20)\nnot_null: true\ncollation: pg_catalog.\"C\"\ndefault: \"'0'\"\nchecks:\n  phone_digits: \"VALUE ~ '^[0-9+ -]+$'\"\n",
        ] {
            let loaded = load_type_str(Path::new("type.yml"), text).unwrap();
            let output = render_type(&loaded.name, &loaded.definition);
            assert_eq!(
                loaded,
                load_type_str(Path::new("type.yml"), &output).unwrap()
            );
        }
    }

    #[test]
    fn invalid_type_shapes_and_duplicate_labels_are_refused() {
        for text in [
            "enum: app.e\nvalues: [a]\nvaluess: [b]\n",
            "enum: app.e\ndomain: app.e\nvalues: [a]\n",
            "enum: app.e\nvalues: [a, a]\n",
            "enum: app.e\ntype: text\nvalues: [a]\n",
            "domain: app.d\ntype: text\nvalues: [a]\n",
            "domain: app.d\n",
            "enum: unqualified\nvalues: []\n",
            "enum: app.e[]\nvalues: []\n",
            "range: app.r\ntype: integer\n",
        ] {
            assert!(
                load_type_str(Path::new("type.yml"), text).is_err(),
                "{text}"
            );
        }
        assert!(load_type_str(Path::new("type.yml"), "enum: app.e\nvalues: []\n").is_ok());
    }

    #[test]
    fn two_type_files_cannot_declare_the_same_identity() {
        let dir = std::env::temp_dir().join(format!("pbps-types1635-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.yml"), "enum: APP.Mood\nvalues: [a]\n").unwrap();
        std::fs::write(dir.join("b.yml"), "domain: app.mood\ntype: text\n").unwrap();
        let errors = crate::load_schema_dir(&dir).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("already declared")),
            "{errors:?}"
        );
    }
}
