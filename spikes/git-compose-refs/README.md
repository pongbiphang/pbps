# Compose ref-protocol measurements

This experiment supports ADR-0015 decision 5 and DECISIONS 487 (#385/#386).
It runs real Git commands in disposable repositories and a linked worktree.
It uses Python's standard library; run it manually on Linux:

```sh
python3 spikes/git-compose-refs/measure.py
python3 spikes/git-compose-refs/measure.py --revert-type-guard
python3 spikes/git-compose-refs/measure.py --revert-head-guard
```

The first command must pass. The second must fail the same-tip symbolic-ref
preservation assertion, after restoring the whole single-shot update. The
third must fail both inserted-HEAD-hop assertions, after restoring the recursive
HEAD read. These intentional failures demonstrate that the proposed guards
answer the measured races. They are not failures in the production test suite.
`observed.txt` records the version, observations and assertion outcomes; reruns
print test results and the actual assertion differences without overwriting it.

Each fixture creates its own temporary repository and linked worktree, without
remotes, and removes only that temporary directory. User/global Git configuration
and inherited Git overrides are excluded. Commands use an empty hooks directory
and disable fsmonitor, as the ADR requires. The experiment never edits this
checkout's index, refs or configuration and does not contact a network.

| Question | Actual boundary exercised |
| --- | --- |
| Does prepare preserve the ref's type until commit? | The live symbolic ref remains readable while the branch lock contains the proposed commit; sibling writers fail on the lock, and abort preserves the live bytes. |
| Does the ordinary CAS still work? | Direct matching tip commits; different direct and symbolic tips refuse preparation without changing the live ref. |
| Which local locks matter? | Preparation refuses a pre-held HEAD lock. The separately prepared index remains locked and uninstalled through successful and refused ref operations. |
| Does the HEAD hop matter before and after the transaction? | A competing process changes this checkout's HEAD to an intermediate ref. The immediate read refuses both windows; a recursive read accepts them. |
| Do the old post-write branch checks still matter? | A branch retarget or tip change after commit and before persistent locks is refused. |

A branch ref is shared with the sibling worktree; HEAD is worktree-local. The
HEAD writer deliberately addresses the composing checkout, not the sibling's
independent HEAD. The fixture pauses at explicit transaction acknowledgements
or after commit, so it does not depend on winning a timing race.

The experiment measures **ref operations and the success predicate only**. It
does not implement compose, undo file placements, publish a recovery record,
install an index, display a refusal, or push. The prepared index is retained for
comparison; an unchanged index here is not evidence that the future UI handles
every failure path. #494 / #64 step 4 must exercise the complete protocol and
crash recovery, including the immediate HEAD check during success recovery.
Windows restoration (#471), macOS, other Git versions and non-files backends
are outside these Linux measurements.
