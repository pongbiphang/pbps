//! Session inputs that are outside the catalog snapshot. User-context GUCs
//! are pinned locally. No qualified renderer runs target routines or changes
//! roles/GUCs; this connection is exclusively borrowed for the whole read.
//! A configuration reload invalidates the interval, including change-and-
//! restore of superuser/postmaster settings. Kernel/build continuity is a
//! separate mandatory native-lifecycle boundary, never implied by this read.

use super::{Uncovered, logical::Catalog, read::Failure};
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_db::transport::QueryConnection;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, PartialEq, Eq, serde::Serialize)]
pub(super) struct Facts {
    pub current_role: ObjectIdentity,
    pub session_role: ObjectIdentity,
    pub settings: BTreeMap<String, Setting>,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Setting {
    pub(super) value: String,
    context: String,
}

pub(super) struct Observation {
    current_role: String,
    session_role: String,
    settings: BTreeMap<String, Setting>,
    epoch: String,
}

impl Observation {
    pub fn logical(&self, catalog: &Catalog) -> Result<Facts, Failure> {
        let role = |name: &str| {
            let mut found = catalog.rows["pg_roles"]
                .iter()
                .filter(|row| row.get("rolname").and_then(Value::as_str) == Some(name));
            let row = found.next().ok_or(Failure::Changed)?;
            if found.next().is_some() {
                return Err(Failure::Incomplete);
            }
            catalog
                .identity("pg_roles", row)
                .map_err(|_| Failure::Incomplete)
        };
        Ok(Facts {
            current_role: role(&self.current_role)?,
            session_role: role(&self.session_role)?,
            settings: self.settings.clone(),
        })
    }

    pub fn unchanged(&self, other: &Self) -> bool {
        self.epoch == other.epoch
            && self.current_role == other.current_role
            && self.session_role == other.session_role
            && self.settings == other.settings
    }
}

pub(super) async fn observe(conn: &mut impl QueryConnection) -> Result<Observation, Failure> {
    let names = super::super::compatibility::SETTINGS
        .iter()
        .chain(super::super::compatibility::OPTIONAL_SETTINGS)
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT current_user::text AS current_role, session_user::text AS session_role, extract(epoch FROM pg_catalog.pg_conf_load_time())::text AS epoch, (SELECT pg_catalog.jsonb_object_agg(name,pg_catalog.jsonb_build_object('value',setting,'context',context))::text FROM pg_catalog.pg_settings WHERE name IN ({names})) AS settings"
    );
    let rows = conn.query(&sql).await.map_err(|_| Failure::Read)?;
    let [row] = rows.as_slice() else {
        return Err(Failure::Incomplete);
    };
    let text = |field| {
        row.try_get::<&str>(field)
            .map_err(|_| Failure::Incomplete)?
            .ok_or(Failure::Incomplete)
    };
    let settings: BTreeMap<String, Setting> =
        serde_json::from_str(text("settings")?).map_err(|_| Failure::Incomplete)?;
    if super::super::compatibility::SETTINGS
        .iter()
        .any(|name| !settings.contains_key(*name))
    {
        return Err(
            Uncovered::class("session-environment", "required setting is unreadable").into(),
        );
    }
    if settings.values().any(|value| {
        !matches!(
            value.context.as_str(),
            "user" | "superuser" | "internal" | "postmaster"
        )
    }) {
        return Err(
            Uncovered::class("session-environment", "setting lifetime is not qualified").into(),
        );
    }
    Ok(Observation {
        current_role: text("current_role")?.into(),
        session_role: text("session_role")?.into(),
        epoch: text("epoch")?.into(),
        settings,
    })
}

pub(super) async fn pin(conn: &mut impl QueryConnection) -> Result<Observation, Failure> {
    let before = observe(conn).await?;
    for (name, setting) in &before.settings {
        if setting.context == "user" {
            let sql = format!(
                "SELECT pg_catalog.set_config({}, {}, true)",
                crate::emit::value_literal(name),
                crate::emit::value_literal(&setting.value)
            );
            conn.query(&sql).await.map_err(|_| Failure::Read)?;
        }
    }
    if !before.unchanged(&observe(conn).await?) {
        return Err(Failure::EnvironmentChanged);
    }
    Ok(before)
}
