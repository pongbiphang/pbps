# Published schema contracts

Each numbered directory is a fixed copy of the three published JSON Schema
kinds carrying that `x-pbps-schema-version`. Tests select the directory using
the binary's published schema-set version, independently of the mutable copies
in the repository's top-level `schemas/` directory.

Keep existing directories unchanged. When a change to the published schema
contract merges, increment `integration::SCHEMA_VERSION` once for the complete
change, regenerate the top-level copies, and add the complete new set under the
new number. Rebase a pending change onto the latest published set before
choosing its number. Do not refresh an existing archive to make a test pass.

Comparison uses parsed JSON and excludes only the top-level
`x-pbps-tool-version` annotation. Whitespace and object-key order do not count;
all other content, including editor descriptions and array order, does. This
conservative equality check guarantees equal validation behavior for the same
version without attempting general JSON Schema equivalence. It also catches a
constraint or enum edit when the generator and top-level copies change together.

- `9/` is copied verbatim from commit
  `9a01bfa85a7d6ac2832b867a979f8870eea61899`. It is a real legacy publication
  accepting `state list`'s `limit: 0`, not a reconstruction using today's types.
  Version 9 was subsequently reused for different documents; this fixture is
  evidence of that ambiguity, not a claim that every historical version-9
  document agreed. The earlier `8928a71` commit changed the binary's constant to
  9 but its checked-in documents still carried 8, so those copies are not used
  as a version-9 archive.
- `13/` adds a module's `public_execute:`, the declaration of whether a routine
  keeps the engine's default `EXECUTE` to `PUBLIC` (issue #318, ADR-0010 §5).
- `10/` starts the fixed-version contract and includes all accumulated changes
  through issue #191. Existing copies labeled 9 cannot be disambiguated by that
  number alone; regenerate from an updated binary or retain the exact document.

The schema-set version is independent of the emitted envelope's
`schema_version` and the saved-plan/state format versions (SPEC 9.8, 14.2;
DECISIONS 465).
