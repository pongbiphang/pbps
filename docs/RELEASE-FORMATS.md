# Releasing the three formats

The plan file, the ledger snapshot and the published schema set each have a
number and a fingerprint (DEC-2412.1, DEC-2412.2, DEC-2412.3). A release freezes
both for each of them and archives the schema set. This page is the whole
procedure: there is no `pbps` command for it and no script, because the
fingerprints come out of the binary's own types and a release does it once.

What the release writes, and nothing else:

- `crates/pbps-model/src/released_formats.json`: the tag, and the number and
  fingerprint of the plan, the state and the schema set. It reads
  `{"released": null}` until the first release.
- `crates/pbps-cli/tests/fixtures/published-schemas/released/<n>/`: the three
  published documents of the set, `<n>` being the set's number. The numbered
  directories beside it are history and are never touched.

## Before the freeze

1. Start from the commit the tag will name, on a clean worktree cut from the
   latest `origin/master`, with no format-changing pull request in flight.
2. Settle the numbers. Before the first release that is #1159's reset: the three
   constants go to 1 and `state::OLDEST_READABLE_VERSION`'s legacy path goes.
   After a release each number is already the released one, or the released one
   plus one if a change since then moved its format; the rule tests say so.
3. When the set's number changed, regenerate the checked-in copies, because they
   carry it: `pbps schema --kind <declaration|config|envelope> --out
   schemas/<file>` for each of `declaration.schema.json`, `pbps.yml.schema.json`
   and `envelope.schema.json`.
4. Run the checks once: `cargo test -p pbps-model` and
   `cargo test -p pbps-cli --bin pbps integration::`. They pass before the
   freeze or the freeze would record a build that fails them.

## The freeze

Run, with the tag typed out (the variable's value is the tag, and unset it does
nothing, so no ordinary `cargo test` can freeze):

```bash
PBPS_FREEZE_FORMATS=v1.0.0 cargo test -p pbps-cli --bin pbps \
  integration::release_formats::freeze_the_formats_for_a_release
```

It writes the record and the archive, reads the record back with the loader's
own parser and runs the `check` conditions on the result, so a freeze that
produces something the checks refuse fails at once.

- It refuses a blank tag.
- It reads the record it replaces before it writes anything, and holds each of
  the three formats of this build to it with the same increment rule the `check`
  mode uses: a format changed under its released number, or a number moved with
  nothing changed, is refused and nothing is written. Without this the freeze
  would check the build against the record it has just written, which always
  agrees, and a missed increment could not be found again. A record that is
  missing or does not parse is refused, naming its path; it is never read as "no
  release", because in the repository the file always exists.
- It never rewrites an archive. When `released/<n>/` already exists it is reused
  if it holds this set, which is what a second release finds when the documents
  did not change since the first, and the freeze stops if it holds another set.
  In that case the set changed without its number moving, which is the thing the
  rule test exists to refuse.
- It writes the archive into a directory it makes itself. If a file write fails,
  it removes that directory and reports the write's error, so a failed freeze
  leaves no partial `released/<n>/` for the next one to trip on. If the removal
  fails too, the message says the directory remains and names it.
- It writes the record last. If writing the record fails, the archive is complete
  and stays, and a second freeze reuses it.

## After the freeze

1. Run `cargo test -p pbps-model -p pbps-cli`. The record is compiled in, so this
   rebuilds with the new one. The `check` mode, `the_released_record_matches_its_archive_and_this_build`,
   now holds the build to the record: the archive exists and hashes to the frozen
   fingerprint, and no format sits at a frozen number with another fingerprint.
2. Expect the envelope corpus to read the new `released/<n>/envelope.schema.json`
   (`envelope_archives.rs`, DEC-2412.3): every emitted envelope must keep
   validating against the released set's envelope schema while the wire version is
   unchanged. A released directory with no readable envelope fails that test.
3. Read the diff. It holds the record and three new files under `released/<n>/`,
   plus whatever the reset changed. No file under an older numbered archive
   directory changes.
4. Commit the record and the archive with the code that produced them, and tag
   that commit.

## The next cycle

The first pull request that changes a format after a release writes the
released number plus one into that format's constant, once. The rule tests
(`the_plan_version_follows_the_increment_rule`, the state's, and the set's
`the_schema_set_version_follows_the_increment_rule`) fail a change that did not,
and a number that moved with no change. A second pull request in the same cycle
writes nothing, or the same line, which merges without a conflict. Nobody adds a
history comment or an archive per change.

## Not part of this procedure

- The reset of the numbers to 1, and the removal of the legacy reading path
  (#1159).
- A check of an older number against the frozen record: a row at the number the
  release froze, written by a build from before the release (DEC-2412.3).
- The envelope's wire version, which the release does not touch (DEC-997.1,
  DEC-1734.1).
