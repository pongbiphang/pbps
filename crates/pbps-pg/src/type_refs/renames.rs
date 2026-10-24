//! Approved type moves in catalog text. Saved schemas and declared-type pins
//! carry deparsed SQL, not bound trees, so compare their expected text in the
//! spelling of the executed rename prefix (DEC-1635.5).

use std::collections::BTreeMap;

use pbps_dialect::Dialect;
use pbps_model::{ColumnType, ManagedType, Schema, TypeRef, ValueSource};

#[derive(Default)]
pub struct TypeRenames(pub BTreeMap<TypeRef, TypeRef>);

impl TypeRenames {
    pub fn column_type(&self, ty: &mut ColumnType) {
        if let Some(to) = ty.type_ref().and_then(|r| self.0.get(r)) {
            let reference = ColumnType::reference(to.clone());
            *ty = if ty.is_array() {
                reference.into_array()
            } else {
                reference
            };
        }
    }

    /// pg_get_expr renders a bound enum/domain reference as a qualified cast.
    /// Lex the catalog text, never replace substrings: ordinary literals, same-named
    /// calls/columns and the output of an earlier mapping must stay intact.
    pub fn expression(&self, text: &str) -> String {
        let pg = crate::Postgres::new();
        let code = pg.lexicon().header(text);
        let mut replacements = Vec::new();
        let mut cursor = 0;
        while cursor < code.len() {
            let ch = code[cursor..].chars().next().expect("in bounds");
            if let Some((end, value)) = regtype_literal(&pg, text, &code, cursor) {
                if let Ok(mut ty) = value.parse::<ColumnType>()
                    && ty.type_ref().is_some()
                {
                    self.column_type(&mut ty);
                    let reference = ty.type_ref().expect("reference retained");
                    let value = format!(
                        "{}{}",
                        quoted(reference),
                        if ty.is_array() { "[]" } else { "" }
                    );
                    replacements.push((cursor, end, crate::emit::value_literal(&value)));
                }
                cursor = end;
            } else if ch == '"' {
                cursor = identifier(&code, cursor).map_or(code.len(), |(_, end)| end);
            } else if code[cursor..].starts_with("::") {
                let start = skip_space(&code, cursor + 2);
                if let Some((schema, dot)) = identifier(&code, start)
                    && code.as_bytes().get(dot) == Some(&b'.')
                    && let Some((name, end)) = identifier(&code, dot + 1)
                    && let Ok(from) = TypeRef::new(schema, name)
                {
                    let to = self.0.get(&from).unwrap_or(&from);
                    // Canonicalize cast identifiers on both comparison sides:
                    // pg_get_expr's keyword quoting varies by server version.
                    replacements.push((start, end, quoted(to)));
                    cursor = end;
                } else {
                    cursor += 2;
                }
            } else {
                cursor += ch.len_utf8();
            }
        }
        let mut out = text.to_owned();
        for (start, end, to) in replacements.into_iter().rev() {
            out.replace_range(start..end, &to);
        }
        out
    }

    pub fn definition(&self, definition: &mut ManagedType) {
        if let ManagedType::Domain {
            base,
            default,
            checks,
            ..
        } = definition
        {
            self.column_type(base);
            if let Some(text) = default {
                *text = self.expression(text);
            }
            for text in checks.values_mut() {
                *text = self.expression(text);
            }
        }
    }

    /// Only the comparison copy is forwarded. The actual read-back and the
    /// declaration record retain their own spelling (DECISIONS 207–208).
    pub fn schema(&self, schema: &mut Schema) {
        for definition in schema.types.values_mut() {
            self.definition(definition);
        }
        for table in schema.tables.values_mut() {
            for column in table.columns.values_mut() {
                self.column_type(&mut column.ty);
                match &mut column.value_source {
                    ValueSource::Default(text) => *text = self.expression(text),
                    ValueSource::Generated(g) => g.expression = self.expression(&g.expression),
                    ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => {}
                }
            }
            for check in table.checks.values_mut() {
                check.expression = self.expression(&check.expression);
            }
            for index in table.indexes.values_mut() {
                if let Some(text) = &mut index.filter {
                    *text = self.expression(text);
                }
                for column in &mut index.columns {
                    if let pbps_model::IndexKey::Expression(text) = &mut column.key {
                        *text = self.expression(text);
                    }
                }
            }
            for policy in table.row_policies.values_mut() {
                for text in [&mut policy.using, &mut policy.with_check]
                    .into_iter()
                    .flatten()
                {
                    *text = self.expression(text.raw()).into();
                }
            }
            for own in table
                .partition_of
                .iter_mut()
                .flat_map(|of| of.columns.values_mut())
            {
                if let Some(text) = &mut own.default {
                    *text = self.expression(text);
                }
            }
        }
        for module in schema.modules.values_mut() {
            module.definition = self.expression(&module.definition);
        }
    }
}

fn quoted(reference: &TypeRef) -> String {
    let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
    format!("{}.{}", quote(reference.schema()), quote(reference.name()))
}

/// A bound regtype constant deparses its OID as a type name in a literal.
/// Ordinary text literals and string routine bodies must remain untouched.
fn regtype_literal(
    pg: &crate::Postgres,
    text: &str,
    code: &str,
    at: usize,
) -> Option<(usize, String)> {
    let rest = text.get(at..)?;
    let escape = rest.starts_with("E'") || rest.starts_with("e'");
    let start = at + usize::from(escape);
    if text.as_bytes().get(start) != Some(&b'\'') || code.as_bytes().get(at) != Some(&b' ') {
        return None;
    }
    let mut end = start + 1;
    while end < text.len() {
        let ch = text[end..].chars().next()?;
        end += ch.len_utf8();
        if escape && ch == '\\' {
            end += text[end..].chars().next()?.len_utf8();
        } else if ch == '\'' {
            if text.as_bytes().get(end) == Some(&b'\'') {
                end += 1;
            } else {
                break;
            }
        }
    }
    let cast = skip_space(text, end);
    if !code.get(cast..)?.starts_with("::") {
        return None;
    }
    let (first, mut tail) = identifier(code, skip_space(code, cast + 2))?;
    let name = if code.as_bytes().get(tail) == Some(&b'.') && first == "pg_catalog" {
        let (name, last) = identifier(code, tail + 1)?;
        tail = last;
        name
    } else {
        first
    };
    if name != "regtype" || code.as_bytes().get(tail) == Some(&b'.') {
        return None;
    }
    let value = pg
        .lexicon()
        .string_literals(&text[at..end])
        .into_iter()
        .next()?;
    Some((end, value))
}

fn skip_space(text: &str, at: usize) -> usize {
    at + text[at..]
        .bytes()
        .take_while(u8::is_ascii_whitespace)
        .count()
}

fn identifier(text: &str, at: usize) -> Option<(String, usize)> {
    let first = text.get(at..)?.chars().next()?;
    if first == '"' {
        let mut out = String::new();
        let mut chars = text[at + 1..].char_indices().peekable();
        while let Some((offset, ch)) = chars.next() {
            if ch == '"' {
                if chars.peek().is_some_and(|(_, next)| *next == '"') {
                    chars.next();
                } else {
                    return Some((out, at + 1 + offset + 1));
                }
            }
            out.push(ch);
        }
        None
    } else if first.is_ascii_alphabetic() || first == '_' || !first.is_ascii() {
        let end = text[at..]
            .find(|ch: char| {
                !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || !ch.is_ascii())
            })
            .map_or(text.len(), |len| at + len);
        Some((text[at..end].to_ascii_lowercase(), end))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn casts_follow_each_original_name_once_and_leave_data_and_other_names_alone() {
        let renames = TypeRenames(
            [
                ("app.a".parse().unwrap(), "app.b".parse().unwrap()),
                ("app.b".parse().unwrap(), "moved.c".parse().unwrap()),
            ]
            .into(),
        );
        let text =
            "'app.a'::app.a[] = app.a('app.b'::app.b) AND \"::app.a\" = $$::app.b$$ /* ::app.a */";
        assert_eq!(
            renames.expression(text),
            r#"'app.a'::"app"."b"[] = app.a('app.b'::"moved"."c") AND "::app.a" = $$::app.b$$ /* ::app.a */"#
        );
        assert_eq!(renames.expression("E'abc\\\'::app.a'"), "E'abc\\\'::app.a'");
        assert_eq!(renames.expression("'ok'::app.aa"), r#"'ok'::"app"."aa""#);
        assert_eq!(renames.expression("app.a.value"), "app.a.value");
    }

    #[test]
    fn regtype_constants_follow_the_bound_type_but_ordinary_literals_do_not() {
        let renames = TypeRenames(
            [(
                "app.mood".parse().unwrap(),
                "moved.feeling".parse().unwrap(),
            )]
            .into(),
        );
        assert_eq!(
            renames.expression("'app.mood'::regtype"),
            r#"E'"moved"."feeling"'::regtype"#
        );
        assert_eq!(
            renames.expression("'app.mood[]'::pg_catalog.regtype"),
            r#"E'"moved"."feeling"[]'::"pg_catalog"."regtype""#
        );
        for text in [
            "'app.mood'::text",
            "$$'app.mood'$$::regtype",
            "'other'::regtype",
            "/* 'app.mood'::regtype */ 1",
            "AS $$ SELECT 'app.mood'::regtype $$",
        ] {
            assert_eq!(renames.expression(text), text);
        }
    }

    #[test]
    fn quoted_identifiers_and_utf8_literals_keep_their_original_offsets() {
        let renames = TypeRenames(
            [(
                TypeRef::new("App", "a\"b").unwrap(),
                TypeRef::new("Moved", "select").unwrap(),
            )]
            .into(),
        );
        assert_eq!(
            renames.expression("'中文'::\"App\".\"a\"\"b\"[]"),
            "'中文'::\"Moved\".\"select\"[]"
        );
        assert_eq!(
            renames.expression("'x'::app.\"a\"\"b\""),
            r#"'x'::"app"."a""b""#
        );
    }
}
