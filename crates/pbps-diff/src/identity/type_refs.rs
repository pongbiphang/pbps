//! Side-specific type identity, shared by planning and execution (DEC-2321.1).

use pbps_model::{Change, ChangeSet, ColumnType, IdsFile, ManagedType, Schema, TypeRef, Uid};

#[derive(Clone, Copy)]
pub enum TypeSide {
    Base,
    Desired,
}

/// Missing identity evidence is an external reference, never an absent type.
#[derive(Debug, PartialEq, Eq)]
pub enum TypeIdentity<'a> {
    Declared(&'a Uid),
    ExternalOrUnrecorded(&'a TypeRef),
}

/// The dependent's own uid determines which side supplies its existing binding.
#[derive(Debug, PartialEq, Eq)]
pub enum TypeDependent<'a> {
    Retained(TypeIdentity<'a>),
    New(TypeIdentity<'a>),
}

/// What a column's values are at the bottom of its domain chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leaf {
    /// A declared enum.
    Enum(Uid),
    /// A built-in type, its own `[]` stripped.
    Builtin(ColumnType),
    /// A type this plan has no declaration of.
    Other,
}

/// The leaf of a column's type and the levels of array above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValuesShape {
    pub leaf: Leaf,
    pub arrays: usize,
}

pub struct TypeResolver<'a> {
    base: &'a IdsFile,
    desired: &'a IdsFile,
}

impl<'a> TypeResolver<'a> {
    pub fn new(base: &'a IdsFile, desired: &'a IdsFile) -> Self {
        Self { base, desired }
    }

    fn ids(&self, side: TypeSide) -> &'a IdsFile {
        match side {
            TypeSide::Base => self.base,
            TypeSide::Desired => self.desired,
        }
    }

    pub fn resolve<'r>(&'r self, side: TypeSide, reference: &'r TypeRef) -> TypeIdentity<'r> {
        match self.ids(side).type_uid(reference) {
            Some(uid) => TypeIdentity::Declared(uid),
            None => TypeIdentity::ExternalOrUnrecorded(reference),
        }
    }

    /// What a column's stored values are, looking through every domain from
    /// its type down to the leaf: the leaf type and the levels of array on the
    /// way, the column's own `[]` and each domain built on an array included.
    /// The one answer to "is this column an array of X" for a value check;
    /// reading only the column's own type misses a domain over `e[]`
    /// (DEC-1635.9). `schema` is the side's, and an unrecorded or external
    /// type is a leaf of its own.
    pub fn values_shape(&self, side: TypeSide, ty: &ColumnType, schema: &Schema) -> ValuesShape {
        let mut arrays = 0;
        let mut ty = ty.clone();
        for _ in 0..=schema.types.len() {
            if ty.is_array() {
                arrays += 1;
            }
            let Some(reference) = ty.type_ref() else {
                return ValuesShape {
                    leaf: Leaf::Builtin(ty.element()),
                    arrays,
                };
            };
            let leaf = |leaf| ValuesShape { leaf, arrays };
            let TypeIdentity::Declared(uid) = self.resolve(side, reference) else {
                return leaf(Leaf::Other);
            };
            match schema.types.get(reference) {
                Some(ManagedType::Enum { .. }) => return leaf(Leaf::Enum(uid.clone())),
                Some(ManagedType::Domain { base, .. }) => ty = base.clone(),
                None => return leaf(Leaf::Other),
            }
        }
        ValuesShape {
            leaf: Leaf::Other,
            arrays,
        }
    }

    pub fn name(&self, side: TypeSide, uid: &Uid) -> Option<&'a TypeRef> {
        self.ids(side).types.get(uid)
    }

    pub fn same_declared(&self, before: &TypeRef, after: &TypeRef) -> bool {
        matches!((self.resolve(TypeSide::Base, before), self.resolve(TypeSide::Desired, after)),
            (TypeIdentity::Declared(a), TypeIdentity::Declared(b)) if a == b)
    }

    /// Resolve once on the source side; a rename chain must not remap its output.
    pub fn remap(&self, ty: &mut ColumnType, from: TypeSide, to: TypeSide) {
        if let Some(reference) = ty.type_ref()
            && let TypeIdentity::Declared(uid) = self.resolve(from, reference)
            && let Some(name) = self.name(to, uid)
        {
            let reference = ColumnType::reference(name.clone());
            *ty = if ty.is_array() {
                reference.into_array()
            } else {
                reference
            };
        }
    }

    /// A retained declaration keeps its binding through a name replacement.
    /// An explicit retype to another spelling can release that binding; merely
    /// recycling its name cannot express a conversion between different uids.
    pub fn dependent<'r>(
        &'r self,
        before: Option<&'r ColumnType>,
        after: &'r ColumnType,
    ) -> Option<TypeDependent<'r>> {
        let reference = after.type_ref()?;
        if let Some(old) = before.and_then(ColumnType::type_ref)
            && (old == reference || self.same_declared(old, reference))
        {
            return Some(TypeDependent::Retained(self.resolve(TypeSide::Base, old)));
        }
        Some(TypeDependent::New(
            self.resolve(TypeSide::Desired, reference),
        ))
    }

    pub fn has_retained_dependent(&self, base: &Schema, desired: &Schema, uid: &Uid) -> bool {
        for (table_uid, name) in &self.desired.tables {
            let old = self
                .base
                .tables
                .get(table_uid)
                .and_then(|n| base.tables.get(n));
            let Some(table) = desired.tables.get(name) else {
                continue;
            };
            for (column, definition) in &table.columns {
                let reference = pbps_model::ColumnRef {
                    table: name.clone(),
                    name: column.clone(),
                };
                let old_column = match self.desired.column_uid(&reference) {
                    Some(column_uid) => self.base.columns.get(column_uid).map(|r| &r.name),
                    None => Some(column),
                };
                let before = old
                    .and_then(|t| old_column.and_then(|n| t.columns.get(n)))
                    .map(|c| &c.ty);
                if self.dependent(before, &definition.ty)
                    == Some(TypeDependent::Retained(TypeIdentity::Declared(uid)))
                {
                    return true;
                }
            }
        }
        self.desired.types.iter().any(|(own_uid, name)| {
            let Some(ManagedType::Domain { base: after, .. }) = desired.types.get(name) else {
                return false;
            };
            let before = self
                .base
                .types
                .get(own_uid)
                .and_then(|n| base.types.get(n))
                .and_then(|t| match t {
                    ManagedType::Domain { base, .. } => Some(base),
                    ManagedType::Enum { .. } => None,
                });
            self.dependent(before, after)
                == Some(TypeDependent::Retained(TypeIdentity::Declared(uid)))
        })
    }
}

/// Reconstruct the two identity maps from admitted uid-bearing operations.
/// This observes their endpoints; it neither schedules nor infers identity.
#[allow(clippy::wildcard_enum_match_arm)] // Only type lifecycle operations change this map.
pub fn before_changes(desired: &IdsFile, changes: &ChangeSet) -> IdsFile {
    let mut base = desired.clone();
    for step in &changes.changes {
        match &step.change {
            Change::CreateType { uid, .. } => {
                base.types.remove(uid);
            }
            Change::AddEnumValue { uid, name, .. }
            | Change::AlterDomain { uid, name, .. }
            | Change::RenameEnumValue { uid, name, .. }
            | Change::EnumRebuild { uid, name, .. } => {
                // The label operation certifies an existing holder even when
                // only statement metadata, rather than a pin bundle, is present.
                base.types
                    .entry(uid.clone())
                    .or_insert_with(|| name.clone());
            }
            Change::DropType { uid, name, .. }
            | Change::RenameType {
                uid, from: name, ..
            } => {
                base.types.insert(uid.clone(), name.clone());
            }
            _ => {}
        }
    }
    base
}

#[allow(clippy::wildcard_enum_match_arm)] // Only type lifecycle operations change this map.
pub fn after_changes(base: &IdsFile, changes: &ChangeSet) -> IdsFile {
    let mut desired = base.clone();
    for step in &changes.changes {
        match &step.change {
            Change::DropType { uid, .. } => {
                desired.types.remove(uid);
            }
            Change::AddEnumValue { uid, name, .. }
            | Change::AlterDomain { uid, name, .. }
            | Change::RenameEnumValue { uid, name, .. }
            | Change::EnumRebuild { uid, name, .. }
            | Change::CreateType { uid, name, .. }
            | Change::RenameType { uid, to: name, .. } => {
                desired.types.insert(uid.clone(), name.clone());
            }
            _ => {}
        }
    }
    desired
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_only_operations_reconstruct_their_declared_holder_without_inventing_other_types() {
        let uid: Uid = "y_000001".parse().unwrap();
        let name: TypeRef = "app.e".parse().unwrap();
        let steps = ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(Change::RenameEnumValue {
                uid: uid.clone(),
                name: name.clone(),
                from: "old".into(),
                to: "new".into(),
                ordinal: 0,
                definition: Box::new(ManagedType::Enum {
                    values: vec!["new".into()],
                }),
            })],
        };
        for ids in [
            before_changes(&IdsFile::default(), &steps),
            after_changes(&IdsFile::default(), &steps),
        ] {
            let resolver = TypeResolver::new(&ids, &ids);
            assert_eq!(
                resolver.resolve(TypeSide::Base, &name),
                TypeIdentity::Declared(&uid)
            );
            let unknown = "app.other".parse().unwrap();
            assert_eq!(
                resolver.resolve(TypeSide::Desired, &unknown),
                TypeIdentity::ExternalOrUnrecorded(&unknown)
            );
        }
    }

    #[test]
    fn each_side_resolves_its_uid_and_unrecorded_names_stay_external() {
        let a = "app.a".parse().unwrap();
        let b = "app.b".parse().unwrap();
        let c = "app.c".parse().unwrap();
        let one: Uid = "y_000001".parse().unwrap();
        let two: Uid = "y_000002".parse().unwrap();
        let base = IdsFile {
            types: [(one.clone(), a), (two.clone(), b)].into(),
            ..Default::default()
        };
        let desired = IdsFile {
            types: [(one.clone(), "app.b".parse().unwrap()), (two.clone(), c)].into(),
            ..Default::default()
        };
        let resolver = TypeResolver::new(&base, &desired);
        let name = "app.b".parse().unwrap();
        assert_eq!(
            resolver.resolve(TypeSide::Base, &name),
            TypeIdentity::Declared(&two)
        );
        assert_eq!(
            resolver.resolve(TypeSide::Desired, &name),
            TypeIdentity::Declared(&one)
        );
        assert!(!resolver.same_declared(&name, &name));
        for ty in ["app.b", "app.b[]"] {
            let mut ty: ColumnType = ty.parse().unwrap();
            let array = ty.is_array();
            resolver.remap(&mut ty, TypeSide::Base, TypeSide::Desired);
            assert_eq!(ty.type_ref(), Some(&"app.c".parse().unwrap()));
            assert_eq!(ty.is_array(), array);
        }
        let empty = IdsFile::default();
        let external = TypeResolver::new(&empty, &desired);
        assert_eq!(
            external.resolve(TypeSide::Base, &name),
            TypeIdentity::ExternalOrUnrecorded(&name)
        );
        assert!(!external.same_declared(&name, &name));
        let reference: ColumnType = "app.b[]".parse().unwrap();
        assert_eq!(
            resolver.dependent(None, &reference),
            Some(TypeDependent::New(TypeIdentity::Declared(&one)))
        );
        assert_eq!(
            resolver.dependent(Some(&reference), &reference),
            Some(TypeDependent::Retained(TypeIdentity::Declared(&two)))
        );
        assert!(matches!(
            external.dependent(Some(&reference), &reference),
            Some(TypeDependent::Retained(TypeIdentity::ExternalOrUnrecorded(
                _
            )))
        ));
        assert_eq!(resolver.dependent(None, &"integer".parse().unwrap()), None);
        let dropped = TypeResolver::new(&base, &empty);
        assert_eq!(
            dropped.resolve(TypeSide::Desired, &name),
            TypeIdentity::ExternalOrUnrecorded(&name)
        );
        assert_eq!(
            dropped.resolve(TypeSide::Base, &name),
            TypeIdentity::Declared(&two)
        );
        let mut unrecorded: ColumnType = "app.b[]".parse().unwrap();
        external.remap(&mut unrecorded, TypeSide::Base, TypeSide::Desired);
        assert_eq!(unrecorded, "app.b[]".parse::<ColumnType>().unwrap());
    }

    /// The shape of a column's values, through every domain (DEC-1635.9): the
    /// leaf and the levels of array, the column's own `[]` included.
    #[test]
    fn a_columns_values_are_read_through_every_domain_to_their_leaf() {
        let uid = |n: u32| -> Uid { format!("y_{n:06}").parse().unwrap() };
        let enumeration = ManagedType::Enum {
            values: vec!["a".into(), "b".into()],
        };
        let over = |base: &str| ManagedType::Domain {
            base: base.parse().unwrap(),
            not_null: false,
            default: None,
            checks: Default::default(),
            collation: None,
        };
        let schema = Schema {
            types: [
                ("app.e".parse().unwrap(), enumeration),
                ("app.scalar".parse().unwrap(), over("app.e")),
                ("app.arr".parse().unwrap(), over("app.e[]")),
                ("app.arr2".parse().unwrap(), over("app.arr")),
                ("app.arr_of_arr".parse().unwrap(), over("app.arr[]")),
                ("app.text".parse().unwrap(), over("text")),
                ("app.texts".parse().unwrap(), over("varchar(20)[]")),
            ]
            .into(),
            ..Default::default()
        };
        let ids = IdsFile {
            types: ["e", "scalar", "arr", "arr2", "arr_of_arr", "text", "texts"]
                .iter()
                .enumerate()
                .map(|(n, name)| (uid(n as u32 + 1), format!("app.{name}").parse().unwrap()))
                .collect(),
            ..Default::default()
        };
        let resolver = TypeResolver::new(&ids, &ids);
        let shape = |ty: &str| {
            let s = resolver.values_shape(TypeSide::Desired, &ty.parse().unwrap(), &schema);
            (s.leaf, s.arrays)
        };
        let e = Leaf::Enum(uid(1));
        // The enum itself and its own array.
        assert_eq!(shape("app.e"), (e.clone(), 0));
        assert_eq!(shape("app.e[]"), (e.clone(), 1));
        // A scalar domain adds none; a domain over `e[]` adds one, and the
        // column's own `[]` another (the engine keeps an array of such a
        // domain as an array of its arrays).
        assert_eq!(shape("app.scalar"), (e.clone(), 0));
        assert_eq!(shape("app.scalar[]"), (e.clone(), 1));
        assert_eq!(shape("app.arr"), (e.clone(), 1));
        assert_eq!(shape("app.arr[]"), (e.clone(), 2));
        // A domain over a domain over `e[]` is still one level, and over an
        // array of it two.
        assert_eq!(shape("app.arr2"), (e.clone(), 1));
        assert_eq!(shape("app.arr_of_arr"), (e.clone(), 2));
        // A built-in leaf is not an enum, scalar or array, through a domain.
        assert_eq!(
            shape("app.text"),
            (Leaf::Builtin("text".parse().unwrap()), 0)
        );
        assert_eq!(
            shape("app.texts"),
            (Leaf::Builtin("varchar(20)".parse().unwrap()), 1)
        );
        assert_eq!(
            shape("integer[]"),
            (Leaf::Builtin("integer".parse().unwrap()), 1)
        );
        // A type the plan does not declare is a leaf of its own, as is one
        // the schema lacks.
        assert_eq!(shape("other.t[]"), (Leaf::Other, 1));
        let nothing = IdsFile::default();
        let bare = TypeResolver::new(&nothing, &nothing);
        assert_eq!(
            bare.values_shape(TypeSide::Desired, &"app.arr".parse().unwrap(), &schema)
                .leaf,
            Leaf::Other
        );
    }
}
