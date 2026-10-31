//! Retry the kernel's write/exec race for freshly written test stand-ins.
use std::io::{self, ErrorKind};
use std::time::Duration;

/// A different test's fork can retain the script's closed write descriptor
/// until exec. Only that typed spawn error is transient; an exit status or
/// diagnostic from a successfully started process is never retried.
pub fn retry_exec<T>(operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    retry_with_budget(operation, 200, Duration::from_millis(10))
}

fn retry_with_budget<T>(
    mut operation: impl FnMut() -> io::Result<T>,
    retries: usize,
    interval: Duration,
) -> io::Result<T> {
    let mut result = operation();
    for _ in 0..retries {
        if !matches!(&result, Err(error) if error.kind() == ErrorKind::ExecutableFileBusy) {
            return result;
        }
        std::thread::sleep(interval);
        result = operation();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_file_busy_is_retried_until_the_stand_in_starts() {
        let mut attempts = 0;
        let result = retry_exec(|| {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::from(ErrorKind::ExecutableFileBusy))
            } else {
                Ok("started")
            }
        });
        assert_eq!(result.unwrap(), "started");
        assert_eq!(attempts, 3);
    }

    #[test]
    fn other_errors_return_immediately_even_when_their_text_says_busy() {
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::PermissionDenied,
            ErrorKind::WouldBlock,
            ErrorKind::Interrupted,
            ErrorKind::Other,
        ] {
            let mut attempts = 0;
            let result: io::Result<()> = retry_exec(|| {
                attempts += 1;
                Err(io::Error::new(kind, "Text file busy"))
            });
            let error = result.unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(error.to_string(), "Text file busy");
            assert_eq!(attempts, 1);
        }
    }

    #[test]
    fn a_persistent_busy_error_returns_the_last_error_when_the_budget_ends() {
        let mut attempts = 0;
        let result: io::Result<()> = retry_with_budget(
            || {
                attempts += 1;
                Err(io::Error::new(
                    ErrorKind::ExecutableFileBusy,
                    format!("attempt {attempts}"),
                ))
            },
            3,
            Duration::ZERO,
        );
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ExecutableFileBusy);
        assert_eq!(attempts, 4);
        assert_eq!(error.to_string(), "attempt 4");
    }
}
