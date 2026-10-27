//! The intended continuation of one scratch backend, independent of a script's
//! settings. Observation reads use a clean recipe; compilation resumes its
//! registered overlay and target zone (DEC-1564.1, DEC-1778.2, DEC-1916.1).

use super::{authorization, scratch};
use authorization::AuthorizationContext;
use pbps_db::DbError;
use pbps_db::transport::ExecuteConnection;
use pbps_model::{RoleName, SessionZone};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reset {
    All,
    Discard,
}

#[derive(Debug, Clone)]
struct Intent {
    role: Option<RoleName>,
    // None means not installed; Some(empty) is a successfully installed overlay.
    settings: Option<AuthorizationContext>,
    pins: Option<SessionZone>,
}

/// Registered when pbps establishes state, never inferred from a script's GUCs.
/// Supplied recipes retain their qualified backend; run-owned callers keep
/// the recipe beside the connection they own.
#[derive(Debug, Clone)]
pub struct SessionState {
    backend: Option<scratch::Backend>,
    intent: Intent,
}

impl SessionState {
    pub fn new(role: Option<RoleName>) -> Self {
        Self {
            backend: None,
            intent: Intent {
                role,
                settings: None,
                pins: None,
            },
        }
    }

    /// A supplied connection keeps the backend its separation qualified.
    /// Run-owned connections are bound by their owning wrapper: SET ROLE can
    /// hide backend_start, so they must not acquire a new stats privilege.
    pub fn checked(backend: scratch::Backend, role: Option<RoleName>) -> Self {
        Self {
            backend: Some(backend),
            ..Self::new(role)
        }
    }

    /// Register only after replay succeeds; a failed replay is not an empty map.
    pub async fn install_settings(
        &mut self,
        conn: &mut impl ExecuteConnection,
        context: &AuthorizationContext,
    ) -> Result<(), DbError> {
        authorization::apply_session_settings(conn, context).await?;
        self.intent.settings = Some(context.clone());
        Ok(())
    }

    /// Compilation's pins are inseparable from the target's captured zone.
    pub fn compiling(&mut self, zone: &SessionZone) {
        self.intent.pins = Some(zone.clone());
    }

    /// Both snapshots share one transaction, so the backend cannot move between
    /// them. Do not add a statistics read under a role the script selected;
    /// the registered continuation checks its qualified backend after commit.
    pub fn observation(&self) -> Self {
        Self::new(self.intent.role.clone())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(
        "the scratch connection reached another server backend than the one the run checked; connect to the scratch server directly or through session pooling, not transaction pooling"
    )]
    MovedBackend,
}

enum Step<'a> {
    Reset(Reset),
    Path,
    Backend,
    Role(Option<&'a RoleName>),
    Settings(&'a AuthorizationContext),
    Pins(&'a SessionZone),
    Keeper,
}

impl Step<'_> {
    fn statement(&self) -> Option<String> {
        match self {
            Self::Reset(Reset::All) => Some("RESET ALL".into()),
            Self::Reset(Reset::Discard) => Some("DISCARD ALL".into()),
            Self::Path => Some("SET search_path = ''".into()),
            Self::Role(None) => Some("SET ROLE NONE".into()),
            Self::Role(Some(role)) => {
                let quoted = role.raw().replace('"', "\"\"");
                Some(format!("SET ROLE \"{quoted}\""))
            }
            Self::Pins(zone) => Some(crate::session_pins(zone)),
            Self::Keeper => Some(scratch::KEEPER_SETTINGS.into()),
            Self::Backend | Self::Settings(_) => None,
        }
    }
}

impl Intent {
    fn restoration(&self, reset: Reset) -> Vec<Step<'_>> {
        let mut steps = vec![Step::Reset(reset), Step::Path, Step::Backend];
        // RESET preserves the role; DISCARD clears it outside the transaction.
        if reset == Reset::Discard {
            steps.push(Step::Role(self.role.as_ref()));
        }
        // Preserve the existing replay precedence: keeper defaults first,
        // then the deployer's installed defaults, then compilation's pins.
        steps.push(Step::Keeper);
        if let Some(settings) = &self.settings {
            steps.push(Step::Settings(settings));
        }
        if let Some(zone) = &self.pins {
            steps.push(Step::Pins(zone));
        }
        steps
    }
}

/// Restore in separate protocol batches: parse-sensitive settings must take
/// effect before any later text is lexed (PITFALLS: settings and batches).
/// Any stage's failure returns immediately, before a usable continuation.
pub async fn reset_and_restore(
    conn: &mut impl ExecuteConnection,
    state: &SessionState,
    reset: Reset,
) -> Result<(), RestoreError> {
    restore_steps(conn, state, reset, |_| Ok(())).await
}

// The callback supplies deterministic stage failures to the live tests; the
// production recipe and its SQL execution are the same path in either case.
async fn restore_steps(
    conn: &mut impl ExecuteConnection,
    state: &SessionState,
    reset: Reset,
    mut before: impl FnMut(usize) -> Result<(), DbError>,
) -> Result<(), RestoreError> {
    for (index, step) in state.intent.restoration(reset).into_iter().enumerate() {
        before(index)?;
        match step {
            Step::Backend => {
                if let Some(backend) = &state.backend
                    && !scratch::on_backend(conn, backend).await?
                {
                    return Err(RestoreError::MovedBackend);
                }
            }
            Step::Settings(context) => authorization::apply_session_settings(conn, context).await?,
            Step::Reset(_) | Step::Path | Step::Role(_) | Step::Pins(_) | Step::Keeper => {
                conn.execute(&step.statement().expect("a SQL restoration step"))
                    .await?
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
