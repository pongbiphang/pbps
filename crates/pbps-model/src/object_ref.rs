//! Declaration addresses and their recorded identities, outside Schema (DEC-2469.1).

mod registry;
pub use registry::{BoundRoot, ObjectDeps, Problem, Registry};

use crate::operation::PartKey;
use crate::{ColumnRef, ModuleId, RoleName, TableName, TypeRef};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Root {
    Table(TableName),
    Column(ColumnRef),
    Type(TypeRef),
    Module(ModuleId),
    Role(RoleName),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Selector {
    Whole,
    Default,
    Generation,
    Sequence,
    NotNull,
    Part(PartKey),
}

/// The same address type is used for named declarations and bound UID roots.
/// Its JSON codec is only a lossless *reference spelling*, not a saved-plan field.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectRef<R = Root> {
    pub root: R,
    pub part: Selector,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid object reference `{}`: {reason}", crate::escape::Escaped(.input))]
pub struct RefError {
    input: String,
    reason: String,
}

impl RefError {
    fn new(input: &str, reason: impl Into<String>) -> Self {
        Self {
            input: input.into(),
            reason: reason.into(),
        }
    }
}

impl Selector {
    fn fields(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Whole => ("whole", None),
            Self::Default => ("default", None),
            Self::Generation => ("generation", None),
            Self::Sequence => ("sequence", None),
            Self::NotNull => ("not_null", None),
            Self::Part(p) => match p {
                PartKey::PrimaryKey => ("primary_key", None),
                PartKey::Unique(n) => ("unique", Some(n.raw())),
                PartKey::ForeignKey(n) => ("foreign_key", Some(n.raw())),
                PartKey::Check(n) => ("check", Some(n.raw())),
                PartKey::Index(n) => ("index", Some(n.raw())),
                PartKey::Computed(n) => ("computed", Some(n.raw())),
                PartKey::Policy(n) => ("policy", Some(n)),
                PartKey::RowSecurity => ("row_security", None),
            },
        }
    }

    fn parse(kind: &str, name: Option<&str>) -> Result<Self, &'static str> {
        if name == Some("") {
            return Err("empty member name");
        }
        Ok(match (kind, name) {
            ("whole", None) => Self::Whole,
            ("default", None) => Self::Default,
            ("generation", None) => Self::Generation,
            ("sequence", None) => Self::Sequence,
            ("not_null", None) => Self::NotNull,
            ("primary_key", None) => Self::Part(PartKey::PrimaryKey),
            ("row_security", None) => Self::Part(PartKey::RowSecurity),
            ("unique", Some(n)) => Self::Part(PartKey::Unique(n.into())),
            ("foreign_key", Some(n)) => Self::Part(PartKey::ForeignKey(n.into())),
            ("check", Some(n)) => Self::Part(PartKey::Check(n.into())),
            ("index", Some(n)) => Self::Part(PartKey::Index(n.into())),
            ("computed", Some(n)) => Self::Part(PartKey::Computed(n.into())),
            ("policy", Some(n)) => Self::Part(PartKey::Policy(n.into())),
            _ => return Err("unknown or malformed selector"),
        })
    }
}

impl Serialize for Selector {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let (kind, name) = self.fields();
        match name {
            Some(name) => std::collections::BTreeMap::from([(kind, name)]).serialize(s),
            None => kind.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Selector {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Selector;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a selector or exactly one named selector")
            }
            fn visit_str<E: serde::de::Error>(self, kind: &str) -> Result<Selector, E> {
                Selector::parse(kind, None).map_err(E::custom)
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Selector, M::Error> {
                let (kind, name): (String, String) = map
                    .next_entry()?
                    .ok_or_else(|| serde::de::Error::custom("missing selector"))?;
                // Even equal duplicate keys are malformed, not last-writer-wins.
                if map.next_key::<String>()?.is_some() {
                    return Err(serde::de::Error::custom("exactly one selector is required"));
                }
                Selector::parse(&kind, Some(&name)).map_err(serde::de::Error::custom)
            }
        }
        d.deserialize_any(Visitor)
    }
}

impl ObjectRef {
    pub fn shown(&self) -> impl std::fmt::Display + '_ {
        struct Shown<'a>(&'a ObjectRef);
        impl std::fmt::Display for Shown<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&crate::escape::Escaped(&self.0.raw()), f)
            }
        }
        Shown(self)
    }

    pub fn whole(root: Root) -> Self {
        Self {
            root,
            part: Selector::Whole,
        }
    }

    /// Legacy YAML is deliberately parsed as ModuleId until the complete route ships.
    pub fn legacy(input: &str) -> Result<Self, crate::ModuleIdError> {
        input.parse().map(|id| Self::whole(Root::Module(id)))
    }

    pub fn module_id(&self) -> Option<&ModuleId> {
        match (&self.root, &self.part) {
            (Root::Module(id), Selector::Whole) => Some(id),
            _ => None,
        }
    }

    pub fn legacy_spelling(&self) -> Option<String> {
        self.module_id().map(|id| id.raw().to_string())
    }

    /// Shorthand is only an optimization. Reparse before choosing it so local
    /// punctuation cannot change the reference's meaning.
    pub fn raw(&self) -> String {
        let root = match &self.root {
            Root::Table(n) => ("table", n.raw().to_string()),
            Root::Column(n) => ("column", n.raw().to_string()),
            Root::Type(n) => ("type", n.raw().to_string()),
            Root::Module(n) => ("module", n.raw().to_string()),
            Root::Role(n) => ("role", n.raw().to_owned()),
        };
        let (kind, member) = self.part.fields();
        let short = if self.part == Selector::Whole {
            format!("{}::{}", root.0, root.1)
        } else {
            let qualifier = if matches!(&self.root, Root::Type(_) | Root::Module(_)) {
                format!("{}::", root.0)
            } else {
                String::new()
            };
            let member = member.map_or_else(String::new, |n| format!(".{n}"));
            format!("{kind}::{qualifier}{}{member}", root.1)
        };
        if short.parse::<Self>().as_ref() == Ok(self) {
            short
        } else {
            format!(
                "object::{}",
                serde_json::to_string(self).expect("reference serializes")
            )
        }
    }
}

impl FromStr for ObjectRef {
    type Err = RefError;
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let error = |reason: &str| RefError::new(input, reason);
        let Some((kind, value)) = input.split_once("::") else {
            return Self::legacy(input).map_err(|_| error("expected a qualified ModuleId"));
        };
        if value.is_empty() {
            return Err(error("empty reference component"));
        }
        if kind == "object" {
            let reference: Self =
                serde_json::from_str(value).map_err(|_| error("malformed structured reference"))?;
            if matches!(&reference.root, Root::Role(n) if n.raw().is_empty()) {
                return Err(error("empty reference component"));
            }
            return Ok(reference);
        }
        let root = match kind {
            "table" => Root::Table(value.parse().map_err(|_| error("expected schema.table"))?),
            "column" => Root::Column(
                value
                    .parse()
                    .map_err(|_| error("expected schema.table.column"))?,
            ),
            "type" => Root::Type(
                value
                    .parse()
                    .map_err(|_| error("expected a qualified type"))?,
            ),
            "module" => Root::Module(value.parse().map_err(|_| error("expected a ModuleId"))?),
            "role" if !value.is_empty() => Root::Role(value.into()),
            "default" | "generation" | "sequence" | "not_null" => {
                let root = if let Some(ty) = value.strip_prefix("type::") {
                    Root::Type(ty.parse().map_err(|_| error("expected a qualified type"))?)
                } else {
                    Root::Column(
                        value
                            .parse()
                            .map_err(|_| error("expected schema.table.column"))?,
                    )
                };
                return Ok(Self {
                    root,
                    part: Selector::parse(kind, None).map_err(error)?,
                });
            }
            "primary_key" | "row_security" => {
                return Ok(Self {
                    root: Root::Table(value.parse().map_err(|_| error("expected schema.table"))?),
                    part: Selector::parse(kind, None).map_err(error)?,
                });
            }
            "index" | "unique" | "check" | "foreign_key" | "policy" => {
                let (scope, value) = if let Some(v) = value.strip_prefix("type::") {
                    ("type", v)
                } else if let Some(v) = value.strip_prefix("module::") {
                    ("module", v)
                } else {
                    ("table", value)
                };
                let (owner, name) = value
                    .rsplit_once('.')
                    .ok_or_else(|| error("missing member name"))?;
                let root = match scope {
                    "type" => Root::Type(
                        owner
                            .parse()
                            .map_err(|_| error("expected a qualified type owner"))?,
                    ),
                    "module" => Root::Module(
                        owner
                            .parse()
                            .map_err(|_| error("expected a ModuleId owner"))?,
                    ),
                    _ => Root::Table(
                        owner
                            .parse()
                            .map_err(|_| error("expected schema.table owner"))?,
                    ),
                };
                return Ok(Self {
                    root,
                    part: Selector::parse(kind, Some(name)).map_err(error)?,
                });
            }
            _ => return Err(error("unknown reference prefix")),
        };
        Ok(Self::whole(root))
    }
}

#[cfg(test)]
mod tests;
