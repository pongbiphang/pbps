//! What a connected plan has to know before it rebuilds a module
//! ([ADR-0009](../docs/ADR-0009-postgres-modules.md) §3 and §4).
//!
//! Every module change on this engine is a drop and a create — §3 works out why
//! `CREATE OR REPLACE` cannot be conditioned on anything the tool is allowed to
//! know — and a `DROP` takes with it everything the catalog attached to the
//! object. `Module::definition` describes almost none of it.
//!
//! # The rule, not the list
//!
//! > Before a rebuild — one the user's edit forced, or one §4 synthesized — the
//! > connected plan enumerates what the catalog holds for that object, carries
//! > each item into the statements it writes, and refuses when it cannot carry
//! > one.
//!
//! Three review rounds of that ADR each found the same shape one attribute
//! further out — the ACL, then the owner, then `reloptions` — and a fourth
//! found view column defaults. So what this file reads is written as an
//! enumeration of the catalog and not as a checklist, and every item it finds
//! that a `CREATE` would not put back is a **refusal**: warning and proceeding
//! would put "the application lost access" behind a line of output nobody reads
//! at 3am.
//!
//! # Why every carried item refuses today, and what changes that
//!
//! ADR-0009 §3 decides that a grant **to a declared role** comes back, by the
//! machinery ADR-0005 built — and roles and grants are Phase 5 step 6 (#81), so
//! on this dialect there is no declared grant for anything to come back from.
//! Until that step lands, an object carrying anything at all is one this
//! dialect cannot rebuild, and it says so by name. The step that adds grants
//! narrows this to what the declarations still cannot reproduce; it does not
//! remove it, because ADR-0010 §5 records that pbps cannot express "revoked
//! from `PUBLIC`" and therefore must not take it away (DECISIONS 288).
//!
//! # Nothing here is called by a command yet
//!
//! `pbps-cli` refuses this dialect outright (`main.rs`), so the caller these
//! answers are for arrives with the CLI's side of Phase 5. They are exercised
//! by the crate's own live suite, against a real server, which is the bar every
//! step of #76 is held to.

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError, Param, Row};
use pbps_model::{ModuleId, ModuleKind, ObjectName, Schema, TableName};

/// The reads run under the same empty `search_path` the pull pins, and for the
/// same reason (DECISIONS 253): `format_type` and `pg_describe_object` qualify
/// a name only when it is not visible on the path, so an operator whose path
/// includes the project's schema would get different text for the same object.
const CANONICAL_PATH: &str = "SELECT pg_catalog.set_config('search_path', '', true) AS was_set";

/// One thing the catalog holds for a module that a `DROP` takes with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Carried {
    /// The catalog this was read from, which is what makes the enumeration
    /// checkable against the ADR's table.
    pub what: &'static str,
    /// What it says, in the engine's own text.
    pub detail: String,
}

/// Whether the read of the carried state was serialized against a concurrent
/// change, and where it was not, why.
///
/// **Measured**, the mechanism differs by kind and one of them is out of reach:
/// a view takes `LOCK TABLE`; a trigger takes its parent table's; a routine is
/// not a relation (`LOCK TABLE kk.f` is `relation "kk.f" does not exist`) and
/// the row lock that would work needs `UPDATE` on `pg_proc`, which owning the
/// routine does not grant. A design that prescribed it would fail before every
/// routine rebuild for exactly the accounts this tool is built for, so the plan
/// takes the lock **when the account can** and otherwise says the rebuild is
/// not serialized, beside the object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Serialized {
    /// The lock is held, and by what.
    By(&'static str),
    /// It is not, and this is what a reviewer needs to be told.
    Not(String),
}

/// What a rebuild of one module would cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebuild {
    pub id: ModuleId,
    /// Empty means the object carries only what a `CREATE` puts back, and the
    /// rebuild is safe. Anything here is a refusal.
    pub carries: Vec<Carried>,
    pub serialized: Serialized,
}

impl Rebuild {
    /// The refusal, or `None` where there is nothing to refuse.
    ///
    /// One message naming every item, not one per item: an operator reading
    /// this is deciding what to do about the object, and three refusals about
    /// one view read as three problems.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        if self.carries.is_empty() {
            return None;
        }
        let items: Vec<String> = self
            .carries
            .iter()
            .map(|c| format!("- {}: {}", c.what, c.detail))
            .collect();
        Some(format!(
            "`{}` cannot be rebuilt: this engine has no `CREATE OR ALTER`, so every module change \
             is a drop and a create (ADR-0009 §3), and the catalog holds things for this object \
             that a `CREATE` would not put back:\n{}\nDeclare them, or undo them, and plan again. \
             Proceeding would report success while quietly changing who may use this object.",
            self.id,
            items.join("\n")
        ))
    }
}

/// Everything a rebuild of `id` would take with it, read under the object's
/// lock where this account can take one.
///
/// **Runs inside the caller's transaction, and refuses without one.** The lock
/// and the read and the rebuild have to be one serialized unit: an assertion
/// and a `DROP` are two statements, so another session can commit
/// `ALTER VIEW … SET (security_invoker = true)` between them, and the rebuild
/// then restores the stale recorded options while a post-create assertion
/// passes — because it compares against that same stale intent. Two checks
/// agreeing with each other is not the same as either being right.
///
/// **Measured**, the remedy works and the racing change is stopped:
///
/// ```text
/// BEGIN; LOCK TABLE jj.v IN ACCESS EXCLUSIVE MODE;   accepted
/// -- from another session, while it is held:
/// ALTER VIEW jj.v SET (security_invoker = true);
///     ERROR:  canceling statement due to lock timeout
/// ```
pub async fn before_a_rebuild(
    conn: &mut Conn,
    id: &ModuleId,
    kind: ModuleKind,
) -> Result<Rebuild, DbError> {
    require_the_callers_transaction(conn).await?;
    conn.query(CANONICAL_PATH).await?;
    let serialized = serialize(conn, id, kind).await?;
    let mut carries = Vec::new();
    match kind {
        ModuleKind::View => {
            read_relation(conn, id, &mut carries).await?;
            read_view_column_defaults(conn, id, &mut carries).await?;
            read_arriving_grants(conn, id, "r", &mut carries).await?;
        }
        ModuleKind::Function | ModuleKind::Procedure => {
            read_routine(conn, id, &mut carries).await?;
            read_arriving_grants(conn, id, "f", &mut carries).await?;
        }
        // A trigger has no owner and no ACL of its own — it is not a grantable
        // object — and `pg_default_acl` has no entry kind that reaches one. Its
        // whole carried state is the switch an operator can turn off.
        ModuleKind::Trigger => read_trigger_enabled(conn, id, &mut carries).await?,
    }
    Ok(Rebuild {
        id: id.clone(),
        carries,
        serialized,
    })
}

/// Takes the lock the kind allows, or says why there is none.
async fn serialize(
    conn: &mut Conn,
    id: &ModuleId,
    kind: ModuleKind,
) -> Result<Serialized, DbError> {
    match kind {
        // A view is a relation, so its own lock is the right one.
        ModuleKind::View => {
            conn.execute(&format!(
                "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
                quoted(&id.object_name())
            ))
            .await?;
            Ok(Serialized::By("the view's own ACCESS EXCLUSIVE lock"))
        }
        // A trigger is neither a relation nor a routine, and the lock that
        // serializes it is its **parent table's**. Measured: with it held,
        // `ALTER TABLE l2.t DISABLE TRIGGER audit` from another session is
        // cancelled.
        ModuleKind::Trigger => {
            let Some(on) = id.attached_to() else {
                return Ok(Serialized::Not(format!(
                    "`{id}` does not say which table it is on, so there is no lock to take"
                )));
            };
            conn.execute(&format!(
                "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
                quoted(on)
            ))
            .await?;
            Ok(Serialized::By(
                "the trigger's parent table's ACCESS EXCLUSIVE lock",
            ))
        }
        // The one that is out of reach for the accounts this tool is for.
        // Measured, as the non-superuser owner of the function:
        // `SELECT oid FROM pg_proc … FOR UPDATE` is `permission denied for
        // table pg_proc`, and as a superuser the same statement is accepted.
        ModuleKind::Function | ModuleKind::Procedure => {
            let Some(oid) = module_oid(conn, id, kind).await? else {
                return Ok(Serialized::Not(format!("`{id}` is not in the catalog")));
            };
            // Inside a savepoint, because a failed statement dooms a
            // PostgreSQL transaction and this one is *expected* to fail for
            // the accounts this tool is built for. **Measured**, as the
            // non-superuser owner of the function:
            //
            // ```text
            // BEGIN; SELECT … FROM pg_proc … FOR UPDATE;
            //     ERROR:  permission denied for table pg_proc
            // SELECT 1;
            //     ERROR:  current transaction is aborted, commands ignored …
            // ```
            //
            // Without it the attempt to serialize destroys the transaction it
            // was protecting, and every read after it fails — so a rebuild
            // that could perfectly well have gone ahead unserialized is
            // refused instead. `ROLLBACK TO SAVEPOINT` un-dooms it, which is
            // the same device ADR-0009 §3 measured for a different question.
            conn.execute("SAVEPOINT pbps_routine_lock").await?;
            let taken = conn
                .query_with(
                    "SELECT p.oid::int8 AS oid FROM pg_catalog.pg_proc p \
                     WHERE p.oid = ($1::int8)::oid FOR UPDATE",
                    &[Param::I64(oid)],
                )
                .await;
            match taken {
                Ok(_) => {
                    conn.execute("RELEASE SAVEPOINT pbps_routine_lock").await?;
                    Ok(Serialized::By(
                        "a row lock on the routine's `pg_proc` entry",
                    ))
                }
                // Not an error to the caller: refusing here would refuse every
                // routine edit, since §3 makes them all rebuilds. The residual
                // is named where the reviewer sees it instead.
                Err(e) => {
                    conn.execute("ROLLBACK TO SAVEPOINT pbps_routine_lock")
                        .await?;
                    Ok(Serialized::Not(format!(
                        "this rebuild is not serialized: a routine is not a relation, so the \
                         only lock that would serialize it is a row lock on its `pg_proc` entry, \
                         and this account cannot take one ({e}). A concurrent `ALTER FUNCTION` \
                         between this read and the `DROP` is reverted by the rebuild, and pbps \
                         cannot stop it without privileges it should not need (ADR-0009 §3)"
                    )))
                }
            }
        }
    }
}

/// A view's owner, ACL and `reloptions`.
async fn read_relation(
    conn: &mut Conn,
    id: &ModuleId,
    carries: &mut Vec<Carried>,
) -> Result<(), DbError> {
    let name = id.object_name();
    let rows = conn
        .query_with(
            "SELECT pg_catalog.pg_get_userbyid(c.relowner) AS owner,
                    COALESCE(c.relacl::text, '') AS acl,
                    COALESCE(pg_catalog.array_to_string(c.reloptions, ', '), '') AS reloptions,
                    CURRENT_USER::text AS deploying_as
               FROM pg_catalog.pg_class c
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'v'",
            &[Param::Str(&name.schema), Param::Str(&name.name)],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    push_owner(row, carries)?;
    push_acl(row, carries)?;
    // Measured: `security_invoker`, `security_barrier` and `check_option` live
    // here and in nothing else — `pg_get_viewdef` cannot show them, and
    // `Module::definition` starts after `AS`. And this one is not confined to
    // the rebuild path: `CREATE OR REPLACE VIEW` drops them too, which is why
    // §3 stops treating a replace as the cheap safe path.
    let reloptions = text(row, "reloptions")?;
    if !reloptions.is_empty() {
        carries.push(Carried {
            what: "options in `pg_class.reloptions`, which the declarations cannot express",
            detail: reloptions,
        });
    }
    Ok(())
}

/// A routine's owner and ACL.
async fn read_routine(
    conn: &mut Conn,
    id: &ModuleId,
    carries: &mut Vec<Carried>,
) -> Result<(), DbError> {
    let name = id.object_name();
    let rows = conn
        .query_with(
            "SELECT pg_catalog.pg_get_userbyid(p.proowner) AS owner,
                    COALESCE(p.proacl::text, '') AS acl,
                    p.prosecdef AS security_definer,
                    CURRENT_USER::text AS deploying_as
               FROM pg_catalog.pg_proc p
               JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
              WHERE n.nspname = $1 AND p.proname = $2
                AND COALESCE((SELECT pg_catalog.string_agg(
                                       pg_catalog.format_type(u.ty, NULL), ',' ORDER BY u.pos)
                                FROM pg_catalog.unnest(p.proargtypes)
                                       WITH ORDINALITY AS u(ty, pos)), '') = $3",
            &[
                Param::Str(&name.schema),
                Param::Str(&name.name),
                Param::Str(&signature(id)),
            ],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    push_owner(row, carries)?;
    push_acl(row, carries)?;
    Ok(())
}

/// The view column defaults that live in `pg_attrdef` and nowhere the
/// declaration can see.
///
/// **Measured**: a default set with `ALTER VIEW … ALTER COLUMN … SET DEFAULT`
/// is absent from `pg_get_viewdef` and gone after a rebuild, and an updatable
/// view then silently stops supplying that value — while `verify` reports
/// nothing, because nothing compares `pg_attrdef` either.
async fn read_view_column_defaults(
    conn: &mut Conn,
    id: &ModuleId,
    carries: &mut Vec<Carried>,
) -> Result<(), DbError> {
    let name = id.object_name();
    for row in conn
        .query_with(
            "SELECT a.attname AS column_name,
                    pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS expression
               FROM pg_catalog.pg_attrdef d
               JOIN pg_catalog.pg_class c ON c.oid = d.adrelid
               JOIN pg_catalog.pg_attribute a
                 ON a.attrelid = d.adrelid AND a.attnum = d.adnum
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'v'
              ORDER BY a.attnum",
            &[Param::Str(&name.schema), Param::Str(&name.name)],
        )
        .await?
    {
        carries.push(Carried {
            what: "a view column default in `pg_attrdef`, which the declarations cannot express",
            detail: format!(
                "column `{}` defaults to {}",
                text(&row, "column_name")?,
                text(&row, "expression")?
            ),
        });
    }
    Ok(())
}

/// A trigger's enabled state.
///
/// **Measured**: after `DISABLE`, `tgenabled` is `D`; after a drop-and-create
/// rebuild it is `O` — so an ordinary trigger edit silently reactivates
/// behaviour an operator disabled.
async fn read_trigger_enabled(
    conn: &mut Conn,
    id: &ModuleId,
    carries: &mut Vec<Carried>,
) -> Result<(), DbError> {
    let Some(on) = id.attached_to() else {
        return Ok(());
    };
    for row in conn
        .query_with(
            "SELECT tg.tgenabled::text AS enabled
               FROM pg_catalog.pg_trigger tg
               JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = $1 AND c.relname = $2 AND tg.tgname = $3
                AND NOT tg.tgisinternal",
            &[
                Param::Str(&on.schema),
                Param::Str(&on.name),
                Param::Str(id.name()),
            ],
        )
        .await?
    {
        let enabled = text(&row, "enabled")?;
        if enabled != "O" {
            carries.push(Carried {
                what: "a trigger's enabled state in `pg_trigger.tgenabled`, which the \
                       declarations cannot express",
                detail: match enabled.as_str() {
                    "D" => "the trigger is disabled".to_owned(),
                    "R" => "the trigger fires only on a replica".to_owned(),
                    "A" => "the trigger fires always, replica or not".to_owned(),
                    other => format!("`tgenabled` is `{other}`"),
                },
            });
        }
    }
    Ok(())
}

/// The grants the **new** object would arrive with, from `pg_default_acl`.
///
/// The half of the enumeration that is not about the old object at all, and the
/// one every earlier version of ADR-0009 §3 missed. **Measured**, with an
/// `ALTER DEFAULT PRIVILEGES` entry for the deploying role — the ordinary way
/// an estate arranges read access — a view that role creates arrives already
/// granted:
///
/// ```text
/// old acl NULL  ->  passes a "reproduce the old ACL" check
/// new acl        {m_deploy=…, m_bystander=r/m_deploy}
/// ```
///
/// An object with no grants at all is the easiest case to wave through, and it
/// is the one where a rebuild hands an unmanaged role `SELECT`.
async fn read_arriving_grants(
    conn: &mut Conn,
    id: &ModuleId,
    objtype: &str,
    carries: &mut Vec<Carried>,
) -> Result<(), DbError> {
    for row in conn
        .query_with(
            "SELECT da.defaclacl::text AS acl,
                    COALESCE(n.nspname, '(every schema)') AS in_schema
               FROM pg_catalog.pg_default_acl da
               LEFT JOIN pg_catalog.pg_namespace n ON n.oid = da.defaclnamespace
              WHERE da.defaclobjtype::text = $1
                AND pg_catalog.pg_get_userbyid(da.defaclrole) = CURRENT_USER::text
                AND (da.defaclnamespace = 0 OR n.nspname = $2)",
            &[Param::Str(objtype), Param::Str(id.schema())],
        )
        .await?
    {
        carries.push(Carried {
            what: "grants the replacement would arrive with, from `pg_default_acl`",
            detail: format!(
                "`ALTER DEFAULT PRIVILEGES` for this account in {} grants {}",
                text(&row, "in_schema")?,
                text(&row, "acl")?
            ),
        });
    }
    Ok(())
}

fn push_owner(row: &Row, carries: &mut Vec<Carried>) -> Result<(), DbError> {
    let owner = text(row, "owner")?;
    let deploying_as = text(row, "deploying_as")?;
    if owner == deploying_as {
        return Ok(());
    }
    // Measured: a rebuild makes the deployment account the owner, and the ACL
    // is `NULL` on both sides — so the ACL check cannot see this at all. A
    // `SECURITY DEFINER` routine keeps that flag through the rebuild and now
    // runs with the deployment account's privileges, which is the most
    // privileged principal in the environment.
    let secdef = matches!(row.try_get::<bool>("security_definer"), Ok(Some(true)));
    carries.push(Carried {
        what: "the object's owner, which a rebuild gives to the deploying account",
        detail: if secdef {
            format!(
                "`{owner}` owns it and this session is `{deploying_as}` — and it is \
                 `SECURITY DEFINER`, so the rebuild would leave it running with this account's \
                 privileges"
            )
        } else {
            format!("`{owner}` owns it and this session is `{deploying_as}`")
        },
    });
    Ok(())
}

fn push_acl(row: &Row, carries: &mut Vec<Carried>) -> Result<(), DbError> {
    let acl = text(row, "acl")?;
    if acl.is_empty() {
        return Ok(());
    }
    // In **either** direction, and the second is the one that is easy to miss.
    // A revocation is not a row in the ACL — it is the *absence* of the
    // engine's default — so a rebuild restores the default and silently
    // reopens a function somebody deliberately closed. Measured:
    // `REVOKE EXECUTE … FROM PUBLIC` leaves `{postgres=X/postgres}`, and after
    // a rebuild the ACL is `NULL` and the function answers a role that had
    // been shut out.
    carries.push(Carried {
        what: "the object's ACL, which a `DROP` destroys and no declaration here can restore",
        detail: acl,
    });
    Ok(())
}

/// The same probe the pull uses, asking the opposite question.
///
/// The pull refuses a caller's transaction because it needs its own snapshot;
/// this needs the caller's, because the lock it takes has to still be held when
/// the `DROP` runs. Both failures are the same class — a read that means
/// something different from what it looks like — so both are refused by name.
async fn require_the_callers_transaction(conn: &mut Conn) -> Result<(), DbError> {
    conn.query("SELECT pg_catalog.set_config('pbps.in_a_transaction', 'yes', true) AS was_set")
        .await?;
    let rows = conn
        .query("SELECT COALESCE(pg_catalog.current_setting('pbps.in_a_transaction', true), '') AS probe")
        .await?;
    let probe = rows.first().map(|r| text(r, "probe")).transpose()?;
    if probe.as_deref() == Some("yes") {
        return Ok(());
    }
    Err(DbError::Driver {
        code: None,
        message: "this read has to run inside the transaction that will do the rebuild, and this \
                  connection has none open.\nWhat it reads is what a `DROP` is about to destroy, \
                  and it takes the object's lock so that nothing changes between the read and the \
                  `DROP`. Outside a transaction the lock is released at the end of the statement \
                  that took it, and the canonical `search_path` these reads pin is set \
                  `is_local` and does nothing at all — so the answer would be true when it was \
                  given, unenforced afterwards, and worded by whatever path the session happened \
                  to hold (ADR-0009 §3)."
            .to_owned(),
    })
}

fn quoted(name: &TableName) -> String {
    format!("{}.{}", one_quoted(&name.schema), one_quoted(&name.name))
}

fn one_quoted(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// The argument types as `string_agg(…, ',')` joins them, which is how the
/// identity is compared in SQL.
fn signature(id: &ModuleId) -> String {
    id.args().map_or_else(String::new, |args| {
        args.iter()
            .map(pbps_model::RoutineArg::as_str)
            .collect::<Vec<_>>()
            .join(",")
    })
}

async fn module_oid(
    conn: &mut Conn,
    id: &ModuleId,
    kind: ModuleKind,
) -> Result<Option<i64>, DbError> {
    let name = id.object_name();
    let rows = match kind {
        ModuleKind::View => {
            conn.query_with(
                "SELECT c.oid::int8 AS oid
                   FROM pg_catalog.pg_class c
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'v'",
                &[Param::Str(&name.schema), Param::Str(&name.name)],
            )
            .await?
        }
        ModuleKind::Function | ModuleKind::Procedure => {
            conn.query_with(
                "SELECT p.oid::int8 AS oid
                   FROM pg_catalog.pg_proc p
                   JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
                  WHERE n.nspname = $1 AND p.proname = $2
                    AND COALESCE((SELECT pg_catalog.string_agg(
                                           pg_catalog.format_type(u.ty, NULL), ',' ORDER BY u.pos)
                                    FROM pg_catalog.unnest(p.proargtypes)
                                           WITH ORDINALITY AS u(ty, pos)), '') = $3",
                &[
                    Param::Str(&name.schema),
                    Param::Str(&name.name),
                    Param::Str(&signature(id)),
                ],
            )
            .await?
        }
        ModuleKind::Trigger => {
            let Some(on) = id.attached_to() else {
                return Ok(None);
            };
            conn.query_with(
                "SELECT tg.oid::int8 AS oid
                   FROM pg_catalog.pg_trigger tg
                   JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = $1 AND c.relname = $2 AND tg.tgname = $3
                    AND NOT tg.tgisinternal",
                &[
                    Param::Str(&on.schema),
                    Param::Str(&on.name),
                    Param::Str(id.name()),
                ],
            )
            .await?
        }
    };
    rows.first()
        .map(|r| r.try_get_at::<i64>(0))
        .transpose()
        .map(Option::flatten)
}

fn text(row: &Row, column: &str) -> Result<String, DbError> {
    Ok(row.try_get::<&str>(column)?.unwrap_or_default().to_owned())
}

// ---- §4: the dependency refusal is the common case, not the corner ----

/// What a dependent is, in terms this project can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holds {
    /// A module: a view, a routine or a trigger, keyed the way the
    /// declarations key it.
    Module(ModuleId),
    /// A part of a table: a check constraint, a column default or an index.
    /// Managed when the table declares that part, and dropped and restored
    /// around the rebuild when it is.
    TablePart { table: TableName, part: Part },
    /// Something no declaration can hold, whatever this project declares.
    ///
    /// "Managed" means **representable**, and two of the four objects
    /// ADR-0009 §4 measured are not: `Column` has no generated-expression
    /// field and `IndexColumn` is a name and a direction, so a generated column
    /// and an expression index have nothing for a planner to recreate them
    /// from. Promising to restore them would be promising to emit a statement
    /// pbps cannot write.
    Unrepresentable(String),
}

/// Which part of a table a dependent is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Check(String),
    Default(String),
    Index(String),
}

/// One object that depends on a module, established by the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependent {
    /// `pg_describe_object`'s own words, which is what a refusal shows: the
    /// engine names the object better than this crate can, and the same text
    /// appears in the `DETAIL` of the refusal the engine would give.
    pub described: String,
    pub holds: Holds,
}

impl Dependent {
    /// Whether the declarations hold this, so that a plan could drop it before
    /// the rebuild and put it back after.
    #[must_use]
    pub fn managed(&self, declared: &Schema) -> bool {
        match &self.holds {
            Holds::Module(id) => declared.modules.contains_key(id),
            Holds::TablePart { table, part } => declared.tables.get(table).is_some_and(|t| {
                match part {
                    Part::Check(name) => t.checks.contains_key(name),
                    Part::Index(name) => t.indexes.contains_key(name),
                    // A default is the column's, and the column is what the
                    // declaration names.
                    Part::Default(column) => {
                        t.columns.get(column).is_some_and(|c| c.default.is_some())
                    }
                }
            }),
            Holds::Unrepresentable(_) => false,
        }
    }
}

/// Every reverse `pg_depend` edge of one module, over **every** kind of
/// dependent and not only modules.
///
/// This enumeration was written too narrowly once — about views depending on
/// tables — and left out everything a *table* can hold that depends on a
/// function. Since §3 emits every module change as drop and create, a function
/// edit meets those dependents every time, and **measured**, all four refuse
/// the drop:
///
/// ```text
/// DROP FUNCTION with a managed check constraint on it:  refused
/// … with a column default on it:                        refused
/// … with a generated column on it:                      refused
/// … with an expression index on it:                     refused
/// ```
///
/// A plan that does not account for them is applyable and predictably fails,
/// which is the one outcome SPEC §7.5 exists to prevent.
///
/// `deptype <> 'i'` because an object's own internal edges are not dependents:
/// a view's `_RETURN` rule and its row type both point at the view itself.
///
/// **Inside the caller's transaction, like [`before_a_rebuild`], and for two
/// reasons that both matter.** The answer has to hold until the `DROP` — a
/// dependent created between this read and the rebuild is one the plan does not
/// account for, which is the applyable-and-predictably-fails outcome SPEC §7.5
/// exists to prevent. And the canonical `search_path` these reads pin is set
/// `is_local`, which outside a transaction is a statement that quietly does
/// nothing: `pg_describe_object` and `format_type` qualify a name only when it
/// is not visible on the path, so the descriptions would silently become
/// whatever the operator's session made them.
pub async fn dependents(
    conn: &mut Conn,
    id: &ModuleId,
    kind: ModuleKind,
) -> Result<Vec<Dependent>, DbError> {
    require_the_callers_transaction(conn).await?;
    conn.query(CANONICAL_PATH).await?;
    let Some(oid) = module_oid(conn, id, kind).await? else {
        return Ok(Vec::new());
    };
    let refclass = match kind {
        ModuleKind::View => "pg_catalog.pg_class",
        ModuleKind::Function | ModuleKind::Procedure => "pg_catalog.pg_proc",
        ModuleKind::Trigger => "pg_catalog.pg_trigger",
    };

    let mut args: std::collections::BTreeMap<i64, Vec<pbps_model::RoutineArg>> =
        std::collections::BTreeMap::new();
    let mut refused_args: BTreeSet<i64> = BTreeSet::new();
    for row in conn
        .query_with(&dependent_routine_args_query(refclass), &[Param::I64(oid)])
        .await?
    {
        let routine = row.try_get_at::<i64>(0)?.unwrap_or_default();
        match text(&row, "ty")?.parse() {
            Ok(arg) => args.entry(routine).or_default().push(arg),
            Err(_) => {
                refused_args.insert(routine);
            }
        }
    }

    let mut out = Vec::new();
    for row in conn
        .query_with(&dependents_query(refclass), &[Param::I64(oid)])
        .await?
    {
        let described = text(&row, "described")?;
        let unrepresentable = text(&row, "unrepresentable")?;
        let dep_schema = text(&row, "dep_schema")?;
        let dep_name = text(&row, "dep_name")?;
        let part = text(&row, "part")?;
        let dep_oid = row.try_get::<i64>("dep_oid")?.unwrap_or_default();
        let holds = if !unrepresentable.is_empty() {
            Holds::Unrepresentable(unrepresentable)
        } else {
            match text(&row, "what")?.as_str() {
                "view" => Holds::Module(ModuleId::Named(ObjectName::new(&dep_schema, &dep_name))),
                "routine" if refused_args.contains(&dep_oid) => Holds::Unrepresentable(
                    "a routine whose argument types this model cannot hold as an identity"
                        .to_owned(),
                ),
                "routine" => Holds::Module(ModuleId::Routine(pbps_model::RoutineId::new(
                    ObjectName::new(&dep_schema, &dep_name),
                    args.remove(&dep_oid).unwrap_or_default(),
                ))),
                "trigger" => Holds::Module(ModuleId::Trigger {
                    on: ObjectName::new(&dep_schema, &dep_name),
                    name: part.clone(),
                }),
                "check" => Holds::TablePart {
                    table: TableName::new(&dep_schema, &dep_name),
                    part: Part::Check(part.clone()),
                },
                "default" => Holds::TablePart {
                    table: TableName::new(&dep_schema, &dep_name),
                    part: Part::Default(part.clone()),
                },
                "index" => Holds::TablePart {
                    table: TableName::new(&dep_schema, &dep_name),
                    part: Part::Index(part.clone()),
                },
                other => Holds::Unrepresentable(format!(
                    "a dependent of a kind this reader does not know (`{other}`)"
                )),
            }
        };
        out.push(Dependent { described, holds });
    }
    Ok(out)
}

fn dependent_routine_args_query(refclass: &str) -> String {
    format!(
        "SELECT d.objid::int8 AS oid, u.pos::int8 AS pos,
                pg_catalog.format_type(u.ty, NULL) AS ty
           FROM pg_catalog.pg_depend d
           JOIN pg_catalog.pg_proc p ON p.oid = d.objid
     CROSS JOIN LATERAL pg_catalog.unnest(p.proargtypes) WITH ORDINALITY AS u(ty, pos)
          WHERE d.refclassid = '{refclass}'::regclass
            AND d.refobjid = ($1::int8)::oid
            AND d.classid = 'pg_catalog.pg_proc'::regclass
            AND d.deptype <> 'i'
          ORDER BY 1, 2"
    )
}

/// One row per dependent, over the six catalogs a dependent can live in.
///
/// `pg_describe_object` is asked for the text rather than assembled here: it is
/// the same wording the engine puts in the `DETAIL` of the refusal a plan
/// without this would hit, so a reviewer reading the plan and an operator
/// reading the failure see one description.
fn dependents_query(refclass: &str) -> String {
    let edge = format!(
        "d.refclassid = '{refclass}'::regclass AND d.refobjid = ($1::int8)::oid AND d.deptype <> 'i'"
    );
    let described = "pg_catalog.pg_describe_object(d.classid, d.objid, d.objsubid) AS described";
    format!(
        "SELECT 'view' AS what, {described}, n2.nspname AS dep_schema, c2.relname AS dep_name,
                '' AS part, 0::int8 AS dep_oid,
                CASE WHEN c2.relkind = 'm'
                     THEN 'a materialized view, which this model does not hold'
                     ELSE '' END AS unrepresentable
           FROM pg_catalog.pg_depend d
           JOIN pg_catalog.pg_rewrite r ON r.oid = d.objid
           JOIN pg_catalog.pg_class c2 ON c2.oid = r.ev_class
           JOIN pg_catalog.pg_namespace n2 ON n2.oid = c2.relnamespace
          WHERE {edge} AND d.classid = 'pg_catalog.pg_rewrite'::regclass
          UNION ALL
         SELECT 'routine', {described}, n2.nspname, p2.proname, '', p2.oid::int8,
                CASE WHEN p2.prokind NOT IN ('f', 'p')
                     THEN 'an aggregate or window function, which this model does not hold'
                     ELSE '' END
           FROM pg_catalog.pg_depend d
           JOIN pg_catalog.pg_proc p2 ON p2.oid = d.objid
           JOIN pg_catalog.pg_namespace n2 ON n2.oid = p2.pronamespace
          WHERE {edge} AND d.classid = 'pg_catalog.pg_proc'::regclass
          UNION ALL
         SELECT 'trigger', {described}, n2.nspname, c2.relname, tg.tgname, 0::int8, ''
           FROM pg_catalog.pg_depend d
           JOIN pg_catalog.pg_trigger tg ON tg.oid = d.objid
           JOIN pg_catalog.pg_class c2 ON c2.oid = tg.tgrelid
           JOIN pg_catalog.pg_namespace n2 ON n2.oid = c2.relnamespace
          WHERE {edge} AND d.classid = 'pg_catalog.pg_trigger'::regclass
            AND NOT tg.tgisinternal
          UNION ALL
         SELECT 'check', {described}, n2.nspname, c2.relname, con.conname, 0::int8,
                CASE WHEN con.contype <> 'c'
                     THEN 'a constraint of a kind that is not a check'
                     ELSE '' END
           FROM pg_catalog.pg_depend d
           JOIN pg_catalog.pg_constraint con ON con.oid = d.objid
           JOIN pg_catalog.pg_class c2 ON c2.oid = con.conrelid
           JOIN pg_catalog.pg_namespace n2 ON n2.oid = c2.relnamespace
          WHERE {edge} AND d.classid = 'pg_catalog.pg_constraint'::regclass
          UNION ALL
         SELECT 'default', {described}, n2.nspname, c2.relname, a.attname, 0::int8,
                CASE WHEN a.attgenerated <> ''
                     THEN 'a generated column, which `Column` has no field for'
                     ELSE '' END
           FROM pg_catalog.pg_depend d
           JOIN pg_catalog.pg_attrdef ad ON ad.oid = d.objid
           JOIN pg_catalog.pg_class c2 ON c2.oid = ad.adrelid
           JOIN pg_catalog.pg_attribute a
             ON a.attrelid = ad.adrelid AND a.attnum = ad.adnum
           JOIN pg_catalog.pg_namespace n2 ON n2.oid = c2.relnamespace
          WHERE {edge} AND d.classid = 'pg_catalog.pg_attrdef'::regclass
          UNION ALL
         SELECT 'index', {described}, n2.nspname, c2.relname, ic.relname, 0::int8,
                CASE WHEN i.indexprs IS NOT NULL
                     THEN 'an index over an expression, which `IndexColumn` has no field for'
                     WHEN ic.relkind <> 'i'
                     THEN 'a relation of a kind this reader does not know'
                     ELSE '' END
           FROM pg_catalog.pg_depend d
           JOIN pg_catalog.pg_class ic ON ic.oid = d.objid
           LEFT JOIN pg_catalog.pg_index i ON i.indexrelid = ic.oid
           LEFT JOIN pg_catalog.pg_class c2 ON c2.oid = i.indrelid
           LEFT JOIN pg_catalog.pg_namespace n2 ON n2.oid = c2.relnamespace
          WHERE {edge} AND d.classid = 'pg_catalog.pg_class'::regclass
          ORDER BY 2"
    )
}

/// The refusal for dependents this project could not put back, or `None`.
///
/// An affected dependent that pbps does not manage cannot be recreated from
/// anything the project holds, so dropping it would destroy an object with no
/// way back — the same shape as ADR-0005 note 10's refusal to drop a role that
/// owns something. And `DROP … CASCADE` is not the way out: it is the shortest
/// path and it destroys objects nobody reviewed (SPEC 14.3).
#[must_use]
pub fn unmanaged_refusal(
    id: &ModuleId,
    dependents: &[Dependent],
    declared: &Schema,
) -> Option<String> {
    let blocked: Vec<String> = dependents
        .iter()
        .filter(|d| !d.managed(declared))
        .map(|d| match &d.holds {
            Holds::Unrepresentable(why) => format!("- {} — {why}", d.described),
            Holds::Module(_) | Holds::TablePart { .. } => {
                format!("- {} — this project does not declare it", d.described)
            }
        })
        .collect();
    if blocked.is_empty() {
        return None;
    }
    Some(format!(
        "`{id}` cannot be rebuilt: this engine has no `CREATE OR ALTER`, so the change is a drop \
         and a create (ADR-0009 §3), and these objects depend on it and this project could not \
         put them back:\n{}\nThe plan names every object it drops, or it does not drop \
         (SPEC 14.3), so `DROP … CASCADE` is not offered. Declare them, or remove the \
         dependency, and plan again.",
        blocked.join("\n")
    ))
}

/// The managed dependents a plan has to drop before the rebuild and create
/// after it.
///
/// **The common case has no module change to rank.** A revision that retypes a
/// column a view selects produces exactly one change, and the engine's refusal
/// still fires — the differ emitted nothing for the view, so there is nothing
/// for an ordering to move. These are what a connected plan **synthesizes**,
/// and they go in the plan where the approver can see them: a view being
/// dropped and recreated is not a detail to discover at apply time.
///
/// Their cost is not the module's cost and belongs in the plan's risk list:
/// restoring a check constraint revalidates the table and rebuilding an index
/// locks it, so a one-line edit to a function can carry a table scan behind it.
/// The reviewer is approving the scan, not just the function.
#[must_use]
pub fn to_rebuild<'a>(dependents: &'a [Dependent], declared: &Schema) -> Vec<&'a Dependent> {
    dependents.iter().filter(|d| d.managed(declared)).collect()
}

/// Managed callers that only a name scan can see — **reported, never refused**.
///
/// **Measured**, `pg_depend` records an edge for a caller only when the calling
/// body was parsed at creation time: of three callers of `m.dep_f(int)`, the
/// `BEGIN ATOMIC` one records an edge and the plpgsql and SQL string-literal
/// bodies record nothing. So a rebuild goes through and the failure lands
/// somewhere else entirely — `function m.dep_f(integer) does not exist`, at the
/// caller's next call, with `verify` clean.
///
/// And an unchanged identity is not safety: **measured**, renaming a
/// *parameter* keeps `mm.f(integer)` exactly as it was and still breaks a
/// caller that used named notation, and a changed return type under the same
/// identity does the same. Both live inside the opaque `definition`, so the
/// trigger for the scan cannot be "the identity changed" — it is "a routine is
/// rebuilt or removed", and `DropModule` is the case with the most to lose.
///
/// # Why this reports and does not refuse
///
/// The scan matches a **name**, and with overloading a name is not an identity,
/// so a managed caller of `f(text)` would block every rebuild of `f(integer)`
/// and editing the caller could not clear the block while it still legitimately
/// mentions `f`. A refusal with no way out is not conservative: it makes valid
/// work impossible and teaches the next person to route around the tool. The
/// refusals in this design are for facts — a `pg_depend` edge, or a
/// `depends_on:` the user declared — and a name is not one.
///
/// There is no exemption for a caller the same plan recreates, although
/// `check_function_bodies = on` is pinned by the transaction framing.
/// **Measured**, that check proves the body *resolves*, which was never the
/// same claim as "resolves to what it resolved to": a caller of `g(1)` that
/// answered `integer overload`, after `g(integer)` is dropped, is accepted on
/// recreation and answers `bigint overload` through an implicit conversion.
/// Suppressing the report on the strength of it would hide exactly the
/// behaviour change the scan exists to surface.
#[must_use]
pub fn callers_by_name(declared: &Schema, id: &ModuleId) -> Vec<ModuleId> {
    let name = id.object_name();
    declared
        .modules
        .iter()
        .filter(|(other, _)| *other != id)
        .filter(|(_, m)| pbps_model::module::references(&m.definition, &name))
        .map(|(other, _)| other.clone())
        .collect()
}

/// The report those callers become, or `None` where the scan found nothing.
#[must_use]
pub fn callers_report(id: &ModuleId, callers: &[ModuleId]) -> Option<String> {
    if callers.is_empty() {
        return None;
    }
    let named: Vec<String> = callers.iter().map(|c| format!("- {c}")).collect();
    Some(format!(
        "`{id}` is being rebuilt or removed, and these declared modules mention its name. The \
         catalog records a dependency only for a body it parsed at creation time — measured, a \
         `BEGIN ATOMIC` body records one and a plpgsql or SQL string-literal body records \
         nothing — so this is a scan, and a name is not an identity where routines overload. It \
         is reported and not refused; check each and, where the call has to be reordered, say so \
         with `depends_on:`:\n{}",
        named.join("\n")
    ))
}

// ---- binding resolution and candidate sets (ADR-0013 §3) ----

/// One module this plan must rebuild because the same plan puts a same-named
/// object where the module's names resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebound {
    /// The module whose binding could move.
    pub module: ModuleId,
    /// What this plan introduces that could capture it.
    pub arriving: ModuleId,
}

/// Every unchanged managed module whose binding this plan could move.
///
/// **The comparison is asked of the catalog as this plan will leave it** —
/// minus what it drops, plus what it creates and the destinations of what it
/// renames — because a shadow the same plan introduces is otherwise found one
/// plan late: the first plan creates the object, and only the *next* one
/// notices that a binding changed. By then the environment has been running on
/// the new binding.
///
/// # Why the test is a name and a path, and not a position
///
/// Deliberately conservative, and the ADR says so in as many words: a
/// declaration that qualified the name in full is rebuilt too, once. **Position
/// on the path is not the question**, because an overload in the same schema
/// captures a call without anything moving; and what an unchanged declaration
/// *would* bind to today cannot be computed without parsing it — which §8.2
/// forbids — or creating it, which planning must not do.
///
/// So the test is: this plan brings an object into a schema on that module's
/// effective write path, and the module's text mentions that object's bare
/// name. One rebuild, once, and the state re-recorded (DECISIONS 289).
///
/// `pg_catalog` is not a schema a write path may list (DECISIONS 277), so
/// nothing this plan creates can arrive there and it is not considered.
#[must_use]
pub fn rebound_by_this_plan(
    declared: &Schema,
    write_path_extras: &[String],
    arriving: &[ModuleId],
    already_changed: &BTreeSet<ModuleId>,
) -> Vec<Rebound> {
    let mut out = Vec::new();
    for (module, definition) in &declared.modules {
        if already_changed.contains(module) {
            continue;
        }
        for new in arriving {
            if new == module {
                continue;
            }
            // On this module's write path: its own schema first, then the
            // configured extras, in the order the emitter writes them.
            let on_the_path = new.schema() == module.schema()
                || write_path_extras.iter().any(|e| e == new.schema());
            if !on_the_path {
                continue;
            }
            if pbps_model::module::references(&definition.definition, &new.object_name()) {
                out.push(Rebound {
                    module: module.clone(),
                    arriving: new.clone(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, Module, ModuleKind, Table};

    fn id(s: &str) -> ModuleId {
        s.parse().expect("a module id parses")
    }

    fn module(definition: &str) -> Module {
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: definition.to_owned(),
        }
    }

    fn carried(what: &'static str, detail: &str) -> Carried {
        Carried {
            what,
            detail: detail.to_owned(),
        }
    }

    /// An object carrying nothing is the case a rebuild is allowed to take, and
    /// it has to be distinguishable from one whose reads all came back empty
    /// for another reason. `refusal` says nothing only when `carries` is empty.
    #[test]
    fn a_module_that_carries_nothing_refuses_nothing() {
        let clean = Rebuild {
            id: id("app.v"),
            carries: Vec::new(),
            serialized: Serialized::By("the view's own ACCESS EXCLUSIVE lock"),
        };
        assert_eq!(clean.refusal(), None);
    }

    /// One message naming every item, not one per item: an operator reading
    /// this is deciding what to do about the object, and three refusals about
    /// one view read as three problems.
    #[test]
    fn one_refusal_names_every_thing_the_rebuild_would_destroy() {
        let loaded = Rebuild {
            id: id("app.v"),
            carries: vec![
                carried("the object's ACL", "{app=arwdDxtm/app,reader=r/app}"),
                carried("options in `pg_class.reloptions`", "security_invoker=true"),
            ],
            serialized: Serialized::By("the view's own ACCESS EXCLUSIVE lock"),
        };
        let refusal = loaded.refusal().expect("two carried things refuse");
        assert_eq!(refusal.matches("app.v").count(), 1, "{refusal}");
        assert!(refusal.contains("reader=r/app"), "{refusal}");
        assert!(refusal.contains("security_invoker=true"), "{refusal}");
    }

    fn schema_with_a_table() -> Schema {
        let mut schema = Schema::default();
        let mut t = Table::default();
        t.columns.insert(
            "d".into(),
            Column {
                default: Some("app.g(1)".into()),
                ..Column::new("integer".parse().expect("a type"))
            },
        );
        t.columns
            .insert("n".into(), Column::new("integer".parse().expect("a type")));
        t.checks.insert(
            "ck".into(),
            pbps_model::CheckConstraint {
                expression: "app.g(n) > 0".into(),
            },
        );
        schema.tables.insert(TableName::new("app", "t"), t);
        schema
    }

    fn dependent(described: &str, holds: Holds) -> Dependent {
        Dependent {
            described: described.to_owned(),
            holds,
        }
    }

    /// "Managed" is a question about the declarations, and it has to be asked
    /// of the exact part: a table this project declares does not make every
    /// constraint on it declared, and a column with no `default:` has no
    /// default for a plan to restore.
    #[test]
    fn a_part_is_managed_only_where_the_declaration_holds_that_part() {
        let declared = schema_with_a_table();
        let table = TableName::new("app", "t");
        let cases = [
            (Part::Check("ck".into()), true),
            (Part::Check("ck_other".into()), false),
            (Part::Default("d".into()), true),
            // Declared, and with no default: nothing to put back.
            (Part::Default("n".into()), false),
            (Part::Default("absent".into()), false),
            (Part::Index("ix".into()), false),
        ];
        for (part, expected) in cases {
            let d = dependent(
                "a part",
                Holds::TablePart {
                    table: table.clone(),
                    part: part.clone(),
                },
            );
            assert_eq!(d.managed(&declared), expected, "{part:?}");
        }
        // And a table this project does not declare at all.
        assert!(
            !dependent(
                "elsewhere",
                Holds::TablePart {
                    table: TableName::new("other", "t"),
                    part: Part::Check("ck".into()),
                },
            )
            .managed(&declared)
        );
    }

    /// Never managed, whatever is declared. A generated column and an
    /// expression index have no declaration for a planner to recreate them
    /// from, so promising to restore them would be promising to emit a
    /// statement pbps cannot write.
    #[test]
    fn what_the_model_cannot_represent_is_never_managed() {
        let mut declared = schema_with_a_table();
        declared
            .modules
            .insert(id("app.f()"), module("() RETURNS int"));
        for why in ["a generated column", "an index over an expression"] {
            assert!(
                !dependent("x", Holds::Unrepresentable(why.to_owned())).managed(&declared),
                "{why}"
            );
        }
    }

    /// The refusal is over what is *left*, and a plan with everything declared
    /// refuses nothing — otherwise the rule would make ordinary work
    /// impossible, which is the failure ADR-0009 §4 corrected twice.
    #[test]
    fn a_dependent_the_declarations_hold_is_not_a_refusal() {
        let mut declared = schema_with_a_table();
        declared.modules.insert(id("app.v"), module("SELECT 1"));
        let all_declared = [
            dependent(
                "constraint ck on table app.t",
                Holds::TablePart {
                    table: TableName::new("app", "t"),
                    part: Part::Check("ck".into()),
                },
            ),
            dependent("view app.v", Holds::Module(id("app.v"))),
        ];
        assert_eq!(
            unmanaged_refusal(&id("app.g(integer)"), &all_declared, &declared),
            None
        );
        assert_eq!(to_rebuild(&all_declared, &declared).len(), 2);

        let one_missing = [dependent("view app.other", Holds::Module(id("app.other")))];
        let refusal = unmanaged_refusal(&id("app.g(integer)"), &one_missing, &declared)
            .expect("an undeclared dependent refuses");
        assert!(refusal.contains("app.other"), "{refusal}");
        // The shortest way out is the one SPEC 14.3 refuses to offer.
        assert!(!refusal.contains("CASCADE the"), "{refusal}");
        assert!(to_rebuild(&one_missing, &declared).is_empty());
    }

    /// The scan over-approximates on purpose, and the negative half is what
    /// keeps it honest: it never reports the object being rebuilt as its own
    /// caller, and it says nothing where no declaration mentions the name.
    #[test]
    fn the_caller_scan_reports_names_and_never_the_object_itself() {
        let mut declared = Schema::default();
        declared.modules.insert(
            id("app.g(integer)"),
            module("(a integer) RETURNS int AS $$ SELECT app.g(a) $$"),
        );
        declared.modules.insert(
            id("app.caller()"),
            module("() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN app.g(1); END $$"),
        );
        declared.modules.insert(
            id("app.bare()"),
            module("() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN g(1); END $$"),
        );
        declared.modules.insert(
            id("app.unrelated()"),
            module("() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"),
        );
        let found = callers_by_name(&declared, &id("app.g(integer)"));
        assert_eq!(found, vec![id("app.bare()"), id("app.caller()")]);
        assert!(callers_report(&id("app.g(integer)"), &found).is_some());
        assert_eq!(callers_report(&id("app.g(integer)"), &[]), None);
    }

    /// The whole shape of ADR-0013 §3's answer, and its three negatives: a
    /// module the plan already changes is not rebuilt twice, an object arriving
    /// off the write path cannot capture anything, and a name no declaration
    /// mentions is not a shadow.
    #[test]
    fn only_a_name_arriving_on_the_modules_own_write_path_rebinds_it() {
        let mut declared = Schema::default();
        declared.modules.insert(
            id("app.caller()"),
            module("() RETURNS text LANGUAGE sql BEGIN ATOMIC SELECT f(1); END"),
        );
        declared.modules.insert(
            id("app.quiet()"),
            module("() RETURNS int AS $$ SELECT 1 $$"),
        );
        let extras = vec!["shared".to_owned()];
        let nothing_changed = BTreeSet::new();

        // The module's own schema is first on its write path.
        let own = rebound_by_this_plan(
            &declared,
            &extras,
            &[id("app.f(integer)")],
            &nothing_changed,
        );
        assert_eq!(
            own,
            vec![Rebound {
                module: id("app.caller()"),
                arriving: id("app.f(integer)"),
            }]
        );

        // And so is a configured extra.
        assert_eq!(
            rebound_by_this_plan(
                &declared,
                &extras,
                &[id("shared.f(integer)")],
                &nothing_changed
            )
            .len(),
            1
        );

        // A schema that is not on the path cannot capture an unqualified name,
        // so nothing is rebuilt for it.
        assert!(
            rebound_by_this_plan(
                &declared,
                &extras,
                &[id("elsewhere.f(integer)")],
                &nothing_changed
            )
            .is_empty()
        );

        // A name no declaration mentions is not a shadow.
        assert!(
            rebound_by_this_plan(
                &declared,
                &extras,
                &[id("app.nobody(integer)")],
                &nothing_changed
            )
            .is_empty()
        );

        // Once: a module this plan already changes is rebuilt by that change,
        // not again by this answer.
        let already: BTreeSet<ModuleId> = [id("app.caller()")].into_iter().collect();
        assert!(
            rebound_by_this_plan(&declared, &extras, &[id("app.f(integer)")], &already).is_empty()
        );
    }
}
