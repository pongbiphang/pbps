//! A rename refusal, not policy binding certification (#595). Read the
//! engine's resolved tree with the existing versioned decoder; never parse or
//! rewrite authored SQL. pg_depend omits an owner's whole-row reference.

use super::nodes::{self, Uncovered, Value};

pub(crate) struct Rename {
    pub relation: u32,
    pub row_types: [u32; 2],
    pub owner: bool,
    pub alias_changes: bool,
    pub regclass: bool,
}

pub(crate) fn affected(text: &str, major: u32, rename: &Rename) -> Result<bool, String> {
    let read = || {
        let tree = nodes::decode(text, major)?;
        // Unknown fields or children cannot certify a negative answer.
        nodes::references(&tree, major)?;
        mentions(&tree, 0, rename)
    };
    read()
        .map_err(|why| format!("the policy's stored rename dependencies cannot be read ({why:?})"))
}

fn mentions(value: &Value, depth: u32, rename: &Rename) -> Result<bool, Uncovered> {
    match value {
        Value::Node(node) => {
            let depth = depth + u32::from(node.tag == "QUERY");
            if node.tag == "VAR"
                && rename.owner
                && rename.alias_changes
                && node.number("varno")? == 1
                && node.number("varlevelsup")? == depth
                && (depth > 0
                    || matches!(node.fields.get("varattno"), Some(Value::Atom(n)) if n == "0"))
            {
                // Root scalar columns deparse bare; a whole row and a
                // correlated owner reference carry the owner's alias.
                return Ok(true);
            }
            if node.tag == "RANGETBLENTRY"
                && node.number("rtekind")? == 0
                && node.number("relid")? == rename.relation
            {
                return Ok(true);
            }
            if node.tag == "CONST" {
                let ty = node.number("consttype")?;
                if (rename.regclass && ty == 2205) || rename.row_types.contains(&ty) {
                    return Ok(true);
                }
            }
            // An inferred result type does not print a type name. Explicit
            // composite casts do, including an array of the table's row type.
            let cast = match node.tag.as_str() {
                "ROWEXPR" => Some(("row_typeid", "row_format", false)),
                "CONVERTROWTYPEEXPR" => Some(("resulttype", "convertformat", false)),
                "RELABELTYPE" => Some(("resulttype", "relabelformat", false)),
                "COERCEVIAIO" | "ARRAYCOERCEEXPR" => Some(("resulttype", "coerceformat", false)),
                "FUNCEXPR" => Some(("funcresulttype", "funcformat", true)),
                _ => None,
            };
            if let Some((ty, format, function)) = cast {
                let format = node.number(format)?;
                if rename.row_types.contains(&node.number(ty)?)
                    && if function { format == 1 } else { format != 2 }
                {
                    return Ok(true);
                }
            }
            for value in node.fields.values() {
                if mentions(value, depth, rename)? {
                    return Ok(true);
                }
            }
        }
        Value::List(values) => {
            for value in values {
                if mentions(value, depth, rename)? {
                    return Ok(true);
                }
            }
        }
        Value::Null | Value::Atom(_) | Value::Datum => {}
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variable(major: u32, att: i32) -> String {
        format!(
            "{{VAR :varno 1 :varattno {att} :vartype 23 :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 {}:varnosyn 1 :varattnosyn {att} :location 0}}",
            if major == 18 {
                ":varreturningtype 0 "
            } else {
                ""
            }
        )
    }

    #[test]
    fn owner_alias_changes_do_not_include_bare_scalar_columns_or_schema_only_moves() {
        for major in [16, 18] {
            let mut rename = Rename {
                relation: 50000,
                row_types: [50001, 50002],
                owner: true,
                alias_changes: true,
                regclass: false,
            };
            assert!(!affected(&variable(major, 1), major, &rename).unwrap());
            assert!(affected(&variable(major, 0), major, &rename).unwrap());
            rename.alias_changes = false;
            assert!(!affected(&variable(major, 0), major, &rename).unwrap());
            rename.alias_changes = true;
            rename.owner = false;
            assert!(!affected(&variable(major, 0), major, &rename).unwrap());
            assert!(affected("{UNKNOWN :x 1}", major, &rename).is_err());
            assert!(affected("{VAR :varattno 0}", major, &rename).is_err());
            assert!(affected(&variable(major, 0), 17, &rename).is_err());
        }
    }
}
