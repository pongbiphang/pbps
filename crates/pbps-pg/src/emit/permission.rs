//! Emits the permission family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(pg: &Postgres, permission: &GrantChange) -> Sql {
    match permission {
        GrantChange::Grant {
            role,
            target,
            permissions,
        } => grant_statements(pg, role, target, permissions, false),
        // One privilege per statement, unlike `GRANT` above. A `REVOKE`
        // removes only what the one grantor PostgreSQL selects for it put
        // there, and that selection is made once for the whole statement:
        // measured on 18.6, a deployer inheriting both `ga` and `gb` ran
        // `REVOKE SELECT, INSERT` over `r1=r/ga,r1=a/gb` and took only the
        // `SELECT`, warning `not all privileges could be revoked`; the same
        // two privileges revoked one statement each took both. The pull
        // decides revocability per privilege for exactly this reason
        // (`catalog::revocable_by_current_role`), so keeping the emitted
        // statement that wide would promise something it cannot do
        // (DECISIONS 518, #251).
        GrantChange::Revoke {
            role,
            target,
            permissions,
        } => grant_statements(pg, role, target, permissions, true),
        // `PUBLIC` is written bare, and that is the whole point of the
        // variant: it is SQL's keyword for every principal at once, and
        // `quote` would turn it into `"PUBLIC"`, a role of that name — which
        // on this engine is a different grantee, and on a cluster that has no
        // such role is `role "PUBLIC" does not exist`.
        //
        // `ON ROUTINE`, because one word has to take a function and a
        // procedure alike (DECISIONS 372), and the signature is always
        // spelled: this change carries a `RoutineId`, so there is no
        // overloaded bare name to be ambiguous about.
        //
        // Both decisions write a statement, and the second one is the reason
        // to distrust "the `CREATE` already did it". A cluster whose
        // deployment role has run `ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON
        // ROUTINES FROM PUBLIC` creates routines with an explicit `proacl`
        // that `PUBLIC` is not in — measured on 18.6 — so a `Kept` that
        // emitted nothing would apply a declaration and leave the declared
        // state unreached, with `verify` unable to say so because what
        // `PUBLIC` holds is never compared (DECISIONS 371). The `GRANT` is
        // idempotent against the untouched case: on a routine whose `proacl`
        // is still `NULL` it writes exactly `acldefault('f', owner)`,
        // measured equal, so the rebuild guard that compares against
        // `acldefault` sees no difference either.
        GrantChange::PublicExecution {
            access, routine, ..
        } => {
            let target = GrantTarget::Routine(routine.clone());
            let execute = BTreeSet::from([Permission::Execute]);
            let statement = match access {
                PublicAccess::Revoked => {
                    format!(
                        "REVOKE EXECUTE ON {} FROM PUBLIC;",
                        securable(&target, &execute)?
                    )
                }
                PublicAccess::Kept => {
                    format!(
                        "GRANT EXECUTE ON {} TO PUBLIC;",
                        securable(&target, &execute)?
                    )
                }
            };
            Ok(vec![scoped(pg, target.schema(), &statement)?])
        }
    }
}
