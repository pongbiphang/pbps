use crate::{LoadError, dto::ExtensionDto, fmt::scalar};
use pbps_model::{Extension, ObjectLocalName};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedExtension {
    pub name: ObjectLocalName,
    pub definition: Extension,
}

pub fn load_extension_str(path: &Path, text: &str) -> Result<LoadedExtension, Vec<LoadError>> {
    let fail = |message| {
        vec![LoadError::Yaml {
            path: path.into(),
            message,
        }]
    };
    let dto: ExtensionDto = serde_saphyr::from_str(text).map_err(|e| fail(e.to_string()))?;
    // A source location preserves the distinction String deserialization loses:
    // even a nonnumeric version must be quoted, rather than a YAML implicit scalar.
    let at = dto.version.defined;
    let quoted = text
        .lines()
        .nth(at.line().saturating_sub(1) as usize)
        .and_then(|line| line.chars().nth(at.column().saturating_sub(1) as usize))
        .is_some_and(|c| matches!(c, '\'' | '"'));
    if !quoted || dto.version.value.is_empty() {
        return Err(fail(format!(
            "extension `{}`: version must be an exact, nonempty quoted string",
            dto.extension.shown()
        )));
    }
    for (kind, name) in [
        ("extension", dto.extension.raw()),
        ("schema", dto.schema.as_str()),
    ] {
        if name.is_empty() || name.len() > 63 || name.contains('\0') {
            return Err(fail(format!(
                "extension `{}`: {kind} must be a PostgreSQL identifier of 1–63 bytes without NUL",
                dto.extension.shown()
            )));
        }
    }
    Ok(LoadedExtension {
        name: dto.extension,
        definition: Extension {
            schema: dto.schema,
            version: dto.version.value,
        },
    })
}

pub fn render_extension(name: &ObjectLocalName, definition: &Extension) -> String {
    format!(
        "extension: {}\nschema: {}\nversion: {}\n",
        scalar(name.raw()),
        scalar(&definition.schema),
        serde_json::to_string(&definition.version).expect("a string serializes")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_versions_and_global_names_round_trip_without_rename_intent() {
        for name in ["citext", "uuid-ossp", "schema.word", "name\nline", "名前"] {
            for version in ["1.4", "1.04", "devel+opaque", "yes", "1", "odd\nversion"] {
                let definition = Extension {
                    schema: "extension schema".into(),
                    version: version.into(),
                };
                let rendered = render_extension(&name.into(), &definition);
                let loaded = load_extension_str(Path::new("extension.yml"), &rendered).unwrap();
                assert_eq!(loaded.name.raw(), name);
                assert_eq!(loaded.definition, definition);
                assert_eq!(render_extension(&loaded.name, &loaded.definition), rendered);
                assert!(matches!(
                    crate::load_file_str(Path::new("e.yml"), &rendered).unwrap(),
                    crate::LoadedFile::Extension(_)
                ));
            }
        }
    }

    #[test]
    fn incomplete_implicit_duplicate_and_renamed_packages_are_refused() {
        for version in [
            "1.4", "1", "yes", "opaque", "null", "''", "[]", "{}", "|\n  1.4",
        ] {
            assert!(
                load_extension_str(
                    Path::new("e.yml"),
                    &format!("extension: citext\nschema: public\nversion: {version}\n")
                )
                .is_err(),
                "{version}"
            );
        }
        for text in [
            "extension: citext\nschema: public\n",
            "extension: citext\nversion: '1.4'\n",
            "extension: ''\nschema: public\nversion: '1.4'\n",
            "extension: citext\nschema: ''\nversion: '1.4'\n",
            "extension: citext\nschema: public\nversion: '1.4'\nversion: '1.5'\n",
            "extension: citext\nschema: public\nversion: '1.4'\nrenamed_from: other\n",
            "extension: citext\nschema: public\nversion: '1.4'\nunknown: true\n",
        ] {
            assert!(
                crate::load_file_str(Path::new("e.yml"), text).is_err(),
                "{text}"
            );
        }
    }
}
