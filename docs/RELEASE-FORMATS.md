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
- It never rewrites an archive. When `released/<n>/` already exists it is reused
  if it holds this set, which is what a second release finds when the documents
  did not change since the first, and the freeze stops if it holds another set.
  In that case the set changed without its number moving, which is the thing the
  rule test exists to refuse.
- It writes the record last. If writing the record fails, the archive stays and a
  second freeze reuses it.

## After the freeze

1. Run `cargo test -p pbps-model -p pbps-cli`. The record is compiled in, so this
   rebuilds with the new one. The `check` mode, `the_released_record_matches_its_archive_and_this_build`,
   now holds the build to the record: the archive exists and hashes to the frozen
   fingerprint, and no format sits at a frozen number with another fingerprint.
2. Read the diff. It holds the record and three new files under `released/<n>/`,
   plus whatever the reset changed. No file under an older numbered archive
   directory changes.
3. Commit the record and the archive with the code that produced them, and tag
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
