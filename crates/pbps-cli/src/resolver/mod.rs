//! Resolver lifecycle, separate from environment and binding qualification.
//!
//! Acquiring an image never authorizes declaration transfer. The planning
//! command may use these resources only after the complete runtime admission
//! and engine-specific qualification required by ADR-0016.

#[cfg(target_os = "linux")]
pub mod docker;

#[cfg(target_os = "linux")]
pub mod native;
