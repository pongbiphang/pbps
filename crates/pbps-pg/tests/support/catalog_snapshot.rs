// Database isolation does not isolate cluster-wide roles. Keep every database
// fact, plus postgres and the caller's entire role namespace: an unexpected role
// created by this fixture must remain visible to a whole-snapshot assertion.
pub(super) fn owned_roles(
    mut schema: pbps_model::Schema,
    owns_role: impl Fn(&str) -> bool,
) -> pbps_model::Schema {
    schema
        .roles
        .retain(|name, _| name.raw() == "postgres" || owns_role(name.raw()));
    schema
}
