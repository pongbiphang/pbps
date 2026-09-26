//! The desired managed namespace, compiled on scratch (ADR-0016 decision 2;
//! issue #613).
//!
//! The statements are the emitter's own: a fresh bootstrap of the declared
//! schema, so what scratch binds is what these declarations bind when they are
//! created. Only the *order* is this module's. The ordinary plan order puts
//! tables, with their defaults, checks and indexes, ahead of every module
//! (`order_key` in pbps-diff), and an expression calling a managed function
//! then either fails or binds whatever else of that name exists already —
//! the very answer this analysis must not get wrong. So a table is created
//! bare, the modules follow, and the expressions come last, when everything
//! they can name exists.
//!
//! Between modules the emitter's order stands: the name scan that already
//! orders a plan's modules (`creation_order_with`). A scan can miss a
//! reference, and a module compiled before a same-named object it would have
//! preferred binds something else without complaint. That cannot be
//! prevented here without parsing SQL, so it is detected afterwards from what
//! the engine actually bound: [`Reconstruction::later_names`] is what the
//! assessment checks each module's bindings against. Scratch order is only
//! how the namespace is built; it decides nothing about deployment order.
//!
//! Grants, roles and rows are not reproduced: none of them changes what a
//! name binds to. What does — the deployer's schema privileges and path — is
//! reproduced by the analysis scope before this runs (#610).

use pbps_db::resolver::capture::ObjectIdentity;
use pbps_dialect::Dialect;
use pbps_model::{Change, ModuleId, ModuleKind, Strategy};
use std::collections::BTreeMap;

/// Why the desired namespace could not be built on scratch. A named limit,
/// never a partially compiled namespace that evidence could be read from.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReconstructError {
    #[error("the desired namespace cannot be reconstructed through a {0} change")]
    Unsupported(String),
    #[error("the emitter cannot write {declaration}: {reason}")]
    Emit { declaration: String, reason: String },
    #[error("scratch compilation of {declaration} failed: {reason}")]
    Compile { declaration: String, reason: String },
    #[error("scratch compilation could not {0} its transaction")]
    Transaction(&'static str),
}

/// When a step runs. Everything in one phase can be named by the phases
/// after it; only modules name each other within a phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Tables,
    Keys,
    Modules,
    Expressions,
}

/// What kind of object a step makes nameable, and so which bindings it
/// could have taken over had it existed earlier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Nameable {
    /// A table or view: a relation, and a row type of the same name, which
    /// can also turn a call `t(x)` without an exact match into a cast.
    Relation,
    /// An index: a relation with no row type.
    Index,
    /// A function or procedure. A call written `t(x)` is also how a type
    /// named `t` is cast to, so a routine can take a type's place too.
    Routine,
    /// A type alone, such as a relation's generated array type: never a
    /// relation, and a call only as a cast.
    Type,
}

impl Nameable {
    /// Whether an object of this kind could be what a binding of the
    /// captured catalog `class` resolves to instead. A row type takes a
    /// routine call only as a cast, so only when a one-argument call can
    /// reach the routine (`one_argument`).
    pub fn shadows(self, class: &str, one_argument: bool) -> bool {
        match (self, class) {
            (Self::Relation, "pg_proc") => one_argument,
            (Self::Relation, class) => matches!(class, "pg_class" | "pg_type"),
            (Self::Index, class) => class == "pg_class",
            (Self::Routine, class) => matches!(class, "pg_proc" | "pg_type"),
            (Self::Type, "pg_proc") => one_argument,
            (Self::Type, class) => class == "pg_type",
        }
    }
}

#[derive(Debug, Clone)]
struct Step {
    phase: Phase,
    /// What an error names: the declaration, never the statement text.
    declaration: String,
    statements: Vec<String>,
    /// What this step makes resolvable, for the order check: its kind,
    /// schema and name.
    names: Vec<(Nameable, String, String)>,
    /// The module this step compiles, if it is one.
    module: Option<ModuleId>,
    /// The routine the engine created for this step, as the capture names
    /// it. A routine's declared signature is type spellings, not catalog
    /// identities, so the step's own object is read back after it compiles:
    /// the overload the order check measures from must be this one, not the
    /// first of its name.
    created: Option<ObjectIdentity>,
}

/// The ordered scratch statements for one desired schema.
#[derive(Debug, Clone)]
pub struct Reconstruction {
    steps: Vec<Step>,
    /// Declared routines the plan drops, which are never compiled, and the
    /// catalog identity each declared signature names once compiled: `None`
    /// until then, or when an argument type cannot be identified (#1063).
    dropped: BTreeMap<ModuleId, Option<ObjectIdentity>>,
}

impl Reconstruction {
    /// `bootstrap` is the plan from an empty schema to the desired one, as
    /// the differ produces it; its order among modules is kept.
    pub fn new(dialect: &crate::Postgres, bootstrap: &[Change]) -> Result<Self, ReconstructError> {
        let mut steps = Vec::new();
        for change in bootstrap {
            match change {
                Change::CreateTable { uid, name, table } => {
                    let mut bare = (**table).clone();
                    let checks = std::mem::take(&mut bare.checks);
                    let indexes = std::mem::take(&mut bare.indexes);
                    let keys = std::mem::take(&mut bare.foreign_keys);
                    let mut defaults = Vec::new();
                    for (column, spec) in &mut bare.columns {
                        if let Some(default) = spec.default.take() {
                            defaults.push((column.clone(), default));
                        }
                    }
                    let declaration = format!("table {name}");
                    steps.push(step(
                        dialect,
                        Phase::Tables,
                        &declaration,
                        &Change::CreateTable {
                            uid: uid.clone(),
                            name: name.clone(),
                            table: Box::new(bare),
                        },
                        relation(&name.schema, &name.name),
                        None,
                    )?);
                    for (key, constraint) in keys {
                        steps.push(step(
                            dialect,
                            Phase::Keys,
                            &declaration,
                            &Change::AddForeignKey {
                                table: name.clone(),
                                name: key,
                                constraint: Box::new(constraint),
                            },
                            Vec::new(),
                            None,
                        )?);
                    }
                    for (column, default) in defaults {
                        steps.push(step(
                            dialect,
                            Phase::Expressions,
                            &format!("the default of {name}.{column}"),
                            &Change::AlterColumnDefault {
                                uid: uid.clone(),
                                column: pbps_model::ColumnRef::new(name.clone(), column),
                                from: None,
                                to: Some(default),
                            },
                            Vec::new(),
                            None,
                        )?);
                    }
                    for (check, constraint) in checks {
                        steps.push(step(
                            dialect,
                            Phase::Expressions,
                            &format!("check {check} on {name}"),
                            &Change::AddCheck {
                                table: name.clone(),
                                name: check,
                                constraint,
                            },
                            Vec::new(),
                            None,
                        )?);
                    }
                    for (index, spec) in indexes {
                        // An index is a relation: it can shadow a name a
                        // module bound, so its name joins the order check.
                        steps.push(step(
                            dialect,
                            Phase::Expressions,
                            &format!("index {index} on {name}"),
                            &Change::AddIndex {
                                table: name.clone(),
                                name: index.clone(),
                                index: Box::new(spec),
                            },
                            vec![(Nameable::Index, name.schema.clone(), index)],
                            None,
                        )?);
                    }
                }
                Change::CreateModule { id, module } => {
                    // A trigger is named by nothing and names its function
                    // with an exact signature, so it waits for everything.
                    let (phase, names, compiled) = if module.kind == ModuleKind::Trigger {
                        (Phase::Expressions, Vec::new(), None)
                    } else {
                        let names = if matches!(id, ModuleId::Routine(_)) {
                            vec![(
                                Nameable::Routine,
                                id.schema().to_owned(),
                                id.name().to_owned(),
                            )]
                        } else {
                            relation(id.schema(), id.name())
                        };
                        (Phase::Modules, names, Some(id.clone()))
                    };
                    steps.push(step(
                        dialect,
                        phase,
                        &format!("{} {id}", module.kind),
                        change,
                        names,
                        compiled,
                    )?);
                }
                // None of these changes what a name resolves to.
                Change::CreateRole { .. }
                | Change::Grant { .. }
                | Change::PublicExecution { .. }
                | Change::InsertRow { .. }
                | Change::SetDataMode { .. } => {}
                // A bootstrap from an empty schema has no renames, drops or
                // alterations; one that does is not a bootstrap.
                Change::DropTable { .. }
                | Change::RenameTable { .. }
                | Change::AddColumn { .. }
                | Change::DropColumn { .. }
                | Change::RenameColumn { .. }
                | Change::AlterColumnType { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::AddForeignKey { .. }
                | Change::DropForeignKey { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddIndex { .. }
                | Change::DropIndex { .. }
                | Change::UpdateRow { .. }
                | Change::DeleteRow { .. }
                | Change::AlterModule { .. }
                | Change::DropModule { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Revoke { .. } => {
                    return Err(ReconstructError::Unsupported(
                        serde_json::to_value(change)
                            .ok()
                            .and_then(|op| op["op"].as_str().map(str::to_owned))
                            .unwrap_or_default(),
                    ));
                }
            }
        }
        // Stable: within a phase the differ's order stands, which for modules
        // is its dependency order.
        steps.sort_by_key(|step| step.phase);
        Ok(Self {
            steps,
            dropped: BTreeMap::new(),
        })
    }

    /// The names made resolvable after the module that `owner` belongs to
    /// was compiled: later modules and every index. A binding of that module
    /// to an object of one of these names may have been made before a better
    /// candidate existed. A view is found by its name; a routine by the exact
    /// overload the engine created for its step, since overloads of one name
    /// are compiled at different steps. `None` for an object no compiled
    /// module is.
    pub fn later_names(
        &self,
        owner: &ObjectIdentity,
    ) -> Option<std::collections::BTreeSet<(Nameable, &str, &str)>> {
        let at = self
            .steps
            .iter()
            .position(|step| match owner.class.as_str() {
                "pg_proc" => step.created.as_ref() == Some(owner),
                "pg_class" => step.module.as_ref().is_some_and(|id| {
                    matches!(id, ModuleId::Named(_)) && [id.schema(), id.name()] == *owner.name
                }),
                _ => false,
            })?;
        Some(
            self.steps[at + 1..]
                .iter()
                .flat_map(|step| {
                    step.names
                        .iter()
                        .map(|(kind, schema, name)| (*kind, schema.as_str(), name.as_str()))
                })
                .collect(),
        )
    }

    /// The catalog identity of the routine compiled for `id`, once the
    /// reconstruction has compiled; `None` for a module it does not compile.
    pub fn created(&self, id: &ModuleId) -> Option<&ObjectIdentity> {
        self.steps
            .iter()
            .find(|step| step.module.as_ref() == Some(id))
            .and_then(|step| step.created.as_ref())
    }

    /// Declared routines the plan drops. Their signatures are identified
    /// after compiling, as the deployer the plan's `DROP` runs as would
    /// resolve them.
    pub fn drops(&mut self, routines: impl IntoIterator<Item = ModuleId>) {
        self.dropped
            .extend(routines.into_iter().map(|routine| (routine, None)));
    }

    /// The catalog identity a dropped routine's declared signature names, when
    /// every argument type was identified; `None` for one that was not.
    pub fn dropped(&self, id: &ModuleId) -> Option<&ObjectIdentity> {
        self.dropped.get(id).and_then(Option::as_ref)
    }

    /// Runs every step in one transaction under the dialect's session pins,
    /// as whatever role the session currently is — the reproduced deployer,
    /// which the scope has entered. Committed only when all of it compiled;
    /// a failure leaves nothing behind and names its declaration.
    pub async fn compile(
        &mut self,
        dialect: &crate::Postgres,
        conn: &mut pbps_db::transport::StreamConn,
    ) -> Result<(), ReconstructError> {
        let framing = dialect.transaction_framing();
        conn.execute(framing.begin)
            .await
            .map_err(|_| ReconstructError::Transaction("begin"))?;
        let outcome = self.compile_steps(conn).await;
        if outcome.is_err() {
            let _ = conn.execute(framing.rollback).await;
            return outcome;
        }
        conn.execute(framing.commit)
            .await
            .map_err(|_| ReconstructError::Transaction("commit"))?;
        // After the commit: a type name the engine cannot parse is an error
        // there, which inside the transaction would abort the compile. Here
        // it only leaves that routine unidentified, which counts for nothing.
        for (id, identity) in &mut self.dropped {
            if let ModuleId::Routine(routine) = id {
                *identity = signature(conn, routine).await;
            }
        }
        Ok(())
    }

    async fn compile_steps(
        &mut self,
        conn: &mut pbps_db::transport::StreamConn,
    ) -> Result<(), ReconstructError> {
        for step in &mut self.steps {
            let failed = |reason: String| ReconstructError::Compile {
                declaration: step.declaration.clone(),
                reason,
            };
            let routine = match &step.module {
                Some(id @ ModuleId::Routine(_)) => {
                    Some((id.schema().to_owned(), id.name().to_owned()))
                }
                Some(ModuleId::Named(_) | ModuleId::Trigger { .. }) | None => None,
            };
            let before = match &routine {
                Some((schema, name)) => routines(conn, schema, name).await.map_err(failed)?,
                None => Vec::new(),
            };
            for statement in &step.statements {
                conn.execute(statement)
                    .await
                    .map_err(|error| failed(error.to_string()))?;
            }
            let relation = step
                .names
                .iter()
                .find(|(kind, _, _)| *kind == Nameable::Relation)
                .map(|(_, schema, name)| (schema.clone(), name.clone()));
            if let Some((schema, name)) = relation
                && let Some((array_schema, array)) =
                    array_type(conn, &schema, &name).await.map_err(failed)?
            {
                step.names.push((Nameable::Type, array_schema, array));
            }
            if let Some((schema, name)) = &routine {
                let mut after = routines(conn, schema, name).await.map_err(failed)?;
                after.retain(|created| !before.contains(created));
                let [created] = after.as_slice() else {
                    return Err(failed(
                        "the routine it created could not be told from its overloads".into(),
                    ));
                };
                step.created = Some(created.clone());
            }
        }
        Ok(())
    }
}

/// The identity a declared routine signature names, as the capture names
/// routines, with each argument type identified the way the session's
/// deployer would identify it; `None` unless every one was.
async fn signature(
    conn: &mut pbps_db::transport::StreamConn,
    routine: &pbps_model::RoutineId,
) -> Option<ObjectIdentity> {
    let literal = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let arguments = routine
        .args
        .iter()
        .map(|arg| literal(arg.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    let rows = conn
        .query(&format!(
            "SELECT COALESCE(pg_catalog.json_agg(pg_catalog.json_build_array(tn.nspname, t.typname) ORDER BY a.ord), '[]')::text AS args, \
             pg_catalog.count(t.oid) = pg_catalog.count(*) AS complete \
             FROM pg_catalog.unnest(ARRAY[{arguments}]::pg_catalog.text[]) WITH ORDINALITY AS a(arg, ord) \
             LEFT JOIN pg_catalog.pg_type t ON t.oid = pg_catalog.to_regtype(a.arg) \
             LEFT JOIN pg_catalog.pg_namespace tn ON tn.oid = t.typnamespace"
        ))
        .await
        .ok()?;
    let row = rows.first()?;
    if row.try_get::<bool>("complete").ok().flatten() != Some(true) {
        return None;
    }
    let args = row.try_get::<&str>("args").ok().flatten()?;
    let args = serde_json::from_str::<Vec<[String; 2]>>(args).ok()?;
    Some(ObjectIdentity {
        class: "pg_proc".into(),
        name: vec![routine.name.schema.clone(), routine.name.name.clone()],
        signature: args
            .into_iter()
            .map(|type_name| ObjectIdentity {
                class: "pg_type".into(),
                name: type_name.into(),
                signature: Vec::new(),
            })
            .collect(),
    })
}

/// Every routine of one schema-qualified name, named as the capture names
/// them: schema and name, then each argument type by schema and name.
async fn routines(
    conn: &mut pbps_db::transport::StreamConn,
    schema: &str,
    name: &str,
) -> Result<Vec<ObjectIdentity>, String> {
    let literal = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let rows = conn
        .query(&format!(
            "SELECT p.proname AS name, COALESCE((SELECT pg_catalog.json_agg(pg_catalog.json_build_array(tn.nspname, t.typname) ORDER BY a.ord) \
             FROM pg_catalog.unnest(p.proargtypes::pg_catalog.oid[]) WITH ORDINALITY AS a(typ, ord) \
             JOIN pg_catalog.pg_type t ON t.oid = a.typ JOIN pg_catalog.pg_namespace tn ON tn.oid = t.typnamespace), '[]')::text AS args \
             FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = {} AND p.proname = {}",
            literal(schema),
            literal(name)
        ))
        .await
        .map_err(|error| error.to_string())?;
    rows.iter()
        .map(|row| {
            let args = row
                .try_get::<&str>("args")
                .ok()
                .flatten()
                .and_then(|args| serde_json::from_str::<Vec<[String; 2]>>(args).ok())
                .ok_or("a created routine's argument types are unreadable")?;
            Ok(ObjectIdentity {
                class: "pg_proc".into(),
                name: vec![schema.to_owned(), name.to_owned()],
                signature: args
                    .into_iter()
                    .map(|type_name| ObjectIdentity {
                        class: "pg_type".into(),
                        name: type_name.into(),
                        signature: Vec::new(),
                    })
                    .collect(),
            })
        })
        .collect()
}

/// What creating a table or view makes nameable before it compiles: the
/// relation with its row type, which always has its name. Its array type is
/// read back once it exists ([`array_type`]).
fn relation(schema: &str, name: &str) -> Vec<(Nameable, String, String)> {
    vec![(Nameable::Relation, schema.to_owned(), name.to_owned())]
}

/// The array type the engine generated for a relation's row type, by the
/// name it chose: `_name` usually, but clipped to the identifier limit, or
/// another spelling when that one was taken. `None` when it made none.
async fn array_type(
    conn: &mut pbps_db::transport::StreamConn,
    schema: &str,
    name: &str,
) -> Result<Option<(String, String)>, String> {
    let literal = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let rows = conn
        .query(&format!(
            "SELECT an.nspname AS schema, a.typname AS name \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_type r ON r.oid = c.reltype \
             JOIN pg_catalog.pg_type a ON a.oid = r.typarray \
             JOIN pg_catalog.pg_namespace an ON an.oid = a.typnamespace \
             WHERE n.nspname = {} AND c.relname = {}",
            literal(schema),
            literal(name)
        ))
        .await
        .map_err(|error| error.to_string())?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => {
            let text = |field| {
                row.try_get::<&str>(field)
                    .ok()
                    .flatten()
                    .map(str::to_owned)
                    .ok_or("a relation's array type is unreadable")
            };
            Ok(Some((text("schema")?, text("name")?)))
        }
        _ => Err("a relation's array type is ambiguous".into()),
    }
}

fn step(
    dialect: &crate::Postgres,
    phase: Phase,
    declaration: &str,
    change: &Change,
    names: Vec<(Nameable, String, String)>,
    module: Option<ModuleId>,
) -> Result<Step, ReconstructError> {
    let statements = dialect
        .emit(change, Strategy::default())
        .map_err(|error| ReconstructError::Emit {
            declaration: declaration.to_owned(),
            reason: error.to_string(),
        })?
        .into_iter()
        .map(|statement| statement.sql)
        .collect();
    Ok(Step {
        phase,
        declaration: declaration.to_owned(),
        statements,
        names,
        module,
        created: None,
    })
}

#[cfg(test)]
mod tests;
