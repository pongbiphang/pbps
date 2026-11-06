//! Prerequisites of columns that reference existing types (DEC-1523.1–3).

pub mod declared;
pub mod renames;

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError};
use pbps_model::escape::Escaped;
use pbps_model::{
    Change, ChangeSet, Column, ColumnName, ColumnType, ModuleKind, Schema, TypePin, TypeProvider,
    TypeRef, ValueSource,
};

use crate::admission::Admission;
use crate::pins::Within;
use crate::user_types::{self, TypeRead, UserType};
use pbps_diff::{
    Side,
    identity::type_refs::{TypeIdentity, TypeResolver, TypeSide},
};

fn failed(why: impl Into<String>) -> DbError {
    DbError::BadRow(why.into())
}

fn reference_uses<'a>(
    schemas: &[(TypeSide, &'a Schema)],
    changes: &'a ChangeSet,
) -> Vec<(TypeSide, &'a ColumnType)> {
    let mut types = Vec::new();
    for (side, schema) in schemas {
        types.extend(
            schema
                .tables
                .values()
                .flat_map(|t| t.columns.values().map(|c| (*side, &c.ty))),
        );
        types.extend(schema.types.values().filter_map(|t| match t {
            pbps_model::ManagedType::Domain { base, .. }
                if base.type_ref().is_some_and(|r| r.schema() != "pg_catalog") =>
            {
                Some((*side, base))
            }
            pbps_model::ManagedType::Enum { .. } | pbps_model::ManagedType::Domain { .. } => None,
        }));
    }
    for planned in &changes.changes {
        match &planned.change {
            Change::RebuildPartitionedTable(r) => {
                types.extend(
                    r.before
                        .values()
                        .flat_map(|t| t.columns.values().map(|c| (TypeSide::Base, &c.ty))),
                );
                types.extend(
                    r.after
                        .values()
                        .flat_map(|t| t.columns.values().map(|c| (TypeSide::Desired, &c.ty))),
                );
            }
            Change::CopyTableRows(c) => types.extend(c.columns.iter().flat_map(|c| {
                std::iter::once((TypeSide::Desired, &c.cast))
                    .chain(c.source.iter().map(|s| (TypeSide::Base, &s.ty)))
            })),
            Change::BuildPartitionedIndex(_)
            | Change::AnalyzeTableTree(_)
            | Change::CreateExtension { .. }
            | Change::DropExtension { .. }
            | Change::UpdateExtension { .. }
            | Change::SetExtensionSchema { .. } => {}
            Change::CreateTable { table, .. }
            | Change::AttachPartition { shape: table, .. }
            | Change::DetachPartition { shape: table, .. } => {
                types.extend(table.columns.values().map(|c| (TypeSide::Desired, &c.ty)));
            }
            Change::AddColumn { column, .. } => types.push((TypeSide::Desired, &column.ty)),
            Change::AlterColumnType { from, to, .. } => {
                types.extend([(TypeSide::Base, from), (TypeSide::Desired, to)])
            }
            Change::AlterColumnNullability { ty, .. } | Change::AlterColumnNumbering { ty, .. } => {
                types.push((TypeSide::Desired, ty))
            }
            Change::InsertRow { types: carried, .. } => {
                types.extend(carried.values().map(|ty| (TypeSide::Desired, ty)))
            }
            Change::UpdateRow {
                types: before,
                after_types,
                key_type,
                ..
            }
            | Change::DeleteRow {
                types: before,
                after_types,
                key_type,
                ..
            } => {
                types.extend(before.values().map(|ty| (TypeSide::Base, ty)));
                types.extend(after_types.values().map(|ty| (TypeSide::Desired, ty)));
                // The row key is compared after structural changes; unlike
                // recorded cells, its carried type is the declared one.
                types.extend(key_type.iter().map(|ty| (TypeSide::Desired, ty)));
            }
            Change::AddEnumValue { definition, .. }
            | Change::AlterDomain { to: definition, .. }
            | Change::RenameEnumValue { definition, .. }
            | Change::CreateType { definition, .. }
            | Change::RenameType { definition, .. } => {
                if let pbps_model::ManagedType::Domain { base, .. } = definition.as_ref()
                    && base.type_ref().is_some_and(|r| r.schema() != "pg_catalog")
                {
                    types.push((TypeSide::Desired, base));
                }
            }
            // A rebuild's replacement is an enum with no references of its own.
            Change::EnumRebuild { .. } => {}
            Change::DropTable { .. }
            | Change::RenameTable { .. }
            | Change::DropColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnExpression { .. }
            | Change::AddComputedColumn { .. }
            | Change::DropComputedColumn { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetReplicaIdentity { .. }
            | Change::SetStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::DropPolicy { .. }
            | Change::CreatePolicy { .. }
            | Change::SetRowSecurity { .. }
            | Change::SetPartitionDefault { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::SetPrimaryKey { .. }
            | Change::AddUnique { .. }
            | Change::DropUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::DropForeignKey { .. }
            | Change::AddCheck { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. }
            | Change::DropIndex { .. }
            | Change::SetDataMode { .. }
            | Change::SetEnsureKeys { .. }
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            | Change::DropModule { .. }
            | Change::DropType { .. }
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::PublicExecution { .. } => {}
        }
    }
    types
}

pub fn references(schemas: &[&Schema], changes: &ChangeSet) -> BTreeSet<TypeRef> {
    let schemas: Vec<_> = schemas.iter().map(|s| (TypeSide::Desired, *s)).collect();
    reference_uses(&schemas, changes)
        .into_iter()
        .filter_map(|(_, ty)| ty.type_ref().cloned())
        .collect()
}

/// A managed name on the other side cannot erase an external prerequisite.
pub fn external_references(
    base: Side<'_>,
    desired: Side<'_>,
    changes: &ChangeSet,
) -> BTreeSet<TypeRef> {
    let resolver = TypeResolver::new(base.ids, desired.ids);
    reference_uses(
        &[
            (TypeSide::Base, base.schema),
            (TypeSide::Desired, desired.schema),
        ],
        changes,
    )
    .into_iter()
    .filter_map(|(side, ty)| {
        let reference = ty.type_ref()?;
        matches!(
            resolver.resolve(side, reference),
            TypeIdentity::ExternalOrUnrecorded(_)
        )
        .then(|| reference.clone())
    })
    .collect()
}

/// Managed definitions are certified by their checksum-pinned declarations.
#[allow(clippy::wildcard_enum_match_arm)] // Only type lifecycle changes add managed names.
pub fn managed_names(schemas: &[&Schema], changes: &ChangeSet) -> BTreeSet<TypeRef> {
    let mut names: BTreeSet<_> = schemas
        .iter()
        .flat_map(|s| s.types.keys().cloned())
        .collect();
    for planned in &changes.changes {
        match &planned.change {
            Change::AddEnumValue { name, .. }
            | Change::AlterDomain { name, .. }
            | Change::RenameEnumValue { name, .. }
            | Change::EnumRebuild { name, .. }
            | Change::CreateType { name, .. }
            | Change::DropType { name, .. } => {
                names.insert(name.clone());
            }
            Change::RenameType { from, to, .. } => {
                names.extend([from.clone(), to.clone()]);
            }
            _ => {}
        }
    }
    names
}

pub(crate) async fn read(
    conn: &mut Conn,
    within: Within,
    references: &[TypeRef],
) -> Result<Vec<UserType>, DbError> {
    let reads = user_types::read(conn, within, references).await;
    reads
        .into_iter()
        .zip(references)
        .map(|(read, reference)| match read {
            TypeRead::Present(ty) => Ok(*ty),
            other @ (TypeRead::Absent
            | TypeRead::NotDefined
            | TypeRead::Unreadable(_)
            | TypeRead::NotAdmitted(_)) => Err(failed(
                other
                    .refusal(reference)
                    .expect("a non-present type refuses"),
            )),
        })
        .collect()
}

fn pin(ty: &UserType) -> TypePin {
    TypePin {
        schema: ty.reference().schema().to_owned(),
        name: ty.reference().name().to_owned(),
        oid: ty.oid(),
        kind: ty.kind().as_str().to_owned(),
        digest: ty.pin().as_str().to_owned(),
        provider: ty.provider().map(|(extension, version)| TypeProvider {
            extension: extension.to_owned(),
            version: version.to_owned(),
            moved_to: None,
        }),
    }
}

/// The kind of a pin whose type the plan itself will bring: a column names a
/// type of a package this plan creates, which the target does not hold yet, so
/// there is nothing to read and digest. The package's own sealed manifest must
/// list the type (DEC-2447.12); apply reads nothing for it.
pub const PLANNED: &str = "planned";

/// Whether `pin` is one of those placeholders.
#[must_use]
pub fn is_planned(pin: &TypePin) -> bool {
    pin.kind == PLANNED
}

pub async fn trusted_roles(
    conn: &mut Conn,
    trusted: &BTreeSet<pbps_model::RoleName>,
) -> Result<(), DbError> {
    let missing =
        crate::input_path::missing_roles(conn, &trusted.iter().cloned().collect::<Vec<_>>())
            .await?;
    if !missing.is_empty() {
        return Err(failed(format!(
            "trusted_roles names roles that do not exist: {}",
            missing
                .iter()
                .map(|role| role.shown().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

fn check_defaults(schema: &Schema, types: &[UserType]) -> Result<(), String> {
    for (table, definition) in &schema.tables {
        let check_default = |column: &ColumnName, spec: &Column| {
            let ty = &spec.ty;
            let Some(reference) = ty.type_ref() else {
                return Ok(());
            };
            if fill(spec) == Fill::Null
                && (ty.is_array()
                    || match schema.types.get(reference) {
                        // Declared types have no external pin, but their kind still
                        // decides whether PostgreSQL preserves the column default.
                        Some(pbps_model::ManagedType::Enum { .. }) => true,
                        Some(pbps_model::ManagedType::Domain { .. }) => false,
                        None => types.iter().any(|ty| {
                            ty.reference() == reference && ty.kind() != user_types::Kind::Domain
                        }),
                    })
            {
                return Err(format!(
                    "column `{table}.{column}` has a NULL default the engine does not keep; declare no default",
                    table = table.shown(),
                    column = column.shown()
                ));
            }
            Ok(())
        };
        for (column, spec) in &definition.columns {
            if matches!(&spec.value_source, ValueSource::Default(_)) {
                check_default(column, spec)?;
            }
        }
        // Partitions declare only overrides; their column types live on the parent.
        if let Some(part) = &definition.partition_of
            && let Some(parent) = schema.tables.get(&part.parent)
        {
            for (column, own) in &part.columns {
                if let Some(spec) = parent.columns.get(column)
                    && let Some(expr) = &own.default
                {
                    let mut override_column = Column::new(spec.ty.clone());
                    override_column.value_source = ValueSource::Default(expr.clone());
                    check_default(column, &override_column)?;
                }
            }
        }
    }
    Ok(())
}

pub async fn seal(
    conn: &mut Conn,
    schemas: &[&Schema],
    changes: &ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
) -> Result<Vec<TypePin>, DbError> {
    trusted_roles(conn, trusted).await?;
    let managed = managed_names(schemas, changes);
    let references: Vec<_> = references(schemas, changes)
        .into_iter()
        .filter(|r| !managed.contains(r))
        .collect();
    seal_references(conn, schemas, references, changes).await
}

/// Planning supplies both uid maps; bootstrap's single-side inventory stays separate.
pub async fn seal_sides(
    conn: &mut Conn,
    base: Side<'_>,
    desired: Side<'_>,
    changes: &ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
) -> Result<Vec<TypePin>, DbError> {
    trusted_roles(conn, trusted).await?;
    let references = external_references(base, desired, changes)
        .into_iter()
        .collect();
    seal_references(conn, &[base.schema, desired.schema], references, changes).await
}

/// The packages the project declares, by the schema each is declared in. A
/// package the target lacks is created by this plan or the plan is refused
/// elsewhere, and the sealed manifest then decides whether a type is its own
/// ([`provider_proof`]). The last schema is the desired one.
fn declared_packages<'a>(schemas: &[&'a Schema]) -> BTreeMap<&'a str, Vec<(&'a str, &'a str)>> {
    let mut by_name: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    for schema in schemas {
        for (name, definition) in &schema.extensions {
            by_name.insert(
                name.raw(),
                (definition.schema.as_str(), definition.version.as_str()),
            );
        }
    }
    let mut by_schema: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for (name, (schema, version)) in by_name {
        by_schema.entry(schema).or_default().push((name, version));
    }
    by_schema
}

/// Reads each reference. One that the target lacks, in the schema of exactly
/// one package the project declares, is taken as that package's and returned
/// as a placeholder pin: the order puts the create first, and the sealed manifest
/// must list the type (DEC-2447.12).
async fn read_or_defer(
    conn: &mut Conn,
    references: &[TypeRef],
    schemas: &[&Schema],
) -> Result<(Vec<UserType>, Vec<TypePin>), DbError> {
    let created = declared_packages(schemas);
    let reads = user_types::read(conn, Within::OwnTransaction, references).await;
    let mut types = Vec::new();
    let mut planned = Vec::new();
    for (read, reference) in reads.into_iter().zip(references) {
        match read {
            TypeRead::Present(ty) => types.push(*ty),
            TypeRead::Absent if created.contains_key(reference.schema()) => {
                match created[reference.schema()].as_slice() {
                    [(extension, version)] => planned.push(TypePin {
                        schema: reference.schema().to_owned(),
                        name: reference.name().to_owned(),
                        oid: 0,
                        kind: PLANNED.to_owned(),
                        digest: String::new(),
                        provider: Some(TypeProvider {
                            extension: (*extension).to_owned(),
                            version: (*version).to_owned(),
                            moved_to: None,
                        }),
                    }),
                    several => {
                        return Err(failed(format!(
                            "the type `{}` is not in the target database and the project declares \
                             {} packages in schema `{}` ({}); say which provides it by declaring \
                             them in separate schemas",
                            reference.shown(),
                            several.len(),
                            Escaped(reference.schema()),
                            several
                                .iter()
                                .map(|(n, _)| format!("`{}`", Escaped(n)))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )));
                    }
                }
            }
            other @ (TypeRead::Absent
            | TypeRead::NotDefined
            | TypeRead::Unreadable(_)
            | TypeRead::NotAdmitted(_)) => {
                return Err(failed(
                    other
                        .refusal(reference)
                        .expect("a non-present type refuses"),
                ));
            }
        }
    }
    Ok((types, planned))
}

/// The proof after the rehearsal for each placeholder pin: the package the plan
/// creates, or moves and updates, must list that exact type among its sealed
/// members (class `type`, as the engine names it), so a type of the same
/// spelling from anywhere else does not stand in for the one the order assumed
/// (DEC-2447.12). A created package is held to its manifest, a changed one to
/// the manifest it has after the change.
pub fn provider_proof(
    pins: &[TypePin],
    created: &[pbps_model::ExtensionPin],
    changed: &[pbps_model::ExtensionChangePin],
) -> Result<(), String> {
    for pin in pins.iter().filter(|p| is_planned(p)) {
        let Some(provider) = &pin.provider else {
            return Err(format!(
                "the type pin for `{}.{}` names no package",
                Escaped(&pin.schema),
                Escaped(&pin.name)
            ));
        };
        // The engine names a type as one qualified name.
        let reference = TypeRef::new(&pin.schema, &pin.name).map_err(|e| e.to_string())?;
        let member = pbps_model::ProviderMember {
            class: "type".to_owned(),
            names: vec![reference.raw().to_string()],
            args: Vec::new(),
        };
        let manifest = created
            .iter()
            .find(|e| e.name.raw() == provider.extension)
            .map(|e| &e.manifest)
            .or_else(|| {
                changed
                    .iter()
                    .find(|e| e.name.raw() == provider.extension)
                    .map(|e| &e.after)
            })
            .ok_or_else(|| {
                format!(
                    "the type `{}.{}` is taken as a member of the extension `{}`, which this \
                     plan neither creates nor changes",
                    Escaped(&pin.schema),
                    Escaped(&pin.name),
                    Escaped(&provider.extension)
                )
            })?;
        if !manifest.members.contains(&member) {
            return Err(format!(
                "the type `{}.{}` is not a member of the extension `{}` after the plan's change, \
                 so a column cannot rely on this plan to bring it",
                Escaped(&pin.schema),
                Escaped(&pin.name),
                Escaped(&provider.extension)
            ));
        }
    }
    Ok(())
}

/// A provided type is usable only when the project manages its package at the
/// version the target holds: a package nobody opted in has no proof route
/// (DEC-2447.3, DEC-2447.12).
fn check_providers(schemas: &[&Schema], types: &[UserType]) -> Result<(), String> {
    for ty in types {
        let Some((extension, version)) = ty.provider() else {
            continue;
        };
        let declared = schemas.iter().find_map(|s| {
            s.extensions
                .iter()
                .find(|(name, _)| name.raw() == extension)
                .map(|(_, definition)| definition)
        });
        match declared {
            None => {
                return Err(format!(
                    "the type `{}` belongs to the extension `{}`, which this project does not \
                     manage; opt it in with `pbps pull --extension {}` and record it with \
                     `pbps baseline`, then plan again",
                    ty.reference().shown(),
                    Escaped(extension),
                    Escaped(extension),
                ));
            }
            Some(definition) if definition.version != version => {
                return Err(format!(
                    "the type `{}` belongs to the extension `{}` at version `{}`, and the \
                     project records version `{}`; bring them together before planning",
                    ty.reference().shown(),
                    Escaped(extension),
                    Escaped(version),
                    Escaped(&definition.version),
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// A column is not retyped to or from a type a package provides (DEC-2447.12):
/// the conversion may run the package's casts, and which of them is safe is a
/// separate, measured decision (`citext` to `text` is binary-coercible, and
/// `boolean` to `citext` calls a package function). Named, never guessed.
fn refuse_retype_of_provided(
    types: &[UserType],
    planned: &[TypePin],
    changes: &ChangeSet,
) -> Result<(), String> {
    let provided = |ty: &ColumnType| {
        ty.type_ref().is_some_and(|reference| {
            types
                .iter()
                .any(|t| t.provider().is_some() && t.reference() == reference)
                || planned
                    .iter()
                    .any(|p| p.schema == reference.schema() && p.name == reference.name())
        })
    };
    for change in &changes.changes {
        if let Change::AlterColumnType {
            column, from, to, ..
        } = &change.change
            && (provided(from) || provided(to))
        {
            return Err(format!(
                "column `{}`.`{}` is retyped to or from a type an extension provides; changing \
                 a column's type through a package's casts is not supported yet, so add the \
                 column with the new type, move the data and drop the old one in separate plans",
                column.table.shown(),
                column.name.shown()
            ));
        }
    }
    Ok(())
}

/// A package is not updated in the plan that holds a column of one of its
/// types (DEC-2447.12): an update can change what the type does (measured:
/// `hstore` 1.4 to 1.8 adds its subscript handler), the type's pin covers that,
/// and a pin that moves inside the plan that approved it can never be checked.
/// The update goes in its own plan, which re-reads and re-pins the type.
fn refuse_update_beside_provided(types: &[UserType], changes: &ChangeSet) -> Result<(), String> {
    for planned in &changes.changes {
        let Change::UpdateExtension { name, .. } = &planned.change else {
            continue;
        };
        if let Some(ty) = types.iter().find(|t| {
            t.provider()
                .is_some_and(|(extension, _)| extension == name.raw())
        }) {
            return Err(format!(
                "this plan updates extension `{}` and also uses its type `{}`; an update can \
                 change what the type does, so update the package in a plan of its own first",
                name.shown(),
                ty.reference().shown()
            ));
        }
    }
    Ok(())
}

async fn seal_references(
    conn: &mut Conn,
    schemas: &[&Schema],
    references: Vec<TypeRef>,
    changes: &ChangeSet,
) -> Result<Vec<TypePin>, DbError> {
    let (types, planned) = read_or_defer(conn, &references, schemas).await?;
    check_providers(schemas, &types).map_err(failed)?;
    refuse_retype_of_provided(&types, &planned, changes).map_err(failed)?;
    refuse_update_beside_provided(&types, changes).map_err(failed)?;
    for schema in schemas {
        check_defaults(schema, &types).map_err(failed)?;
    }
    // Names alone conflate overloads and objects in different system catalogs.
    let mut managed_modules = BTreeSet::new();
    for schema in schemas {
        for (id, module) in &schema.modules {
            let catalog = match module.kind {
                ModuleKind::View => "pg_class",
                ModuleKind::Function | ModuleKind::Procedure => "pg_proc",
                ModuleKind::Trigger => "pg_trigger",
            };
            if types.iter().flat_map(UserType::reached).any(|r| {
                r.catalog == catalog
                    && r.schema.as_deref() == Some(id.schema())
                    && r.name.as_deref() == Some(id.name())
            }) && let Some(oid) = crate::modules::module_oid(conn, id, module.kind).await?
            {
                managed_modules.insert((catalog, oid));
            }
        }
    }
    for ty in &types {
        if let Some(reached) = ty.reaches_managed(|r| {
            r.catalog == "pg_class"
                && schemas.iter().any(|s| {
                    s.tables.keys().any(|t| {
                        r.schema.as_deref() == Some(&t.schema) && r.name.as_deref() == Some(&t.name)
                    })
                })
                || managed_modules.contains(&(r.catalog.as_str(), r.oid))
        }) {
            return Err(failed(format!(
                "type `{}` depends on managed {} `{}`; a referenced type cannot depend on an object pbps changes",
                ty.reference().shown(),
                reached.kind,
                reached.identity
            )));
        }
    }
    let mut pins: Vec<TypePin> = types.iter().map(pin).chain(planned).collect();
    note_moves(&mut pins, changes);
    Ok(pins)
}

/// A pin of a type whose package this plan moves says where the type is once
/// the move has run, so that every later read of it can find it (DEC-2447.12).
fn note_moves(pins: &mut [TypePin], changes: &ChangeSet) {
    for pin in pins.iter_mut().filter(|p| !is_planned(p)) {
        let Some(provider) = &mut pin.provider else {
            continue;
        };
        provider.moved_to = changes.changes.iter().find_map(|planned| {
            let Change::SetExtensionSchema { name, from, to, .. } = &planned.change else {
                return None;
            };
            (name.raw() == provider.extension && from.schema == pin.schema)
                .then(|| to.schema.clone())
        });
    }
}

/// Where each non-placeholder pin's type is now: under its own name, or under
/// the name its package's move gave it, whichever the catalog holds. The type
/// keeps its OID through the move, so exactly one of the two names is there.
pub async fn current_references(
    conn: &mut Conn,
    within: Within,
    pins: &[TypePin],
) -> Result<Vec<TypeRef>, DbError> {
    let mut out = Vec::new();
    for pin in pins.iter().filter(|p| !is_planned(p)) {
        let own = TypeRef::new(&pin.schema, &pin.name).map_err(|e| failed(e.to_string()))?;
        let moved = match pin.provider.as_ref().and_then(|p| p.moved_to.as_deref()) {
            Some(schema) => {
                Some(TypeRef::new(schema, &pin.name).map_err(|e| failed(e.to_string()))?)
            }
            None => None,
        };
        let Some(moved) = moved else {
            out.push(own);
            continue;
        };
        let candidates = [own.clone(), moved.clone()];
        let reads = user_types::read(conn, within, &candidates).await;
        out.push(if matches!(reads.get(1), Some(TypeRead::Present(_))) {
            moved
        } else {
            own
        });
    }
    Ok(out)
}

fn require_references(
    references: &[TypeRef],
    declared: &[pbps_model::DeclaredTypePin],
    changes: &ChangeSet,
) -> Result<(), DbError> {
    let (base_ids, desired_ids) = self::declared::identities(declared, changes);
    let schema = Schema::default();
    let required = external_references(
        Side {
            schema: &schema,
            ids: &base_ids,
        },
        Side {
            schema: &schema,
            ids: &desired_ids,
        },
        changes,
    );
    for reference in &required {
        if !references.contains(reference) {
            return Err(failed(format!(
                "plan has no type pin for `{reference}`; plan again against the database",
                reference = reference.shown(),
            )));
        }
    }
    Ok(())
}

pub async fn check(
    conn: &mut Conn,
    within: Within,
    expected: &[TypePin],
    declared: &[pbps_model::DeclaredTypePin],
    changes: &ChangeSet,
) -> Result<(), DbError> {
    let references: Vec<_> = expected
        .iter()
        .map(|p| TypeRef::new(&p.schema, &p.name).map_err(|e| failed(e.to_string())))
        .collect::<Result<_, _>>()?;
    require_references(&references, declared, changes)?;
    // A placeholder names a type the plan itself brings: nothing to read yet.
    let expected: Vec<&TypePin> = expected.iter().filter(|p| !is_planned(p)).collect();
    let owned: Vec<TypePin> = expected.iter().map(|p| (*p).clone()).collect();
    let references = current_references(conn, within, &owned).await?;
    let actual: Vec<_> = read(conn, within, &references)
        .await?
        .iter()
        .map(pin)
        .collect();
    for (old, new) in expected.iter().copied().zip(&actual) {
        if !same_pin(old, new) {
            return Err(failed(format!(
                "the referenced type `{}.{}` changed since planning; plan again",
                old.schema, old.name
            )));
        }
    }
    Ok(())
}

/// A pin read again is the pin that was sealed, except that a type its package's
/// move carried is in the schema the plan said it would be (DEC-2447.12).
fn same_pin(sealed: &TypePin, now: &TypePin) -> bool {
    // Before the move, the type is where it was and the pin is what was sealed,
    // less the note of where it will be.
    let mut before = sealed.clone();
    if let Some(provider) = &mut before.provider {
        provider.moved_to = None;
    }
    if before == *now {
        return true;
    }
    // After it, the type is in the schema the plan said and nothing else moved.
    let moved = sealed
        .provider
        .as_ref()
        .and_then(|p| p.moved_to.as_deref())
        .is_some_and(|schema| schema == now.schema);
    moved
        && TypePin {
            schema: now.schema.clone(),
            ..before
        } == *now
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fill {
    Null,
    EmptyArray,
    Constructs,
}

// #2155: admission and erased-NULL sealing must agree for every value source.
// Unrecognised shapes keep the full input-path check; no expression runs here.
fn fill(column: &Column) -> Fill {
    let expr = match &column.value_source {
        ValueSource::Default(expr) => expr,
        ValueSource::Generated(g) if g.stored => &g.expression,
        ValueSource::None
        | ValueSource::Identity(_)
        | ValueSource::Sequence(_)
        | ValueSource::Generated(_) => return Fill::Constructs,
    };
    let (core, casts) = crate::rows::unwrapped_with_casts(expr);
    if !casts.iter().all(|cast| {
        cast.parse::<ColumnType>()
            .ok()
            .and_then(|ty| crate::types::normalize(&ty).ok())
            .is_some_and(|ty| ty == column.ty)
    }) {
        return Fill::Constructs;
    }
    if core.eq_ignore_ascii_case("null") {
        return Fill::Null;
    }
    let empty = core == "'{}'"
        || (core
            .get(..6)
            .is_some_and(|s| s.eq_ignore_ascii_case("array["))
            && core.ends_with(']')
            && core[6..core.len() - 1]
                .bytes()
                .all(|b| b.is_ascii_whitespace()));
    // A domain over an array still evaluates its own CHECK.
    if empty && column.ty.is_array() && !casts.is_empty() {
        Fill::EmptyArray
    } else {
        Fill::Constructs
    }
}

/// Potential input paths, independent of the change kind or fill (DEC-2181.1).
/// Both construction and the declared type's own DEFAULT must be eligible.
/// Returns the routines that only a review admits (rule 4 of
/// [`crate::admission`]), which the plan shows and pins.
pub async fn input_routines(
    conn: &mut Conn,
    within: Within,
    references: &[TypeRef],
    admission: &Admission,
) -> Result<BTreeSet<i64>, DbError> {
    use crate::input_path::InputPath;
    let answers = input_answers(conn, within, references, admission).await?;
    let mut roots = BTreeSet::new();
    for (index, answer) in answers.into_iter().enumerate() {
        if let InputPath::Admitted(admitted) = &answer {
            roots.extend(admitted.reviewed());
        } else if let Some(why) = answer.refusal(&references[index / 2], admission) {
            return Err(failed(why));
        }
    }
    Ok(roots)
}

/// A bound tree can prove ineligible code, or fail to supply a verdict.
#[derive(Debug, thiserror::Error)]
pub enum CodeVetError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Unreadable(String),
}

/// Advisory scratch vet: target owner trust is checked only inside apply.
/// Uses no trusted roles and never reviews a routine (DEC-2309.1).
pub async fn vet_code(
    conn: &mut Conn,
    within: Within,
    references: &[TypeRef],
) -> Result<(), CodeVetError> {
    let admission = Admission::without_review([]);
    let answers = input_answers(conn, within, references, &admission)
        .await
        .map_err(|error| CodeVetError::Unreadable(error.to_string()))?;
    for (index, answer) in answers.into_iter().enumerate() {
        code_answer(answer, &references[index / 2], &admission)?;
    }
    Ok(())
}

/// Only the routines are judged here: the scratch's owners are not the target's.
fn code_only(mut answer: crate::input_path::InputPath) -> crate::input_path::InputPath {
    if let crate::input_path::InputPath::Refused(reasons) = &mut answer {
        reasons.retain(|reason| matches!(reason, crate::input_path::Reason::Routine(_)));
    }
    answer
}

fn code_answer(
    answer: crate::input_path::InputPath,
    reference: &TypeRef,
    admission: &Admission,
) -> Result<(), CodeVetError> {
    use crate::input_path::InputPath;
    let answer = code_only(answer);
    if matches!(&answer, InputPath::Refused(reasons) if reasons.is_empty()) {
        return Ok(());
    }
    match answer.refusal(reference, admission) {
        None => Ok(()),
        Some(why) if matches!(answer, InputPath::Unreadable(_)) => {
            Err(CodeVetError::Unreadable(why))
        }
        Some(why) => Err(CodeVetError::Refused(why)),
    }
}

async fn input_answers(
    conn: &mut Conn,
    within: Within,
    references: &[TypeRef],
    admission: &Admission,
) -> Result<Vec<crate::input_path::InputPath>, DbError> {
    use crate::input_path::Added;
    let types = read(conn, within, references).await?;
    let added: Vec<_> = types
        .iter()
        .flat_map(|ty| [Added::new(ty, true), Added::new(ty, false)])
        .collect();
    let answers = crate::input_path::read(conn, within, admission, &added).await;
    if answers.len() != added.len() {
        return Err(failed("the input-path read returned an incomplete answer"));
    }
    Ok(answers)
}

/// Offline reconstruction asserts prerequisites before it creates any object.
pub fn bootstrap_guard(
    schema: &Schema,
    trusted: &BTreeSet<pbps_model::RoleName>,
) -> Result<Option<String>, pbps_dialect::DialectError> {
    check_defaults(schema, &[]).map_err(|feature| pbps_dialect::DialectError::Unsupported {
        dialect: "PostgreSQL",
        feature,
    })?;
    let refs: BTreeSet<_> = references(&[schema], &ChangeSet::default())
        .into_iter()
        .filter(|r| !schema.types.contains_key(r))
        .collect();
    if refs.is_empty() && trusted.is_empty() && schema.extensions.is_empty() {
        return Ok(None);
    }
    let mut sql = String::from("BEGIN\n");
    // A type a package provides is a possible prerequisite only where the schema
    // declares a package; without one the text is what it always was.
    let (provided, kinds) = if schema.extensions.is_empty() {
        ("", " or standalone composite")
    } else {
        (
            " OR (t.typtype='b' AND EXISTS (SELECT FROM pg_catalog.pg_depend d WHERE d.classid='pg_catalog.pg_type'::pg_catalog.regclass AND d.objid=t.oid AND d.refclassid='pg_catalog.pg_extension'::pg_catalog.regclass AND d.deptype='e'))",
            ", standalone composite or a type a package provides",
        )
    };
    for reference in refs {
        let schema = crate::emit::literal(reference.schema());
        let name = crate::emit::literal(reference.name());
        let message = crate::emit::literal(&format!(
            "required type `{reference}` is missing, unusable or not an enum, domain{kinds}",
            reference = reference.shown(),
        ));
        sql.push_str(&format!("IF NOT EXISTS (SELECT FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid=t.typnamespace LEFT JOIN pg_catalog.pg_class c ON c.oid=t.typrelid WHERE n.nspname={schema} AND t.typname={name} AND t.typisdefined AND (t.typtype IN ('e','d') OR (t.typtype='c' AND c.relkind='c'){provided}) AND pg_catalog.has_schema_privilege(n.oid,'USAGE') AND pg_catalog.has_type_privilege(t.oid,'USAGE')) THEN RAISE EXCEPTION '%', {message}; END IF;\n"));
    }
    for role in trusted {
        let name = crate::emit::literal(role.raw());
        let message = crate::emit::literal(&format!(
            "trusted role `{role}` does not exist",
            role = role.shown()
        ));
        sql.push_str(&format!("IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname={name}) THEN RAISE EXCEPTION '%', {message}; END IF;\n"));
    }
    // A bootstrap script does not install packages. Verify on its replay
    // target before any managed-object DDL, just as connected bootstrap does.
    for (name, definition) in &schema.extensions {
        let package = crate::emit::literal(name.raw());
        let namespace = crate::emit::literal(&definition.schema);
        let version = crate::emit::literal(&definition.version);
        let message = crate::emit::literal(&format!(
            "required extension `{}` must already be installed at exact version `{}` in schema `{}`",
            name.shown(),
            Escaped(&definition.version),
            Escaped(&definition.schema),
        ));
        sql.push_str(&format!("IF NOT EXISTS (SELECT FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid=e.extnamespace WHERE e.extname={package} AND n.nspname={namespace} AND e.extversion={version}) THEN RAISE EXCEPTION '%', {message}; END IF;\n"));
    }
    sql.push_str("END");
    let tag = crate::emit::dollar_tag(&sql);
    Ok(Some(format!("DO {tag}\n{sql}\n{tag};")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provided(name: &str, extension: &str, version: &str) -> UserType {
        UserType::provided_for_test(name.parse().unwrap(), extension, version)
    }

    fn schema_with(extension: Option<(&str, &str, &str)>) -> Schema {
        let mut schema = Schema::default();
        if let Some((name, at, version)) = extension {
            schema.extensions.insert(
                name.into(),
                pbps_model::Extension {
                    schema: at.into(),
                    version: version.into(),
                },
            );
        }
        schema
    }

    /// A provided type is usable only against a managed package at the version
    /// the target holds; the refusal names the command that opts it in.
    #[test]
    fn a_provided_type_needs_its_package_managed_at_the_installed_version() {
        let types = [provided("ext.citext", "citext", "1.6")];
        check_providers(&[&schema_with(Some(("citext", "ext", "1.6")))], &types).unwrap();
        // Managed on either side of a comparison.
        let (base, desired) = (
            schema_with(Some(("citext", "ext", "1.6"))),
            schema_with(None),
        );
        check_providers(&[&base, &desired], &types).unwrap();
        let unmanaged = check_providers(&[&schema_with(None)], &types).unwrap_err();
        assert!(
            unmanaged.contains("does not manage")
                && unmanaged.contains("pbps pull --extension citext"),
            "{unmanaged}"
        );
        // Another package is no proof.
        let other = check_providers(&[&schema_with(Some(("hstore", "ext", "1.8")))], &types);
        assert!(other.is_err());
        let skewed =
            check_providers(&[&schema_with(Some(("citext", "ext", "1.8")))], &types).unwrap_err();
        assert!(
            skewed.contains("version `1.6`") && skewed.contains("`1.8`"),
            "{skewed}"
        );
        // A type no package provides asks nothing.
        check_providers(&[&schema_with(None)], &[]).unwrap();
    }

    /// An update of a package is refused in the plan that uses its type, and
    /// any other update, or another package's, is not.
    #[test]
    fn an_update_beside_a_use_of_the_packages_type_is_refused() {
        let update = |name: &str| ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::UpdateExtension {
                    uid: "e_aaaaaa".parse().unwrap(),
                    name: name.into(),
                    from: pbps_model::Extension {
                        schema: "ext".into(),
                        version: "1.5".into(),
                    },
                    to: pbps_model::Extension {
                        schema: "ext".into(),
                        version: "1.6".into(),
                    },
                    requires: Default::default(),
                    releases: Default::default(),
                },
            )],
        };
        let types = [provided("ext.citext", "citext", "1.5")];
        let refused = refuse_update_beside_provided(&types, &update("citext")).unwrap_err();
        assert!(refused.contains("plan of its own"), "{refused}");
        refuse_update_beside_provided(&types, &update("hstore")).unwrap();
        refuse_update_beside_provided(&[], &update("citext")).unwrap();
        refuse_update_beside_provided(&types, &ChangeSet::default()).unwrap();
    }

    /// A type a package provides is a possible prerequisite of a replay only
    /// where the schema declares a package, and the text is the old one without.
    #[test]
    fn the_bootstrap_guard_admits_a_provided_type_only_beside_a_declared_package() {
        let mut with = schema_with(Some(("citext", "ext", "1.6")));
        let mut without = Schema::default();
        for schema in [&mut with, &mut without] {
            let mut table = pbps_model::Table::default();
            table.columns.insert(
                "c".into(),
                Column::new(ColumnType::reference("ext.citext".parse().unwrap())),
            );
            schema
                .tables
                .insert(pbps_model::TableName::new("app", "t"), table);
        }
        let guard = |schema: &Schema| {
            bootstrap_guard(schema, &BTreeSet::new())
                .unwrap()
                .expect("a reference needs a guard")
        };
        let (declared, bare) = (guard(&with), guard(&without));
        assert!(
            declared.contains("t.typtype='b'") && declared.contains("a type a package provides")
        );
        assert!(!bare.contains("typtype='b'") && bare.contains("domain or standalone composite"));
    }

    fn pin_of(schema: &str, name: &str, provider: Option<(&str, Option<&str>)>) -> TypePin {
        TypePin {
            schema: schema.to_owned(),
            name: name.to_owned(),
            oid: 17,
            kind: "provided".to_owned(),
            digest: "ab".repeat(32),
            provider: provider.map(|(extension, moved_to)| TypeProvider {
                extension: extension.to_owned(),
                version: "1.6".to_owned(),
                moved_to: moved_to.map(str::to_owned),
            }),
        }
    }

    fn planned_pin(schema: &str, name: &str, extension: &str) -> TypePin {
        TypePin {
            kind: PLANNED.to_owned(),
            oid: 0,
            digest: String::new(),
            ..pin_of(schema, name, Some((extension, None)))
        }
    }

    fn manifest(members: &[(&str, &str)]) -> pbps_model::ExtensionProvider {
        pbps_model::ExtensionProvider {
            definition: pbps_model::Extension {
                schema: "ext".into(),
                version: "1.6".into(),
            },
            requires: Default::default(),
            members: members
                .iter()
                .map(|(class, name)| pbps_model::ProviderMember {
                    class: (*class).to_owned(),
                    names: vec![(*name).to_owned()],
                    args: Vec::new(),
                })
                .collect(),
        }
    }

    /// A placeholder pin is proved by the package's sealed manifest: the exact
    /// type, as a member of the package the pin names, in a package the plan
    /// creates or changes. Anything else is refused by name (DEC-2447.12).
    #[test]
    fn a_planned_type_must_be_a_member_of_the_package_the_plan_brings() {
        let create = |members: &[(&str, &str)]| pbps_model::ExtensionPin {
            uid: "e_aaaaaa".parse().unwrap(),
            name: "citext".into(),
            definition: pbps_model::Extension {
                schema: "ext".into(),
                version: "1.6".into(),
            },
            manifest: manifest(members),
        };
        let pins = [planned_pin("ext", "citext", "citext")];
        provider_proof(&pins, &[create(&[("type", "ext.citext")])], &[]).unwrap();
        // The type is not in the manifest, or only a function of that name is.
        let missing = provider_proof(&pins, &[create(&[("function", "ext.citext")])], &[]);
        assert!(missing.unwrap_err().contains("not a member"));
        // The package is not one the plan brings.
        let none = provider_proof(&pins, &[], &[]).unwrap_err();
        assert!(none.contains("neither creates nor changes"), "{none}");
        // A package the plan moves is held to the manifest it has afterwards.
        let moved = [planned_pin("moved", "citext", "citext")];
        let change = pbps_model::ExtensionChangePin {
            uid: "e_aaaaaa".parse().unwrap(),
            name: "citext".into(),
            from: pbps_model::Extension {
                schema: "ext".into(),
                version: "1.6".into(),
            },
            to: pbps_model::Extension {
                schema: "moved".into(),
                version: "1.6".into(),
            },
            path: Vec::new(),
            before: manifest(&[("type", "ext.citext")]),
            after: manifest(&[("type", "moved.citext")]),
        };
        provider_proof(&moved, &[], std::slice::from_ref(&change)).unwrap();
        assert!(provider_proof(&pins, &[], std::slice::from_ref(&change)).is_err());
        // A pin that is not a placeholder is not this check's.
        provider_proof(&[pin_of("ext", "citext", Some(("citext", None)))], &[], &[]).unwrap();
    }

    /// A type pin read again is the pin sealed, except that a type its
    /// package's move carried is where the plan said it would be; nothing else
    /// may differ, and a pin without a provider has no such allowance.
    #[test]
    fn a_pin_read_again_may_differ_only_by_the_schema_its_move_gave_it() {
        let sealed = pin_of("ext", "citext", Some(("citext", Some("moved"))));
        // Before the move, and after it.
        assert!(same_pin(
            &sealed,
            &pin_of("ext", "citext", Some(("citext", None)))
        ));
        assert!(same_pin(
            &sealed,
            &pin_of("moved", "citext", Some(("citext", None)))
        ));
        // Negative: another schema, a changed digest or OID, another package, a
        // pin that names no move.
        assert!(!same_pin(
            &sealed,
            &pin_of("other", "citext", Some(("citext", None)))
        ));
        let mut digest = pin_of("moved", "citext", Some(("citext", None)));
        digest.digest = "cd".repeat(32);
        assert!(!same_pin(&sealed, &digest));
        let mut oid = pin_of("moved", "citext", Some(("citext", None)));
        oid.oid += 1;
        assert!(!same_pin(&sealed, &oid));
        assert!(!same_pin(
            &sealed,
            &pin_of("moved", "citext", Some(("hstore", None)))
        ));
        let stays = pin_of("ext", "citext", Some(("citext", None)));
        assert!(!same_pin(
            &stays,
            &pin_of("moved", "citext", Some(("citext", None)))
        ));
    }

    /// A pin of a type whose package this plan moves says where the type will
    /// be, and only that one; a pin of another package, of a type in another
    /// schema, or a placeholder says nothing.
    #[test]
    fn a_pin_notes_the_move_of_its_own_package_and_no_other() {
        let move_of = |name: &str, from: &str, to: &str| {
            pbps_model::PlannedChange::new(pbps_model::Change::SetExtensionSchema {
                uid: "e_aaaaaa".parse().unwrap(),
                name: name.into(),
                from: pbps_model::Extension {
                    schema: from.into(),
                    version: "1.6".into(),
                },
                to: pbps_model::Extension {
                    schema: to.into(),
                    version: "1.6".into(),
                },
                releases: Default::default(),
            })
        };
        let changes = ChangeSet {
            changes: vec![move_of("citext", "ext", "moved")],
        };
        let mut pins = [
            pin_of("ext", "citext", Some(("citext", None))),
            pin_of("ext", "hstore", Some(("hstore", None))),
            pin_of("elsewhere", "citext", Some(("citext", None))),
            pin_of("ext", "enum", None),
        ];
        note_moves(&mut pins, &changes);
        let moved = |i: usize| pins[i].provider.as_ref().and_then(|p| p.moved_to.clone());
        assert_eq!(moved(0).as_deref(), Some("moved"));
        assert_eq!((moved(1), moved(2), moved(3)), (None, None, None));
    }

    /// The packages a project declares are found by the schema each is in, so a
    /// type the target lacks can be taken as one package's; two in one schema
    /// are both candidates and the read refuses to choose.
    #[test]
    fn the_declared_packages_are_grouped_by_schema_with_the_desired_side_last() {
        let at = |schema: &str, version: &str| pbps_model::Extension {
            schema: schema.into(),
            version: version.into(),
        };
        let base = Schema {
            extensions: [("citext".into(), at("ext", "1.5"))].into(),
            ..Default::default()
        };
        let desired = Schema {
            extensions: [
                ("citext".into(), at("moved", "1.6")),
                ("hstore".into(), at("moved", "1.8")),
            ]
            .into(),
            ..Default::default()
        };
        let packages = declared_packages(&[&base, &desired]);
        assert_eq!(packages.get("moved").map(Vec::len), Some(2));
        assert!(!packages.contains_key("ext"), "the desired side decides");
        assert_eq!(
            packages["moved"],
            vec![("citext", "1.6"), ("hstore", "1.8")]
        );
    }

    /// A column is not retyped to or from a type a package provides, whether
    /// the package is installed or one this plan brings.
    #[test]
    fn a_retype_to_or_from_a_provided_type_is_refused_and_any_other_is_not() {
        use pbps_model::{ColumnRef, Uid};
        let retype = |from: &str, to: &str| ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::AlterColumnType {
                    uid: "c_aaaaaa".parse::<Uid>().unwrap(),
                    column: ColumnRef {
                        table: pbps_model::TableName::new("app", "t"),
                        name: "c".into(),
                    },
                    from: from.parse().unwrap(),
                    to: to.parse().unwrap(),
                    from_nullable: true,
                    to_nullable: true,
                    from_collation: None,
                    to_collation: None,
                    through_external_partitions: false,
                    using: None,
                },
            )],
        };
        let planned = [planned_pin("ext", "citext", "citext")];
        let refused = refuse_retype_of_provided(&[], &planned, &retype("text", "ext.citext"));
        assert!(refused.unwrap_err().contains("an extension provides"));
        assert!(refuse_retype_of_provided(&[], &planned, &retype("ext.citext", "text")).is_err());
        // Negative: neither side is a provided type, or the pin is another type.
        refuse_retype_of_provided(&[], &planned, &retype("text", "integer")).unwrap();
        refuse_retype_of_provided(&[], &planned, &retype("text", "ext.other")).unwrap();
    }

    #[test]
    fn bootstrap_package_guards_pin_exact_metadata_and_quote_opaque_versions() {
        let schema = Schema {
            extensions: [(
                "vendor".into(),
                pbps_model::Extension {
                    schema: "pg_catalog".into(),
                    version: "v'$pbps$\nnext".into(),
                },
            )]
            .into(),
            ..Default::default()
        };
        let sql = bootstrap_guard(&schema, &BTreeSet::new()).unwrap().unwrap();
        for expected in [
            "pg_catalog.pg_extension",
            "e.extname=",
            "n.nspname=",
            "e.extversion=",
            "required extension",
            "DO $pbps1$",
        ] {
            assert!(sql.contains(expected), "{sql}");
        }
        assert!(!sql.contains("CREATE EXTENSION"));
        assert_eq!(
            bootstrap_guard(&Schema::default(), &BTreeSet::new()).unwrap(),
            None
        );
    }

    fn declared_default_schema(definition: pbps_model::ManagedType, array: bool) -> Schema {
        let reference = "app.value".parse().unwrap();
        let scalar = ColumnType::reference(reference);
        let mut column = Column::new(if array { scalar.into_array() } else { scalar });
        column.value_source = ValueSource::Default("NULL".into());
        Schema {
            types: [("app.value".parse().unwrap(), definition)].into(),
            tables: [(
                pbps_model::TableName::new("app", "t"),
                pbps_model::Table {
                    columns: [("value".into(), column)].into(),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        }
    }

    #[test]
    fn newly_created_types_need_no_external_pin_for_tables_and_domain_bases() {
        for array in ["", "[]"] {
            for domain in [false, true] {
                let enumeration = pbps_model::ManagedType::Enum {
                    values: vec!["ok".into()],
                };
                let mut changes = ChangeSet {
                    changes: vec![pbps_model::PlannedChange::new(Change::CreateType {
                        uid: "y_000001".parse().unwrap(),
                        name: "app.mood".parse().unwrap(),
                        definition: Box::new(enumeration),
                    })],
                };
                if domain {
                    changes
                        .changes
                        .push(pbps_model::PlannedChange::new(Change::CreateType {
                            uid: "y_000002".parse().unwrap(),
                            name: "app.choice".parse().unwrap(),
                            definition: Box::new(pbps_model::ManagedType::Domain {
                                base: format!("app.mood{array}").parse().unwrap(),
                                not_null: false,
                                default: None,
                                checks: Default::default(),
                                collation: None,
                            }),
                        }));
                }
                let ty = if domain {
                    "app.choice".to_owned()
                } else {
                    format!("app.mood{array}")
                };
                changes
                    .changes
                    .push(pbps_model::PlannedChange::new(Change::CreateTable {
                        uid: "t_000001".parse().unwrap(),
                        name: "app.t".parse().unwrap(),
                        table: Box::new(pbps_model::Table {
                            columns: [("v".into(), Column::new(ty.parse().unwrap()))].into(),
                            ..Default::default()
                        }),
                        beside_default: None,
                        moves_at_most: None,
                    }));
                require_references(&[], &[], &changes).unwrap();
                let (base, desired) = declared::identities(&[], &changes);
                assert!(base.types.is_empty());
                assert_eq!(
                    desired.type_uid(&"app.mood".parse().unwrap()),
                    Some(&"y_000001".parse().unwrap())
                );
                // A matching name without its explicit CREATE remains external.
                changes.changes.remove(0);
                let refusal = require_references(&[], &[], &changes)
                    .unwrap_err()
                    .to_string();
                assert!(refusal.contains("app.mood"), "{refusal}");
            }
        }
    }

    #[test]
    fn row_keys_resolve_the_desired_uid_after_a_type_creation_or_rename() {
        for rename in [false, true] {
            let definition = Box::new(pbps_model::ManagedType::Enum {
                values: vec!["ok".into()],
            });
            let lifecycle = if rename {
                Change::RenameType {
                    uid: "y_000001".parse().unwrap(),
                    from: "app.old".parse().unwrap(),
                    to: "app.mood".parse().unwrap(),
                    definition,
                }
            } else {
                Change::CreateType {
                    uid: "y_000001".parse().unwrap(),
                    name: "app.mood".parse().unwrap(),
                    definition,
                }
            };
            for delete in [false, true] {
                let key_type = Some("app.mood".parse().unwrap());
                let row = if delete {
                    Change::DeleteRow {
                        table: "app.t".parse().unwrap(),
                        key_column: "v".into(),
                        key: "ok".into(),
                        cause: pbps_model::change::DeleteCause::Undeclared,
                        row: Default::default(),
                        dropped: Default::default(),
                        types: Default::default(),
                        after_types: Default::default(),
                        key_type,
                    }
                } else {
                    Change::UpdateRow {
                        table: "app.t".parse().unwrap(),
                        key_column: "v".into(),
                        key: "ok".into(),
                        columns: Default::default(),
                        unchanged: Default::default(),
                        types: Default::default(),
                        after_types: Default::default(),
                        key_type,
                    }
                };
                let changes = ChangeSet {
                    changes: vec![
                        pbps_model::PlannedChange::new(lifecycle.clone()),
                        pbps_model::PlannedChange::new(row.clone()),
                    ],
                };
                require_references(&[], &[], &changes).unwrap();
                assert!(
                    require_references(
                        &[],
                        &[],
                        &ChangeSet {
                            changes: vec![pbps_model::PlannedChange::new(row)]
                        }
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn an_external_reference_on_one_side_is_not_hidden_by_a_managed_name_on_the_other() {
        let name: TypeRef = "app.mood".parse().unwrap();
        let definition = pbps_model::ManagedType::Enum {
            values: vec!["ok".into()],
        };
        let base_ids = pbps_model::IdsFile {
            types: [("y_000001".parse().unwrap(), name.clone())].into(),
            ..Default::default()
        };
        let base = Schema {
            types: [(name.clone(), definition.clone())].into(),
            ..Default::default()
        };
        for array in ["", "[]"] {
            let column = Column::new(format!("app.mood{array}").parse().unwrap());
            let table = pbps_model::Table {
                columns: [("v".into(), column)].into(),
                ..Default::default()
            };
            let desired = Schema {
                tables: [("app.t".parse().unwrap(), table.clone())].into(),
                ..Default::default()
            };
            let empty = pbps_model::IdsFile::default();
            let changes = ChangeSet {
                changes: vec![
                    pbps_model::PlannedChange::new(Change::DropType {
                        uid: "y_000001".parse().unwrap(),
                        name: name.clone(),
                        definition: Box::new(definition.clone()),
                    }),
                    pbps_model::PlannedChange::new(Change::CreateTable {
                        uid: "t_000001".parse().unwrap(),
                        name: "app.t".parse().unwrap(),
                        table: Box::new(table),
                        beside_default: None,
                        moves_at_most: None,
                    }),
                ],
            };
            let old = Side {
                schema: &base,
                ids: &base_ids,
            };
            let new = Side {
                schema: &desired,
                ids: &empty,
            };
            assert_eq!(
                external_references(old, new, &changes),
                [name.clone()].into()
            );
            assert!(require_references(&[], &[], &changes).is_err());
            require_references(std::slice::from_ref(&name), &[], &changes).unwrap();

            let managed = pbps_model::IdsFile {
                types: [("y_000002".parse().unwrap(), name.clone())].into(),
                ..Default::default()
            };
            assert!(
                external_references(
                    old,
                    Side {
                        schema: &desired,
                        ids: &managed
                    },
                    &changes
                )
                .is_empty()
            );
        }
    }

    #[test]
    fn declared_enum_null_defaults_refuse_by_column_name_while_domains_keep_them() {
        for domain in [false, true] {
            let definition = if domain {
                pbps_model::ManagedType::Domain {
                    base: "integer".parse().unwrap(),
                    not_null: false,
                    default: None,
                    checks: Default::default(),
                    collation: None,
                }
            } else {
                pbps_model::ManagedType::Enum {
                    values: vec!["ok".into()],
                }
            };
            for array in [false, true] {
                let mut schema = declared_default_schema(definition.clone(), array);
                let table = pbps_model::TableName::new("app", "t");
                let ty = schema.tables[&table].columns[&ColumnName::from("value")]
                    .ty
                    .clone();
                let canonical = ty.raw().to_string();
                for expression in ["NULL".into(), format!("NULL::{canonical}")] {
                    schema.tables.get_mut(&table).unwrap().columns[&ColumnName::from("value")]
                        .value_source = ValueSource::Default(expression);
                    let result = check_defaults(&schema, &[]);
                    if domain && !array {
                        result.unwrap();
                        assert_eq!(bootstrap_guard(&schema, &BTreeSet::new()).unwrap(), None);
                    } else {
                        let refusal = result.unwrap_err();
                        assert!(refusal.contains("app.t.value"), "{refusal}");
                        assert!(refusal.contains("NULL default the engine does not keep"));
                        assert!(bootstrap_guard(&schema, &BTreeSet::new()).is_err());
                    }
                }
                // The kind check must not refuse a declaration with no default.
                schema.tables.get_mut(&table).unwrap().columns[&ColumnName::from("value")]
                    .value_source = ValueSource::None;
                check_defaults(&schema, &[]).unwrap();
                assert_eq!(bootstrap_guard(&schema, &BTreeSet::new()).unwrap(), None);
            }
        }
    }

    #[test]
    fn declared_enum_partition_null_defaults_refuse_by_partition_name() {
        let mut schema = declared_default_schema(
            pbps_model::ManagedType::Enum {
                values: vec!["ok".into()],
            },
            false,
        );
        let parent = pbps_model::TableName::new("app", "t");
        schema.tables.get_mut(&parent).unwrap().columns[&ColumnName::from("value")].value_source =
            ValueSource::None;
        schema.tables.insert(
            pbps_model::TableName::new("app", "low"),
            pbps_model::Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent,
                    bound: pbps_model::PartitionBound::Default,
                    columns: [(
                        "value".into(),
                        pbps_model::PartitionColumn {
                            default: Some("NULL".into()),
                            not_null: false,
                        },
                    )]
                    .into(),
                }),
                ..Default::default()
            },
        );
        let refusal = check_defaults(&schema, &[]).unwrap_err();
        assert!(refusal.contains("app.low.value"), "{refusal}");
        assert!(bootstrap_guard(&schema, &BTreeSet::new()).is_err());
        schema.types.insert(
            "app.value".parse().unwrap(),
            pbps_model::ManagedType::Domain {
                base: "integer".parse().unwrap(),
                not_null: false,
                default: None,
                checks: Default::default(),
                collation: None,
            },
        );
        check_defaults(&schema, &[]).unwrap();
        assert_eq!(bootstrap_guard(&schema, &BTreeSet::new()).unwrap(), None);
    }

    #[test]
    fn defaults_and_stored_generated_fills_share_the_same_cast_checked_shapes() {
        for ty in ["app.checked", "app.checked[]"] {
            for expr in [
                "NULL".to_owned(),
                format!("NULL::{ty}"),
                format!("CAST(NULL AS {ty})"),
                format!("(NULL::{ty})::{ty}"),
            ] {
                for source in [
                    ValueSource::Default(expr.clone()),
                    ValueSource::Generated(pbps_model::Generated {
                        expression: expr.clone(),
                        stored: true,
                    }),
                ] {
                    let mut column = Column::new(ty.parse().unwrap());
                    column.value_source = source;
                    assert_eq!(fill(&column), Fill::Null, "{ty}: {:?}", column.value_source);
                }
            }
        }
        for (ty, expr, expected) in [
            ("app.checked[]", "ARRAY[]::app.checked[]", Fill::EmptyArray),
            (
                "app.checked[]",
                "ARRAY[ \t\n ]::app.checked[]",
                Fill::EmptyArray,
            ),
            (
                "app.checked[]",
                "CAST(ARRAY[] AS app.checked[])",
                Fill::EmptyArray,
            ),
            ("app.checked[]", "'{}'::app.checked[]", Fill::EmptyArray),
            (
                "app.checked[]",
                "CAST('{}' AS app.checked[])",
                Fill::EmptyArray,
            ),
            (
                "app.checked[]",
                "(ARRAY[]::app.checked[])::app.checked[]",
                Fill::EmptyArray,
            ),
            (
                "app.checked[]",
                "ARRAY[NULL]::app.checked[]",
                Fill::Constructs,
            ),
            (
                "app.checked[]",
                "ARRAY['x']::app.checked[]",
                Fill::Constructs,
            ),
            ("app.checked[]", "'{x}'::app.checked[]", Fill::Constructs),
            ("app.checked[]", "'{{}}'::app.checked[]", Fill::Constructs),
            (
                "app.checked[]",
                "ARRAY[ARRAY[]]::app.checked[]",
                Fill::Constructs,
            ),
            (
                "app.checked[]",
                "app.empty_array()::app.checked[]",
                Fill::Constructs,
            ),
            ("app.checked[]", "'{}'", Fill::Constructs),
            ("app.checked[]", "ARRAY[]", Fill::Constructs),
            ("app.checked[]", "ARRAY[]::app.other[]", Fill::Constructs),
            (
                "app.checked[]",
                "ARRAY[]::app.other[]::app.checked[]",
                Fill::Constructs,
            ),
            (
                "app.checked[]",
                "ARRAY[\u{a0}]::app.checked[]",
                Fill::Constructs,
            ),
            (
                "app.checked",
                "NULL::app.domain_over_checked",
                Fill::Constructs,
            ),
            (
                "app.checked",
                "NULL::app.domain_over_checked::app.checked",
                Fill::Constructs,
            ),
            (
                "app.checked",
                "CAST(NULL::app.domain_over_checked AS app.checked)",
                Fill::Constructs,
            ),
            (
                "app.outer_domain",
                "'{}'::app.outer_domain",
                Fill::Constructs,
            ),
            (
                "app.outer_domain",
                "ARRAY[]::app.checked[]",
                Fill::Constructs,
            ),
        ] {
            for source in [
                ValueSource::Default(expr.into()),
                ValueSource::Generated(pbps_model::Generated {
                    expression: expr.into(),
                    stored: true,
                }),
            ] {
                let mut column = Column::new(ty.parse().unwrap());
                column.value_source = source;
                assert_eq!(fill(&column), expected, "{ty}: {:?}", column.value_source);
            }
        }
    }

    #[test]
    fn sources_without_a_stored_fill_expression_construct_values() {
        for source in [
            ValueSource::None,
            ValueSource::Identity(pbps_model::Identity::new(1, 1)),
            ValueSource::Sequence(pbps_model::OwnedSequence::serial()),
            ValueSource::Generated(pbps_model::Generated {
                expression: "'{}'::app.checked[]".into(),
                stored: false,
            }),
        ] {
            let mut column = Column::new("app.checked[]".parse().unwrap());
            column.value_source = source;
            assert_eq!(fill(&column), Fill::Constructs, "{:?}", column.value_source);
        }
    }

    #[test]
    fn bootstrap_guards_only_references_and_choose_a_tag_the_names_cannot_close() {
        let mut schema = Schema::default();
        assert_eq!(bootstrap_guard(&schema, &BTreeSet::new()).unwrap(), None);
        let mut table = pbps_model::Table::default();
        table.columns.insert(
            "v".into(),
            pbps_model::Column::new(ColumnType::reference(
                TypeRef::new("app", "x$pbps$y").unwrap(),
            )),
        );
        schema
            .tables
            .insert(pbps_model::TableName::new("app", "t"), table);
        let sql = bootstrap_guard(&schema, &BTreeSet::new()).unwrap().unwrap();
        assert!(sql.starts_with("DO $pbps1$"), "{sql}");
        assert_eq!(sql.matches("$pbps1$").count(), 2);
        assert!(
            sql.contains("t.typisdefined")
                && sql.contains("has_type_privilege")
                && sql.contains("c.relkind='c'")
        );
        assert_eq!(references(&[&schema], &ChangeSet::default()).len(), 1);
    }

    #[test]
    fn referenced_type_keys_and_cast_fallback_keep_raw_names_distinct() {
        let raw = TypeRef::new("app", "type\n\\\u{1b}\"名前").unwrap();
        let visible = TypeRef::new("app", r#"type\n\\\u{1b}"名前"#).unwrap();
        let ty = ColumnType::reference(raw.clone());
        let other = ColumnType::reference(visible.clone());
        let table = pbps_model::Table {
            columns: [
                ("raw".into(), Column::new(ty.clone())),
                ("shown".into(), Column::new(other.clone().into_array())),
            ]
            .into(),
            ..Default::default()
        };
        let schema = Schema {
            tables: [(pbps_model::TableName::new("app", "t"), table)].into(),
            ..Default::default()
        };
        assert_eq!(
            references(&[&schema], &ChangeSet::default()),
            BTreeSet::from([raw.clone(), visible])
        );
        let canonical = "app.\"type\n\\\u{1b}\"\"名前\"";
        let mut column = Column::new(ty);
        column.value_source = ValueSource::Default(format!("NULL::{canonical}"));
        // The existing cast recognizer conservatively leaves quoted control
        // characters to the full input-path check; changing that parser is
        // outside the mechanical name migration (#2084 step 1c).
        assert_eq!(fill(&column), Fill::Constructs);
        column.value_source = ValueSource::Default(format!("NULL::{}", other.raw()));
        assert_eq!(fill(&column), Fill::Constructs);
        let pins = [pbps_model::TypePin {
            schema: raw.schema().into(),
            name: raw.name().into(),
            oid: 1,
            kind: "enum".into(),
            digest: "ab".repeat(32),
            provider: None,
        }];
        let from_pin = TypeRef::new(&pins[0].schema, &pins[0].name).unwrap();
        assert_eq!(from_pin, raw);
        assert!(references(&[&schema], &ChangeSet::default()).contains(&from_pin));
    }
}

#[cfg(test)]
mod code_only_tests {
    use super::*;
    use crate::input_path::{
        InputPath, OwnerReason, Part, Reason, RoutineKind, RoutineOwnerReason, RoutineReason,
        Unreadable, Use, Why,
    };

    #[test]
    fn scratch_code_vetting_ignores_owners_keeps_routines_and_refuses_unreadable_trees() {
        let owner = Reason::Owner(OwnerReason {
            ty: "app.d".into(),
            owner: "deployer".into(),
            actor: "deployer".into(),
        });
        let routine_owner = Reason::RoutineOwner(RoutineOwnerReason {
            kind: RoutineKind::Function,
            identity: "app.f(text)".into(),
            owner: "deployer".into(),
            actor: "deployer".into(),
        });
        assert_eq!(
            code_only(InputPath::Refused(vec![
                owner.clone(),
                routine_owner.clone()
            ])),
            InputPath::Refused(vec![])
        );
        for why in [Why::NoSearchPath, Why::SecurityDefiner, Why::RunsSql] {
            let routine = Reason::Routine(RoutineReason {
                kind: RoutineKind::Function,
                identity: "app.f(text)".into(),
                runs: None,
                owner: "deployer".into(),
                why,
                oid: 16384,
                used_by: Use {
                    ty: "app.d".into(),
                    part: Part::Default,
                },
                detail: None,
            });
            assert_eq!(
                code_only(InputPath::Refused(vec![
                    owner.clone(),
                    routine.clone(),
                    routine_owner.clone()
                ])),
                InputPath::Refused(vec![routine])
            );
        }
        let unreadable = InputPath::Unreadable(Unreadable::Failed("unknown tree".into()));
        assert!(matches!(
            code_answer(
                unreadable.clone(),
                &"app.d".parse().unwrap(),
                &Admission::without_review([])
            ),
            Err(CodeVetError::Unreadable(_))
        ));
        assert_eq!(code_only(unreadable.clone()), unreadable);
        assert!(
            code_only(unreadable)
                .refusal(&"app.d".parse().unwrap(), &Admission::without_review([]))
                .unwrap()
                .contains("unknown tree")
        );
    }
}
