//! Standalone sequence declaration files (#2721, DEC-2721.1).
//!
//! One file per sequence, `sequence: app.order_no`, beside the tables. The file says
//! where a restart would begin and how the numbers run; it has no field for where the
//! counter stands, because that is state the database advances on every `nextval`.

use std::path::Path;

use pbps_model::{ColumnType, ObjectName, Sequence, SequenceOptions};

use crate::{LoadError, dto::StandaloneSequenceDto, fmt::scalar};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSequence {
    pub name: ObjectName,
    pub definition: Sequence,
}

/// Loads a `sequence:` file. `path` is only for diagnostics.
///
/// The type defaults to `bigint`, the engine's own default, and the start and the
/// increment to one, as an identity's do. Which types and ranges the engine accepts is the
/// dialect's to say, so a nonsense one loads here and is refused by `validate` with the
/// engine's reason.
pub fn load_sequence_str(path: &Path, text: &str) -> Result<LoadedSequence, Vec<LoadError>> {
    let fail = |message: String| {
        vec![LoadError::Yaml {
            path: path.into(),
            message,
        }]
    };
    let dto: StandaloneSequenceDto =
        serde_saphyr::from_str(text).map_err(|e| fail(e.to_string()))?;
    let name = dto.sequence.parse::<ObjectName>().map_err(|e| {
        fail(format!(
            "sequence `{}`: {e}",
            pbps_model::escape::Escaped(&dto.sequence)
        ))
    })?;
    let ty = match &dto.ty {
        None => ColumnType::simple("bigint"),
        Some(text) => text.parse::<ColumnType>().map_err(|e| {
            fail(format!(
                "sequence `{}`: type `{}`: {e}",
                name.shown(),
                pbps_model::escape::Escaped(text)
            ))
        })?,
    };
    Ok(LoadedSequence {
        name,
        definition: Sequence {
            ty,
            start: dto.start.unwrap_or(1),
            increment: dto.increment.unwrap_or(1),
            options: SequenceOptions {
                min: dto.min,
                max: dto.max,
                cycle: dto.cycle,
                cache: dto.cache,
            },
        },
    })
}

/// The declaration file text of one sequence: what [`load_sequence_str`] reads back.
///
/// Type, start and increment are always written, so a file says what a restart would do
/// without the reader knowing the defaults; the options only when they are set.
pub fn render_sequence(name: &ObjectName, definition: &Sequence) -> String {
    let mut out = format!(
        "sequence: {}\ntype: {}\nstart: {}\nincrement: {}\n",
        scalar(&name.raw().to_string()),
        scalar(&definition.ty.raw().to_string()),
        definition.start,
        definition.increment
    );
    let options = &definition.options;
    if let Some(min) = options.min {
        out.push_str(&format!("min: {min}\n"));
    }
    if let Some(max) = options.max {
        out.push_str(&format!("max: {max}\n"));
    }
    if options.cycle {
        out.push_str("cycle: true\n");
    }
    if let Some(cache) = options.cache {
        out.push_str(&format!("cache: {cache}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> LoadedSequence {
        load_sequence_str(Path::new("s.yml"), text).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn refusal(text: &str) -> String {
        load_sequence_str(Path::new("s.yml"), text)
            .unwrap_err()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// A sequence renders to text that loads back to the same object and renders to the
    /// same bytes, with every option, a descending count and a name that needs quoting.
    #[test]
    fn a_sequence_renders_to_text_that_loads_back_to_the_same_bytes() {
        let full = "sequence: app.order_no\ntype: integer\nstart: 100\nincrement: 5\n\
                    min: 100\nmax: 9999\ncycle: true\ncache: 20\n";
        let loaded = load(full);
        assert_eq!(render_sequence(&loaded.name, &loaded.definition), full);
        assert_eq!(loaded.definition.options.max, Some(9999));

        let plain = "sequence: app.plain\ntype: bigint\nstart: 1\nincrement: 1\n";
        let loaded = load(plain);
        assert_eq!(render_sequence(&loaded.name, &loaded.definition), plain);
        assert!(loaded.definition.options.is_default());

        let down = Sequence {
            ty: ColumnType::simple("smallint"),
            start: -1,
            increment: -3,
            options: SequenceOptions {
                min: Some(-90),
                ..Default::default()
            },
        };
        let name: ObjectName = "app.\"Odd Name\"".parse().unwrap();
        let rendered = render_sequence(&name, &down);
        let back = load(&rendered);
        assert_eq!((back.name, back.definition), (name, down), "{rendered}");
    }

    /// Left out, the type is the engine's default and the start and increment are one:
    /// the loader says what an absent field means, and the formatter then writes them.
    #[test]
    fn an_absent_type_start_and_increment_mean_the_engines_defaults() {
        let loaded = load("sequence: app.s\n");
        assert_eq!(loaded.definition.ty, ColumnType::simple("bigint"));
        assert_eq!(
            (loaded.definition.start, loaded.definition.increment),
            (1, 1)
        );
        assert_eq!(
            render_sequence(&loaded.name, &loaded.definition),
            "sequence: app.s\ntype: bigint\nstart: 1\nincrement: 1\n"
        );
    }

    /// Negatives: an unqualified name, a name with three parts, a type that is not one, a
    /// field the model has nowhere to put (the counter's position above all), and a bound
    /// that is not a number.
    #[test]
    fn a_sequence_file_that_cannot_be_read_is_refused_by_name() {
        for (text, expected) in [
            ("sequence: s\n", "`s`"),
            ("sequence: a.b.c\n", "a.b.c"),
            ("sequence: app.s\ntype: \"\"\n", "app.s"),
            ("sequence: app.s\nlast_value: 7\n", "last_value"),
            ("sequence: app.s\ncurrent_value: 7\n", "current_value"),
            ("sequence: app.s\nmax: lots\n", "max"),
            ("sequence: app.s\nowned_by: app.t.id\n", "owned_by"),
        ] {
            let said = refusal(text);
            assert!(said.contains(expected), "{text:?} -> {said}");
        }
    }
}
