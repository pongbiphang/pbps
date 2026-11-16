//! Package reads used by the preserved apply/bootstrap helpers.
//! Comparisons and their refusal wording remain at each original CLI gate
//! (DEC-2447.6, DEC-2447.8, DEC-2447.10).
use super::*;
use pbps_model::{Extension, ExtensionProvider};
use pbps_pg::extension_provider::Target;

pub(crate) async fn vet_before_change(
    conn: &mut Conn,
    name: &str,
    pin: &pbps_model::ExtensionChangePin,
    from: &Extension,
    moving: bool,
) -> Result<(), DbError> {
    pbps_pg::extension_provider::vet_before_change(conn, name, pin, from, moving).await
}

pub(crate) async fn confirm_changed(
    conn: &mut Conn,
    name: &str,
    pin: &pbps_model::ExtensionChangePin,
    moving: bool,
) -> Result<(), DbError> {
    pbps_pg::extension_provider::confirm_changed(conn, name, pin, moving).await
}

pub(crate) async fn vet(
    conn: &mut Conn,
    name: &str,
    expected: &ExtensionProvider,
) -> Result<(), DbError> {
    pbps_pg::extension_provider::vet(conn, name, expected).await
}

pub(crate) async fn vet_before_drop(
    conn: &mut Conn,
    name: &str,
    pin: &pbps_model::ExtensionDropPin,
) -> Result<(), DbError> {
    pbps_pg::extension_provider::vet_before_drop(conn, name, pin).await
}

pub(crate) async fn confirm_dropped(conn: &mut Conn, name: &str) -> Result<(), DbError> {
    pbps_pg::extension_provider::confirm_dropped(conn, name).await
}

pub(crate) async fn drop_problems(
    conn: &mut Conn,
    names: &BTreeSet<String>,
) -> Result<Vec<String>, DbError> {
    pbps_pg::extension_provider::drop_problems(conn, names).await
}

pub(crate) async fn installed(conn: &mut Conn) -> Result<BTreeMap<String, Extension>, DbError> {
    pbps_pg::extension_provider::installed(conn).await
}

pub(crate) async fn update_problems(
    conn: &mut Conn,
    updates: &BTreeMap<String, (Extension, Extension)>,
    creating: &BTreeMap<String, Extension>,
) -> Result<Vec<String>, DbError> {
    pbps_pg::extension_provider::update_problems(conn, updates, creating).await
}

pub(crate) async fn move_problems(
    conn: &mut Conn,
    moves: &BTreeMap<String, (Extension, Extension)>,
    updating: &BTreeSet<String>,
) -> Result<Vec<String>, DbError> {
    pbps_pg::extension_provider::move_problems(conn, moves, updating).await
}

pub(crate) async fn target(conn: &mut Conn) -> Result<Target, DbError> {
    pbps_pg::extension_provider::target(conn).await
}

pub(crate) fn create_problems(
    target: &Target,
    name: &str,
    wanted: &Extension,
    pinned: &BTreeMap<String, Extension>,
) -> Vec<String> {
    pbps_pg::extension_provider::create_problems(target, name, wanted, pinned)
}
