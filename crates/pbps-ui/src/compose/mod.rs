//! Composing reviewed intent through `git` (ADR-0015 decision 5).

pub mod git;

/// The placement protocol of step 2 is Linux-only: macOS needs `renamex_np`
/// and `acl_get_fd_np` and Windows the relative `NtCreateFile` walk, neither
/// reachable from a workspace that forbids `unsafe`. Decision 5's own rule for
/// a platform without these calls is to refuse to compose and give the
/// commands to run by hand.
#[cfg(target_os = "linux")]
pub mod fsx;
#[cfg(target_os = "linux")]
pub mod locks;
#[cfg(target_os = "linux")]
pub mod record;
pub mod repo_path;
