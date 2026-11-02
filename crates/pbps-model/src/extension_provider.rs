//! What an installed extension provides, as logical facts (DEC-2447.4).
//!
//! A pinned name, schema and version says which package was asked for, not what
//! it brought: its member objects are written by vendor scripts that pbps does
//! not read. This is the observable closure of those scripts, so that the same
//! package read on two servers can be compared. It carries no object number, no
//! owner and no library path: a number means nothing on the other server, an
//! owner depends on who installed the package, and a path is not a content
//! digest (design on #2447, "Resolver, scratch and extension-provided SQL
//! surfaces").
//!
//! Nothing executable consumes this yet; the CREATE operation that seals it into
//! a plan is a later slice of #2447.

use crate::Extension;
use crate::escape::Escaped;
use std::collections::BTreeSet;
use std::fmt;

/// One object an extension owns, addressed as the engine renders it: the class
/// (`function`, `type`, `operator class`, ...), the name parts, and the argument
/// types that tell overloads and operators apart. The parts are the engine's, so
/// a schema-qualified function is two parts while a type is one rendered string
/// (`ext.citext`); no consumer splits them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderMember {
    pub class: String,
    pub names: Vec<String>,
    pub args: Vec<String>,
}

impl fmt::Display for ProviderMember {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ", Escaped(&self.class))?;
        for (i, part) in self.names.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{}", Escaped(part))?;
        }
        if !self.args.is_empty() {
            f.write_str("(")?;
            for (i, arg) in self.args.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{}", Escaped(arg))?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}

/// An installed package and everything it owns or requires.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionProvider {
    pub definition: Extension,
    /// The packages this one was installed on top of. Empty is a fact (it
    /// needs none), never a stand-in for an unread list.
    pub requires: BTreeSet<String>,
    /// Direct members and their internal and automatic subsidiaries. Empty is
    /// legal only for a package whose script made no object.
    pub members: BTreeSet<ProviderMember>,
}

/// How many differing items a refusal spells out before it counts the rest.
const SHOWN: usize = 5;

impl ExtensionProvider {
    /// Every way `actual` is not this provider, one sentence each; empty when
    /// they are equal. Both sides are complete closures, so a member present on
    /// one side only is a difference whichever side holds it: a rename is a
    /// removal and an addition, never a match.
    #[must_use]
    pub fn differences(&self, actual: &Self) -> Vec<String> {
        let mut out = Vec::new();
        if self.definition.schema != actual.definition.schema {
            out.push(format!(
                "schema `{}` became `{}`",
                Escaped(&self.definition.schema),
                Escaped(&actual.definition.schema)
            ));
        }
        if self.definition.version != actual.definition.version {
            out.push(format!(
                "version `{}` became `{}`",
                Escaped(&self.definition.version),
                Escaped(&actual.definition.version)
            ));
        }
        let missing: Vec<_> = self
            .requires
            .difference(&actual.requires)
            .map(|n| Escaped(n))
            .collect();
        let extra: Vec<_> = actual
            .requires
            .difference(&self.requires)
            .map(|n| Escaped(n))
            .collect();
        list(&mut out, "prerequisite", "missing", &missing);
        list(&mut out, "prerequisite", "unexpected", &extra);
        let missing: Vec<_> = self.members.difference(&actual.members).collect();
        let extra: Vec<_> = actual.members.difference(&self.members).collect();
        list(&mut out, "member", "missing", &missing);
        list(&mut out, "member", "unexpected", &extra);
        out
    }
}

fn list<T: fmt::Display>(out: &mut Vec<String>, what: &str, kind: &str, items: &[T]) {
    if items.is_empty() {
        return;
    }
    let shown: Vec<String> = items.iter().take(SHOWN).map(|i| format!("`{i}`")).collect();
    let more = items.len().saturating_sub(SHOWN);
    out.push(format!(
        "{} {what}{}: {}{}",
        kind,
        if items.len() == 1 { "" } else { "s" },
        shown.join(", "),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(class: &str, name: &str, args: &[&str]) -> ProviderMember {
        ProviderMember {
            class: class.into(),
            names: vec!["ext".into(), name.into()],
            args: args.iter().map(|a| (*a).into()).collect(),
        }
    }

    fn provider() -> ExtensionProvider {
        ExtensionProvider {
            definition: Extension {
                schema: "ext".into(),
                version: "1.6".into(),
            },
            requires: ["cube".to_owned()].into(),
            members: [
                member("type", "citext", &[]),
                member("function", "citextin", &["cstring"]),
            ]
            .into(),
        }
    }

    #[test]
    fn an_identical_closure_has_no_differences() {
        assert!(provider().differences(&provider()).is_empty());
    }

    #[test]
    fn each_kind_of_difference_is_named_on_its_own() {
        let expected = provider();
        type Change = fn(&mut ExtensionProvider);
        let cases: [(&str, Change); 6] = [
            ("schema `ext` became `other`", |p| {
                p.definition.schema = "other".into()
            }),
            ("version `1.6` became `1.8`", |p| {
                p.definition.version = "1.8".into()
            }),
            ("missing prerequisite: `cube`", |p| p.requires.clear()),
            ("unexpected prerequisite: `plperl`", |p| {
                p.requires.insert("plperl".into());
            }),
            ("missing member: `type ext.citext`", |p| {
                p.members.remove(&member("type", "citext", &[]));
            }),
            (
                "unexpected member: `function ext.citext_eq(citext, citext)`",
                |p| {
                    p.members
                        .insert(member("function", "citext_eq", &["citext", "citext"]));
                },
            ),
        ];
        for (sentence, change) in cases {
            let mut actual = expected.clone();
            change(&mut actual);
            assert_eq!(expected.differences(&actual), [sentence], "{sentence}");
        }
    }

    #[test]
    fn a_renamed_member_is_a_removal_and_an_addition_not_a_match() {
        let mut actual = provider();
        actual.members.remove(&member("type", "citext", &[]));
        actual.members.insert(member("type", "citext2", &[]));
        assert_eq!(provider().differences(&actual).len(), 2);
        // Overloads are different members: only the argument types differ.
        let mut overload = provider();
        overload
            .members
            .insert(member("function", "citextin", &["text"]));
        assert_eq!(provider().differences(&overload).len(), 1);
    }

    #[test]
    fn a_long_difference_is_counted_and_a_member_name_is_escaped() {
        let mut actual = provider();
        for i in 0..8 {
            actual
                .members
                .insert(member("function", &format!("f{i}"), &[]));
        }
        let found = provider().differences(&actual);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("and 3 more"), "{found:?}");
        let mut spoofed = provider();
        spoofed
            .members
            .insert(member("function", "a\nforged line", &[]));
        let found = provider().differences(&spoofed);
        assert!(!found[0].contains('\n'), "{found:?}");
    }

    #[test]
    fn the_wire_form_refuses_unknown_fields_and_round_trips() {
        let json = serde_json::to_string(&provider()).unwrap();
        assert_eq!(
            serde_json::from_str::<ExtensionProvider>(&json).unwrap(),
            provider()
        );
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["owner"] = "postgres".into();
        assert!(serde_json::from_value::<ExtensionProvider>(value).is_err());
    }
}
