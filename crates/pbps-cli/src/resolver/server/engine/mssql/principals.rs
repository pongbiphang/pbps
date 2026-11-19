//! The principals a SQL Server rehearsal's statements name (#2379): moved with
//! the rehearsal's roles out of `scratch::rehearse`, which now names no engine.

use crate::resolver::server::scratch::rehearse::Roles;
use pbps_model::RoleName;
use pbps_model::role_mapping::RoleMapping;
use std::collections::BTreeSet;

/// The principals a SQL Server rehearsal's statements name, as the scratch
/// can hold them: every one is a role the declarations hold (the base creates
/// it) or one every database holds.
///
/// pbps declares roles and the grants to them; users and logins stay with the
/// environment (ADR-0005), and no statement it writes names one. Roles and
/// users are database-scoped, so the scratch database holds the target's own
/// role names whatever instance it is on, and nothing is made on the instance
/// (measured: a confined login makes the same names in two databases at once,
/// and dropping the database leaves the server's principals as they were).
/// A login is a server principal; a rehearsal never creates, alters or names
/// one (DEC-2379.3). A statement that did name a principal the declarations do
/// not hold would run against a user or login the scratch does not have and
/// fail for a reason that says nothing about the plan, so such a plan is not
/// rehearsed for authorization and the record says why.
pub(super) fn principals(
    named: &BTreeSet<RoleName>,
    base: &pbps_model::Schema,
    desired: &pbps_model::Schema,
) -> Roles {
    let missing = undeclared(named, base, desired);
    if missing.is_empty() {
        Ok(RoleMapping::default())
    } else {
        Err(format!(
            "the plan names {}, which the declarations do not hold: a user or login belongs to \
             the environment, and a rehearsal makes none",
            missing
                .iter()
                .map(|name| format!("`{}`", name.shown()))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// The principals `named` that are neither declared in `base` or `desired` nor
/// built into every database.
fn undeclared<'a>(
    named: &'a BTreeSet<RoleName>,
    base: &pbps_model::Schema,
    desired: &pbps_model::Schema,
) -> Vec<&'a RoleName> {
    named
        .iter()
        .filter(|name| {
            !base.roles.contains_key(*name)
                && !desired.roles.contains_key(*name)
                && !builtin(name.raw())
        })
        .collect()
}

/// The fixed database roles, which every database holds.
const FIXED_ROLES: [&str; 9] = [
    "db_owner",
    "db_accessadmin",
    "db_securityadmin",
    "db_ddladmin",
    "db_backupoperator",
    "db_datareader",
    "db_datawriter",
    "db_denydatareader",
    "db_denydatawriter",
];

/// A principal every database holds: it is never made, and never mapped.
/// Compared as the engine compares a principal, ignoring case, and by the whole
/// name: a `db_` prefix is not the fixed roles (a user may name a role
/// `db_app_reader`, which a scratch does not have).
fn builtin(name: &str) -> bool {
    ["dbo", "public", "guest", "sys", "INFORMATION_SCHEMA"]
        .iter()
        .chain(FIXED_ROLES.iter())
        .any(|own| own.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod principals_tests {
    use super::*;

    fn role(name: &str) -> RoleName {
        name.into()
    }

    fn holding(roles: &[&str]) -> pbps_model::Schema {
        let mut schema = pbps_model::Schema::default();
        for name in roles {
            schema
                .roles
                .insert(role(name), pbps_model::role::Role::default());
        }
        schema
    }

    #[test]
    fn declared_and_built_in_principals_are_held_and_a_user_is_not() {
        let named: BTreeSet<_> = [
            "reader",
            "later",
            "dbo",
            "PUBLIC",
            "db_datareader",
            "app_user",
        ]
        .map(role)
        .into();
        let missing = undeclared(&named, &holding(&["reader"]), &holding(&["later"]));
        assert_eq!(missing, vec![&role("app_user")]);
        assert!(principals(&named, &holding(&["reader"]), &holding(&["later"])).is_err());
    }

    #[test]
    fn nothing_to_hold_is_a_rehearsal_under_the_targets_own_names() {
        let named: BTreeSet<_> = ["reader", "dbo"].map(role).into();
        assert!(principals(&named, &holding(&["reader"]), &holding(&[])).is_ok());
        assert!(principals(&BTreeSet::new(), &holding(&[]), &holding(&[])).is_ok());
    }

    /// The names are compared as the engine compares a principal, so a name
    /// that differs only in case from a built-in is the built-in's. Only the
    /// fixed roles are built in, not everything that starts with `db_`.
    #[test]
    fn a_name_that_only_looks_built_in_is_not() {
        assert!(builtin("DBO") && builtin("db_owner") && builtin("Guest"));
        assert!(builtin("DB_DATAREADER") && builtin("Db_DenyDataWriter"));
        assert!(!builtin("dbo_reader") && !builtin("database_user") && !builtin("sysadmin"));
        assert!(!builtin("db_app_reader") && !builtin("db_") && !builtin("db_owner2"));
        let named: BTreeSet<_> = ["db_app_reader", "DB_DATAREADER"].map(role).into();
        assert_eq!(
            undeclared(&named, &holding(&[]), &holding(&[])),
            vec![&role("db_app_reader")]
        );
    }
}
