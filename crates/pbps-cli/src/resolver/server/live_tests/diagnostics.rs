use super::{Error, ServerFailure};
use std::fmt;

/// Backend strings may contain credentials or SQL. Keep only the typed cause
/// and the lifecycle owner's generated recovery names (SPEC §9.3).
pub(super) struct SafeFailure<'a>(pub(super) &'a ServerFailure);

impl fmt::Display for SafeFailure<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Exhaustive so a new payload-bearing variant cannot silently reach
        // fixture logs through Debug or Display.
        match &self.0.cause {
            Error::UnsupportedProfile { .. } => f.write_str("UnsupportedProfile"),
            Error::Endpoint => f.write_str("Endpoint"),
            Error::Daemon(_) => f.write_str("Daemon"),
            Error::Container(_) => f.write_str("Container"),
            Error::Configuration(_) => f.write_str("Configuration"),
            Error::EngineExecutable => f.write_str("EngineExecutable"),
            Error::Containment(premise) => write!(f, "Containment({premise:?})"),
            Error::Mount(_) => f.write_str("Mount"),
            Error::TargetInstance => f.write_str("TargetInstance"),
            Error::Channel(_) => f.write_str("Channel"),
            Error::Exclusivity(signal) => write!(f, "Exclusivity({signal:?})"),
            Error::Unqualified(_) => f.write_str("Unqualified"),
            Error::Identity(_) => f.write_str("Identity"),
            Error::Deadline => f.write_str("Deadline"),
            Error::Cancelled => f.write_str("Cancelled"),
            Error::Scratch => f.write_str("Scratch"),
            Error::Cleanup => f.write_str("Cleanup"),
            Error::Consumed => f.write_str("Consumed"),
            Error::Scope(_) => f.write_str("Scope"),
            Error::Incompatible(_) => f.write_str("Incompatible"),
            Error::Binding(_) => f.write_str("Binding"),
            Error::Read(_) => f.write_str("Read"),
        }?;
        write!(f, " recovery_names={:?}", self.0.recovery_names)
    }
}

pub(super) enum FixtureFailure<'a> {
    Open(&'a ServerFailure),
    // The observation helper returns an unstructured backend string; its
    // stage is known, but its contents are not a safe error category.
    AdministrativeObservation,
}

impl fmt::Display for FixtureFailure<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(failure) => write!(f, "open_scratch: {}", SafeFailure(failure)),
            Self::AdministrativeObservation => f.write_str("AdministrativeObservation"),
        }
    }
}

/// Cleanup remains fatal. Report both stages before a discard refusal can
/// replace the failure that made the fixture discard its real owner (#1434).
pub(super) fn require_discarded(
    original: FixtureFailure<'_>,
    discarded: Result<(), ServerFailure>,
) {
    if let Err(discarded) = discarded {
        panic!(
            "fixture failure: original={original}; discard={}",
            SafeFailure(&discarded)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::server::{Premise, Signal, generated_names};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    const PRIVATE: &str = "password=fixture-secret; endpoint=private://fixture; SELECT private_sql";

    fn owned_failure(cause: Error) -> ServerFailure {
        let names = generated_names().unwrap();
        ServerFailure {
            cause,
            recovery_names: vec![names.database().to_owned(), names.login().to_owned()],
        }
    }

    fn panic_message(operation: impl FnOnce()) -> String {
        let panic = catch_unwind(AssertUnwindSafe(operation)).expect_err("discard must stay fatal");
        if let Some(message) = panic.downcast_ref::<String>() {
            message.clone()
        } else {
            panic.downcast_ref::<&str>().unwrap().to_string()
        }
    }

    #[test]
    fn dual_refusals_retain_both_stages_and_their_owned_names_without_backend_text() {
        let original = owned_failure(Error::Channel(PRIVATE.into()));
        let discarded = owned_failure(Error::Cleanup);
        let cleanup_names = discarded.recovery_names.clone();
        let message = panic_message(|| {
            require_discarded(FixtureFailure::Open(&original), Err(discarded));
        });
        assert!(
            message.contains("original=open_scratch: Channel"),
            "{message}"
        );
        assert!(message.contains("discard=Cleanup"), "{message}");
        for name in original.recovery_names.iter().chain(&cleanup_names) {
            assert!(message.contains(name), "missing {name}: {message}");
        }
        for private in ["fixture-secret", "private://fixture", "private_sql"] {
            assert!(!message.contains(private), "{message}");
        }
    }

    #[test]
    fn successful_discard_preserves_the_original_refusal_for_retry_classification() {
        let original = owned_failure(Error::Exclusivity(Signal::Unreadable));
        let names = original.recovery_names.clone();
        require_discarded(FixtureFailure::Open(&original), Ok(()));
        assert!(matches!(
            original.cause,
            Error::Exclusivity(Signal::Unreadable)
        ));
        assert_eq!(original.recovery_names, names);
        // Names still prevent a retry even when discard subsequently succeeds.
        assert!(!original.recovery_names.is_empty());
    }

    #[test]
    fn successful_discard_keeps_an_ordinary_exclusivity_refusal_retryable() {
        let original = ServerFailure {
            cause: Error::Exclusivity(Signal::SessionList),
            recovery_names: Vec::new(),
        };
        require_discarded(FixtureFailure::Open(&original), Ok(()));
        assert!(matches!(
            original.cause,
            Error::Exclusivity(Signal::SessionList)
        ));
        assert!(original.recovery_names.is_empty());
    }

    #[test]
    fn observation_and_discard_failures_keep_distinct_safe_categories() {
        let discarded = owned_failure(Error::Daemon(PRIVATE.into()));
        let names = discarded.recovery_names.clone();
        let message = panic_message(|| {
            require_discarded(FixtureFailure::AdministrativeObservation, Err(discarded));
        });
        assert!(
            message.contains("original=AdministrativeObservation"),
            "{message}"
        );
        assert!(message.contains("discard=Daemon"), "{message}");
        for name in names {
            assert!(message.contains(&name), "{message}");
        }
        assert!(!message.contains("fixture-secret"), "{message}");
    }

    #[test]
    fn every_error_category_omits_payloads_and_retains_structured_signals() {
        let cases = [
            (
                Error::UnsupportedProfile {
                    name: PRIVATE.into(),
                    implemented: PRIVATE.into(),
                },
                "UnsupportedProfile",
            ),
            (Error::Endpoint, "Endpoint"),
            (Error::Daemon(PRIVATE.into()), "Daemon"),
            (Error::Container(PRIVATE), "Container"),
            (Error::Configuration(PRIVATE), "Configuration"),
            (Error::EngineExecutable, "EngineExecutable"),
            (
                Error::Containment(Premise::Occupants),
                "Containment(Occupants)",
            ),
            (Error::Mount(PRIVATE.into()), "Mount"),
            (Error::TargetInstance, "TargetInstance"),
            (Error::Channel(PRIVATE.into()), "Channel"),
            (
                Error::Exclusivity(Signal::Unreadable),
                "Exclusivity(Unreadable)",
            ),
            (Error::Unqualified(PRIVATE), "Unqualified"),
            (Error::Identity(PRIVATE.into()), "Identity"),
            (Error::Deadline, "Deadline"),
            (Error::Cancelled, "Cancelled"),
            (Error::Scratch, "Scratch"),
            (Error::Cleanup, "Cleanup"),
            (Error::Consumed, "Consumed"),
            (Error::Scope(PRIVATE.into()), "Scope"),
            (Error::Binding(PRIVATE.into()), "Binding"),
        ];
        for (cause, category) in cases {
            let failure = owned_failure(cause);
            assert_eq!(
                SafeFailure(&failure).to_string(),
                format!("{category} recovery_names={:?}", failure.recovery_names)
            );
        }
    }
}
