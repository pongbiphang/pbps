# Sourced by both live runners; setup keeps the caller's errexit semantics.
# DEC-2620.1: later invocations must still publish receipts after a failed test.
live_test_status=0
cargo() {
    local status
    # command preserves the driver's PATH proxy without recursing into this
    # shell-local wrapper. Child helpers retain their own failure handling.
    if command cargo "$@"; then
        return 0
    else
        status=$?
    fi
    # A timeout or interruption ends this runner before it starts more work.
    if (( status >= 128 )); then
        exit "$status"
    fi
    if [[ $live_test_status == 0 ]]; then
        live_test_status=$status
    fi
    return 0
}
