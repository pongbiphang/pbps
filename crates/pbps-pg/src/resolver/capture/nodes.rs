//! Decode the engine's resolved node representation, not SQL syntax.
//!
//! Pinned builtin objects are omitted from pg_depend. Their actual bindings
//! still occur in these trees. Unknown nodes/fields refuse coverage instead
//! of silently turning an incomplete dependency set into a proof.

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ReferenceClass {
    Relation,
    Routine,
    Type,
    Operator,
    Collation,
    Role,
    Constraint,
    OperatorClass,
    OperatorFamily,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Reference {
    pub path: Vec<String>,
    pub class: ReferenceClass,
    pub oid: u32,
}

// No Debug/Serialize: a node can hold a private literal, including its bytes.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Value {
    Null,
    Atom(String),
    List(Vec<Value>),
    Node(Node),
    Datum,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Node {
    pub tag: String,
    pub fields: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Uncovered {
    Version,
    Malformed,
    Limit,
    Node,
    Field,
    Reference,
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
    items: usize,
}

impl Reader<'_> {
    fn whitespace(&mut self) {
        while self
            .bytes
            .get(self.position)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.position += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.whitespace();
        self.bytes.get(self.position).copied()
    }

    fn take(&mut self, expected: u8) -> Result<(), Uncovered> {
        if self.peek() != Some(expected) {
            return Err(Uncovered::Malformed);
        }
        self.position += 1;
        Ok(())
    }

    fn atom(&mut self) -> Result<String, Uncovered> {
        self.whitespace();
        let mut bytes = Vec::new();
        let quoted = self.bytes.get(self.position) == Some(&b'"');
        if quoted {
            self.position += 1;
        }
        loop {
            let Some(&byte) = self.bytes.get(self.position) else {
                if quoted {
                    return Err(Uncovered::Malformed);
                }
                break;
            };
            if quoted && byte == b'"' {
                self.position += 1;
                break;
            }
            if !quoted && (byte.is_ascii_whitespace() || b"{}()[]".contains(&byte)) {
                break;
            }
            self.position += 1;
            if byte == b'\\' {
                let escaped = *self.bytes.get(self.position).ok_or(Uncovered::Malformed)?;
                self.position += 1;
                bytes.push(escaped);
            } else {
                bytes.push(byte);
            }
        }
        if bytes.is_empty() && !quoted {
            return Err(Uncovered::Malformed);
        }
        String::from_utf8(bytes).map_err(|_| Uncovered::Malformed)
    }

    fn value(&mut self, depth: usize) -> Result<Value, Uncovered> {
        self.items += 1;
        if depth > 256 || self.items > 1_000_000 {
            return Err(Uncovered::Limit);
        }
        match self.peek().ok_or(Uncovered::Malformed)? {
            b'{' => {
                self.position += 1;
                let tag = self.atom()?;
                if tag.len() > 64 || !tag.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
                    return Err(Uncovered::Malformed);
                }
                let mut fields = BTreeMap::new();
                while self.peek() != Some(b'}') {
                    let key = self.atom()?;
                    let key = key
                        .strip_prefix(':')
                        .ok_or(Uncovered::Malformed)?
                        .to_owned();
                    if key.is_empty() {
                        return Err(Uncovered::Malformed);
                    }
                    let mut value = self.value(depth + 1)?;
                    // Datum output has two parts: its declared byte length
                    // and a bracketed byte vector. Neither is a binding.
                    if key == "constvalue" && self.peek() == Some(b'[') {
                        let Value::Atom(length) = &value else {
                            return Err(Uncovered::Malformed);
                        };
                        length.parse::<usize>().map_err(|_| Uncovered::Malformed)?;
                        self.position += 1;
                        while self.peek() != Some(b']') {
                            let byte = self
                                .atom()?
                                .parse::<i16>()
                                .map_err(|_| Uncovered::Malformed)?;
                            if !(-128..=255).contains(&byte) {
                                return Err(Uncovered::Malformed);
                            }
                        }
                        self.take(b']')?;
                        value = Value::Datum;
                    }
                    if fields.insert(key, value).is_some() {
                        return Err(Uncovered::Malformed);
                    }
                }
                self.take(b'}')?;
                Ok(Value::Node(Node { tag, fields }))
            }
            b'(' => {
                self.position += 1;
                let mut values = Vec::new();
                while self.peek() != Some(b')') {
                    values.push(self.value(depth + 1)?);
                }
                self.take(b')')?;
                Ok(Value::List(values))
            }
            b'}' | b')' | b'[' | b']' => Err(Uncovered::Malformed),
            _ => {
                let quoted = self.peek() == Some(b'"');
                let atom = self.atom()?;
                Ok(if !quoted && atom == "<>" {
                    Value::Null
                } else {
                    Value::Atom(atom)
                })
            }
        }
    }
}

pub(super) fn decode(text: &str, major: u32) -> Result<Value, Uncovered> {
    if !matches!(major, 16 | 18) {
        return Err(Uncovered::Version);
    }
    if text.len() > 16 * 1024 * 1024 {
        return Err(Uncovered::Limit);
    }
    let mut reader = Reader {
        bytes: text.as_bytes(),
        position: 0,
        items: 0,
    };
    let value = reader.value(0)?;
    if reader.peek().is_some() || matches!(value, Value::Null | Value::Atom(_) | Value::Datum) {
        return Err(Uncovered::Malformed);
    }
    Ok(value)
}

impl Value {
    pub(super) fn number(&self) -> Result<u32, Uncovered> {
        match self {
            Self::Atom(value) => value.parse().map_err(|_| Uncovered::Reference),
            Self::Null | Self::List(_) | Self::Node(_) | Self::Datum => Err(Uncovered::Reference),
        }
    }
}

impl Node {
    pub(super) fn number(&self, key: &str) -> Result<u32, Uncovered> {
        self.fields.get(key).ok_or(Uncovered::Field)?.number()
    }
}

pub(super) fn references(value: &Value, major: u32) -> Result<Vec<Reference>, Uncovered> {
    let mut references = Vec::new();
    walk(value, major, &mut Vec::new(), &mut references)?;
    Ok(references)
}

fn walk(
    value: &Value,
    major: u32,
    path: &mut Vec<String>,
    found: &mut Vec<Reference>,
) -> Result<(), Uncovered> {
    match value {
        Value::Node(node) => {
            let spec = specification(&node.tag, major).ok_or(Uncovered::Node)?;
            let fields = if node.tag == "RANGETBLENTRY" {
                range_table_fields(node, major)?
            } else {
                spec.fields.to_vec()
            };
            // Missing a child list is just as dangerous as missing an OID:
            // both would otherwise certify an incomplete set as complete.
            if fields.len() != node.fields.len()
                || fields.iter().any(|field| !node.fields.contains_key(*field))
            {
                return Err(Uncovered::Field);
            }
            for (name, value) in &node.fields {
                if !fields.contains(&name.as_str()) {
                    return Err(Uncovered::Field);
                }
                if let Some((_, list)) = spec.children.iter().find(|(field, _)| *field == name) {
                    let valid = matches!(value, Value::Null | Value::List(_))
                        || (!list && matches!(value, Value::Node(_)));
                    if !valid {
                        return Err(Uncovered::Field);
                    }
                }
                path.push(name.clone());
                if let Some((_, class, list)) =
                    spec.references.iter().find(|(field, _, _)| *field == name)
                {
                    if *list {
                        match value {
                            Value::Null => {}
                            Value::List(values) => {
                                if !matches!(values.first(), Some(Value::Atom(kind)) if kind == "o")
                                {
                                    return Err(Uncovered::Reference);
                                }
                                for (index, value) in values.iter().skip(1).enumerate() {
                                    path.push(index.to_string());
                                    add_reference(value, *class, path, found)?;
                                    path.pop();
                                }
                            }
                            Value::Atom(_) | Value::Node(_) | Value::Datum => {
                                return Err(Uncovered::Reference);
                            }
                        }
                    } else {
                        add_reference(value, *class, path, found)?;
                    }
                } else {
                    walk(value, major, path, found)?;
                }
                path.pop();
            }
        }
        Value::List(values) => {
            for (index, value) in values.iter().enumerate() {
                path.push(index.to_string());
                walk(value, major, path, found)?;
                path.pop();
            }
        }
        Value::Null | Value::Atom(_) | Value::Datum => {}
    }
    Ok(())
}

// outfuncs.c serializes only the active RangeTblEntry union arm. Requiring
// every header field rejects real subqueries; accepting arbitrary subsets
// loses their bindings. These are the two measured server layouts.
fn range_table_fields(node: &Node, major: u32) -> Result<Vec<&'static str>, Uncovered> {
    let kind = node.number("rtekind")?;
    let mut fields = vec![
        "alias",
        "eref",
        "rtekind",
        "lateral",
        "inFromCl",
        "securityQuals",
    ];
    if major == 16 || matches!(kind, 0 | 1) {
        fields.push("inh");
    }
    fields.extend_from_slice(match kind {
        0 => &[
            "relid",
            "relkind",
            "rellockmode",
            "tablesample",
            "perminfoindex",
        ],
        1 => &[
            "subquery",
            "security_barrier",
            "relid",
            "relkind",
            "rellockmode",
            "perminfoindex",
        ],
        2 => &[
            "jointype",
            "joinmergedcols",
            "joinaliasvars",
            "joinleftcols",
            "joinrightcols",
            "join_using_alias",
        ],
        3 => &["functions", "funcordinality"],
        4 => &["tablefunc"],
        5 => &["values_lists", "coltypes", "coltypmods", "colcollations"],
        6 => &[
            "ctename",
            "ctelevelsup",
            "self_reference",
            "coltypes",
            "coltypmods",
            "colcollations",
        ],
        7 => &[
            "enrname",
            "enrtuples",
            "coltypes",
            "coltypmods",
            "colcollations",
            "relid",
        ],
        8 => &[],
        9 if major == 18 => &["groupexprs"],
        _ => return Err(Uncovered::Node),
    });
    Ok(fields)
}

fn add_reference(
    value: &Value,
    class: ReferenceClass,
    path: &[String],
    found: &mut Vec<Reference>,
) -> Result<(), Uncovered> {
    let oid = value.number()?;
    if oid != 0 {
        found.push(Reference {
            path: path.to_vec(),
            class,
            oid,
        });
    }
    Ok(())
}

type Fields = &'static [&'static str];
type References = &'static [(&'static str, ReferenceClass, bool)];
type Children = &'static [(&'static str, bool)];

struct Specification {
    fields: Fields,
    references: References,
    children: Children,
}

include!("node_fields.rs");

#[cfg(test)]
mod tests {
    use super::*;

    fn actual_tree(major: u32) -> &'static str {
        match major {
            16 => include_str!("fixtures/builtin-16.nodes"),
            18 => include_str!("fixtures/builtin-18.nodes"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn pinned_builtin_bindings_are_read_even_without_dependency_rows() {
        for major in [16, 18] {
            let value = decode(actual_tree(major), major).unwrap_or_else(|e| panic!("{e:?}"));
            let bindings = references(&value, major).unwrap();
            for (class, oid) in [
                (ReferenceClass::Operator, 551),
                (ReferenceClass::Routine, 177),
                (ReferenceClass::Type, 23),
            ] {
                assert!(bindings.iter().any(|r| r.class == class && r.oid == oid));
            }
            assert!(bindings.iter().all(|r| r.oid != 0));
            assert_eq!(
                bindings
                    .iter()
                    .filter(|r| r.class == ReferenceClass::Type)
                    .count(),
                3
            );
        }
    }

    #[test]
    fn engine_views_routines_and_checks_keep_their_complete_bound_fields() {
        for (major, fixtures) in [
            (16, include_str!("fixtures/surfaces-16.nodes")),
            (18, include_str!("fixtures/surfaces-18.nodes")),
        ] {
            for (case, text) in fixtures.lines().enumerate() {
                let value = decode(text, major)
                    .unwrap_or_else(|e| panic!("PG{major} surface {case}: {e:?}"));
                let bindings = references(&value, major)
                    .unwrap_or_else(|e| panic!("PG{major} surface {case}: {e:?}"));
                assert!(!bindings.is_empty(), "PG{major} surface {case}");
            }
        }
    }

    #[test]
    fn unknown_or_missing_binding_fields_cannot_certify_an_empty_set() {
        let original = actual_tree(18);
        for changed in [
            original.replace(":opno 551", ":unknown_binding 551"),
            original.replace(":opno 551", ""),
            original.replace(":constraintDeps <>", ""),
            original.replace(":rtable <>", ""),
            original.replace(":rtable <>", ":rtable unreadable"),
        ] {
            let value = decode(&changed, 18).unwrap_or_else(|e| panic!("{e:?}"));
            assert_eq!(references(&value, 18), Err(Uncovered::Field));
        }
        let empty_query = decode("{QUERY}", 18).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(references(&empty_query, 18), Err(Uncovered::Field));
        let changed = original.replace("OPEXPR", "FUTUREEXPR");
        let value = decode(&changed, 18).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(references(&value, 18), Err(Uncovered::Node));
        assert!(matches!(decode(original, 17), Err(Uncovered::Version)));
    }

    #[test]
    fn absent_malformed_and_oversized_inputs_are_not_empty_evidence() {
        for text in [
            "",
            "<>",
            "{VAR :varno 1 :varno 2}",
            "(unclosed",
            "{}",
            "() trailing",
        ] {
            assert!(matches!(decode(text, 18), Err(Uncovered::Malformed)));
        }
        let too_deep = format!("{}{}", "(".repeat(258), ")".repeat(258));
        assert!(matches!(decode(&too_deep, 18), Err(Uncovered::Limit)));
        let bad_datum = actual_tree(18).replacen("[ 1", "[ 256", 1);
        assert!(matches!(decode(&bad_datum, 18), Err(Uncovered::Malformed)));
    }

    #[test]
    fn a_quoted_null_spelling_and_escaped_names_keep_their_identity() {
        let tree = decode(
            r#"{ALIAS :aliasname "<>" :colnames ("two words" "quote\"name")}"#,
            18,
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let Value::Node(node) = tree else {
            panic!("expected an alias")
        };
        assert!(matches!(&node.fields["aliasname"], Value::Atom(value) if value == "<>"));
        let Value::List(names) = &node.fields["colnames"] else {
            panic!("expected names")
        };
        assert!(matches!(&names[0], Value::Atom(value) if value == "two words"));
        assert!(matches!(&names[1], Value::Atom(value) if value == "quote\"name"));
    }
}
