//! What the owned catalog must hold before a hop meets it, derived from the
//! previous and incoming hops' recorded identities (#2552, ADR-0019 §3/§6).
//!
//! A missing name does not establish a new identity: a role the incoming hop
//! spells differently from the previous one may be the same principal under a
//! recorded rename. So this asks the identity mappings first and the catalog
//! second, and creates only what no recorded identity accounts for.
use super::{Assembled, quoted};
use pbps_db::Conn;
use pbps_model::{IdsFile, RoleName, Schema};
use std::collections::BTreeSet;

/// The two hops a prerequisite pass sits between. The first bootstrap has no
/// previous hop, and that absence is stated, not defaulted: a pass that is
/// handed a later hop with `None` would read every name as new.
#[derive(Clone, Copy)]
pub(crate) struct HopContext<'a> {
    pub previous: Option<&'a Assembled>,
    pub incoming: &'a Assembled,
}

impl<'a> HopContext<'a> {
    /// The first bootstrap: the previous hop is explicitly absent.
    pub fn first(incoming: &'a Assembled) -> Self {
        Self {
            previous: None,
            incoming,
        }
    }
    pub fn after(previous: &'a Assembled, incoming: &'a Assembled) -> Self {
        Self {
            previous: Some(previous),
            incoming,
        }
    }
}

/// The cluster roles the incoming hop needs, as identity facts alone.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Roles {
    /// Recorded renames the hop declares, `(from, to)`: the same rule the
    /// ordinary rename evidence applies ([`IdsFile::role_renames`]).
    pub renames: Vec<(RoleName, RoleName)>,
    /// Named roles the previous hop did not name and no rename accounts for:
    /// the candidates for creation. A role the previous hop already named is
    /// retained, whatever the catalog now says about it, and the ordinary
    /// presence check judges that.
    pub introduced: BTreeSet<RoleName>,
}

/// One hop's recorded identities and declarations.
type Hop<'a> = (&'a IdsFile, &'a Schema);

pub(super) fn roles(ctx: HopContext<'_>) -> Roles {
    derive(
        ctx.previous.map(|p| (&p.ids, &p.loaded.schema)),
        (&ctx.incoming.ids, &ctx.incoming.loaded.schema),
    )
}

fn derive(previous: Option<Hop<'_>>, incoming: Hop<'_>) -> Roles {
    let nothing = IdsFile::default();
    let renames: Vec<_> = previous
        .map_or(&nothing, |(ids, _)| ids)
        .role_renames(incoming.0)
        .map(|(from, to)| (from.clone(), to.clone()))
        .collect();
    let mut introduced = incoming.1.named_roles();
    if let Some((_, schema)) = previous {
        for name in schema.named_roles() {
            introduced.remove(&name);
        }
    }
    for (_, to) in &renames {
        introduced.remove(to);
    }
    Roles {
        renames,
        introduced,
    }
}

/// Completes each recorded rename inside the owned scratch, so the ordinary
/// `rename_evidence` check finds it done. Only the state "old name present,
/// new name absent" is completed. An occupied destination, a vanished source
/// and an unreadable catalog are left to that check, which refuses them; the
/// read error propagates before anything changes.
pub(crate) async fn complete_renames(
    conn: &mut Conn,
    renames: &[(RoleName, RoleName)],
) -> anyhow::Result<()> {
    use pbps_pg::roles::{RenameEvidence, rename_evidence};
    for (from, to) in renames {
        if rename_evidence(conn, from, to).await? == RenameEvidence::NotRunYet {
            conn.execute(&format!(
                "ALTER ROLE {} RENAME TO {}",
                quoted(from.raw())?,
                quoted(to.raw())?
            ))
            .await?;
        }
    }
    Ok(())
}

/// Creates the introduced roles the cluster lacks. Roles that exist keep their
/// attributes, and a new one gets none of the elevated ones.
pub(super) async fn create_missing(
    conn: &mut Conn,
    introduced: &BTreeSet<RoleName>,
) -> anyhow::Result<()> {
    for name in pbps_pg::roles::missing_roles(conn, &introduced.iter().collect()).await? {
        let role = quoted(name.raw())?;
        conn.execute(&format!(
            "CREATE ROLE {role} NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS"
        ))
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(roles: &[(&str, &str)]) -> (IdsFile, Schema) {
        let mut ids = IdsFile::default();
        let mut schema = Schema::default();
        for (uid, name) in roles {
            ids.roles.insert(uid.parse().unwrap(), (*name).into());
            schema.roles.insert((*name).into(), Default::default());
        }
        (ids, schema)
    }
    fn names(roles: &Roles) -> Vec<&str> {
        roles.introduced.iter().map(|r| r.raw()).collect()
    }

    #[test]
    fn the_first_bootstrap_introduces_every_named_role() {
        let (ids, schema) = hop(&[("r_aaaaaa", "reader"), ("r_bbbbbb", "writer")]);
        let found = derive(None, (&ids, &schema));
        assert_eq!(names(&found), ["reader", "writer"]);
        assert!(found.renames.is_empty());
    }

    #[test]
    fn a_recorded_rename_is_completed_and_its_new_name_is_not_a_new_principal() {
        let (before, schema_before) = hop(&[("r_aaaaaa", "old"), ("r_bbbbbb", "kept")]);
        let (after, schema_after) = hop(&[("r_aaaaaa", "new"), ("r_bbbbbb", "kept")]);
        let found = derive(Some((&before, &schema_before)), (&after, &schema_after));
        assert_eq!(found.renames, [("old".into(), "new".into())]);
        assert!(found.introduced.is_empty(), "{found:?}");
    }

    /// Negative cases: a new uid whose spelling resembles a retained name is a
    /// new role, and a retained name is never created again.
    #[test]
    fn a_resemblance_is_new_and_a_retained_name_is_not_introduced_again() {
        let (before, schema_before) = hop(&[("r_aaaaaa", "reader")]);
        let (after, schema_after) = hop(&[("r_aaaaaa", "reader"), ("r_cccccc", "reader2")]);
        let found = derive(Some((&before, &schema_before)), (&after, &schema_after));
        assert!(found.renames.is_empty());
        assert_eq!(names(&found), ["reader2"]);
    }
}
