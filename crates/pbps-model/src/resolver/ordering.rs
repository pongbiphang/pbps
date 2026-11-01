//! The dependency proof for the exact sequence an approver receives.

use super::{Binding, ObjectIdentity};
use crate::{ChangeSet, ColumnRef, ModuleId, TableName};
use schemars::JsonSchema;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// A managed surface the existing typed changes can remove or establish.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize, JsonSchema,
)]
#[serde(
    tag = "kind",
    content = "subject",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum Surface {
    Namespace(String),
    Table(TableName),
    Column(ColumnRef),
    Default(ColumnRef),
    Check {
        table: TableName,
        name: crate::ObjectLocalName,
    },
    Index {
        table: TableName,
        name: crate::ObjectLocalName,
    },
    Module(ModuleId),
    /// A declared enum or domain: its `pg_type` row and its array type
    /// (#2407).
    Type(crate::TypeRef),
}

/// One actual observation. `managed_inputs` is the adapter's mapping of
/// logical targets to declared identities; it does not parse SQL or invent
/// an identity decision. The complete logical bindings remain in `bindings`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoundSurface {
    pub object: ObjectIdentity,
    pub bindings: Vec<Binding>,
    pub managed_inputs: BTreeSet<Surface>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SurfaceResolution {
    pub surface: Surface,
    /// None means this surface does not exist, not that a read failed.
    #[serde(deserialize_with = "crate::presence::required_option")]
    #[schemars(
        required,
        schema_with = "crate::presence::required_option_schema::<BoundSurface>"
    )]
    pub current: Option<BoundSurface>,
    #[serde(deserialize_with = "crate::presence::required_option")]
    #[schemars(
        required,
        schema_with = "crate::presence::required_option_schema::<BoundSurface>"
    )]
    pub desired: Option<BoundSurface>,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum OrderReason {
    Structural,
    Binding,
    Identity,
    Data,
    Authorization,
    Restoration,
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct OrderEdge {
    pub before: usize,
    pub after: usize,
    pub reason: OrderReason,
}

/// Positions name the final sealed sequence, not a transient planner graph.
/// Consumers validate this proof against their ChangeSet; they never reorder it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrderingProof {
    version: u32,
    changes: String,
    edges: BTreeSet<OrderEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OrderError {
    #[error("unsupported resolver ordering format")]
    Version,
    #[error("the resolver ordering proof does not describe this change sequence")]
    Changed,
    #[error("the resolver ordering proof has an invalid or unsatisfied edge")]
    Edge,
}

impl OrderingProof {
    pub fn new(changes: &ChangeSet, edges: BTreeSet<OrderEdge>) -> Result<Self, OrderError> {
        let proof = Self {
            version: 1,
            changes: checksum(changes),
            edges,
        };
        proof.validate(changes)?;
        Ok(proof)
    }

    pub fn edges(&self) -> &BTreeSet<OrderEdge> {
        &self.edges
    }

    pub fn validate(&self, changes: &ChangeSet) -> Result<(), OrderError> {
        if self.version != 1 {
            return Err(OrderError::Version);
        }
        if self.changes != checksum(changes) {
            return Err(OrderError::Changed);
        }
        if self
            .edges
            .iter()
            .any(|e| e.before >= e.after || e.after >= changes.changes.len())
        {
            return Err(OrderError::Edge);
        }
        Ok(())
    }
}

fn checksum(changes: &ChangeSet) -> String {
    // These are the managed, reviewed typed changes, never captured external
    // properties. Their ordinary approval checksum remains SHA-256.
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(changes).expect("changes serialize"))
    )
}
