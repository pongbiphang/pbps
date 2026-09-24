# Policies

The policy rules, their settings and suppressions, and what a refusal writes.
Part of the [decision record](../DECISIONS.md), which says how to add an entry
here.

<a id="decision-64"></a>

64. **A rule's finding carries the rule's id, and the envelope's id became a
    `String`.** Findings from the catalogue are what a project re-weights and
    suppresses by, so the id they reach CI under is the rule's own
    (`naming.column`), not a `policy.*` wrapper around it. The envelope's
    `Finding::id` was a `&'static str`; every id was a literal until now, but
    a rule id read out of a plan file is not, and a leak or a lookup table to
    keep it static would have been a workaround for a type that no longer
    described the data.

<a id="decision-65"></a>

65. **A block with problems is refused whole at the plan point.** `validate`
    lists what is wrong with `policies:`; `plan` evaluates none of it until
    that is fixed. Running the rules that parsed would report against a
    configuration the project did not manage to write, and a plan that
    passed under it would pass for the wrong reason.

<a id="decision-66"></a>

66. **A rule setting accepts the YAML boolean `false` as `off`.** The word
    `off` is a boolean to the loader (so are `on`, `yes` and `no`), and
    `naming.table: off` is what an operator will write. Refusing it as a type
    error, or demanding quotes, would make the most common setting the one
    that fails; `true` is refused instead, because "on" names no severity.
    ADR-0008 "Implementation status" item 6.

<a id="decision-79"></a>

79. **A suppression's `until` is checked against the calendar.** Compared as
    text, `2026-02-31` kept a suppression alive to the end of February and
    expired it on March 1, for a date that never comes; `2025-02-29` the
    same. The day is checked against its month, leap years included.

<a id="decision-82"></a>

82. **A rule switched on without what it runs on is refused, in either
    spelling.** `naming.table: error` checked no name — an error-level
    policy that accepted everything — and `change.window: error` had no
    window and refused every plan; the bare-word form skipped the parameter
    checks entirely. The catalogue names each rule's `required` parameters,
    and the block's check refuses an enabled setting that lacks one, whether
    it was switched on by a word, by its own severity, or by the catalogue's
    default.

<a id="decision-96"></a>

96. **A clock field in a window is two digits, checked before it is read.**
    Rust's integer parse takes a sign, so `+01:-30` was thirty minutes
    east and `+9:00` a valid hour, and the window was measured at a time
    nobody wrote. The offset's and the time's fields are two ASCII digits
    each, and anything else is refused by name — the shape `parse_date`
    already had (78).

<a id="decision-107"></a>

107. **A suppression's `until` is compared as it was validated.**
    `parse_date` trims, the lexical comparison did not, and `" 2026-12-31"`
    sorted before today: a future suppression expired at once, and a
    trailing space kept one alive on its own day. One text for both.

<a id="decision-111"></a>

111. **`pull` draws its row-count line from the `data.max-rows` rule, not
    from `max_data_rows`.** That field is only the rule's default parameter
    (ADR-0008). Read directly, a project that raised the threshold or turned
    the rule off was warned by `pull` about files `validate` accepts, and one
    that lowered it was handed files the next `validate` rejects — the two
    commands disagreeing about the same declaration, which is the failure
    ADR-0008 exists to remove.

<a id="decision-114"></a>

114. **`pull` refuses to write when `data.max-rows` is `error`.** 111 made
    `pull` read the rule instead of the field but kept only the row count,
    so a project that had set the rule to `error` still got a warning and
    the files — which the very next `validate` rejects. The severity travels
    with the count now, and at `error` nothing is written at all: refusing
    before the write is the difference between "no files" and "files you
    must now delete by hand".

<a id="decision-120"></a>

120. **`pull --data` draws its row line through `validate`'s own
    evaluation, not a count of its own.** 111 and 114 read the rule's
    severity and count, but the loop was `pull`'s and never consulted the
    suppressions, so a table the project had excused by name was refused at
    the moment it was pulled and accepted by the next `validate`. The rule is
    evaluated by the same function `validate` calls, over the pulled schema
    and narrowed to `data.max-rows`, so the two commands cannot disagree
    again; a block with problems contributes nothing, as it contributes
    nothing to a plan.

<a id="decision-154"></a>

154. **A plan a policy refuses writes nothing, the identity file included.**
    ADR-0008 says an `error` refuses to produce the plan before any file is
    written, and the code said so too — in a comment above the policy gate,
    with `write_ids` a hundred lines *higher*. So a revision that minted a uid
    and then failed a rule left `pbps.ids.json` changed for a plan that does
    not exist, and the next run compared against identities no reviewed plan
    ever used. The `--check` branch stays where it is, since it writes
    nothing and answers a different question; only the write moves, past the
    gate.

<a id="decision-172"></a>

172. **The editor schema spells the rule catalogue, because a schema that
    blesses what the loader refuses is worse than no schema.**
    `policies.rules` is a `BTreeMap<String, RuleSetting>`, and the derive turns
    that into `additionalProperties: {$ref: RuleSetting}` — any key at all. So
    an editor completing and validating against the published schema accepted
    `naming.tabel`, marked the file correct, and left the typo for
    `pbps validate` to find later. The catalogue is closed *on purpose*
    (`rules.rs`: "a typo in `policies:` is refused by name rather than
    silently configuring nothing"), which is exactly the knowledge the schema
    was throwing away.
    This is the rule `integration.rs` already states about `deny_unknown_fields`
    — "a schema that made it optional would let an editor bless a file the
    loader rejects, worse than shipping none" — applied to keys instead of
    fields. `rules_schema` writes the ten ids out of `rules::RULES`, with
    `additionalProperties: false` and each key carrying the catalogue's own
    sentence, so there is still one list and not two. Swept: a suppression's
    `rule` is the same closed set and gets the same treatment, because
    suppressing a rule that does not exist suppresses nothing.
    **Not swept into severity, deliberately.** `severity` and the bare-word
    form are also a closed set to the loader — but `Severity::from_str` trims
    and lowercases, so a JSON Schema `enum` of the four words would refuse
    `Error`, which the loader accepts. A schema *stricter* than the loader is
    the mirror image of the bug above, not a further fix of it: it reports a
    valid file as wrong. Both halves of "the schema is the loader" have to
    hold, and only the ids can state it exactly — `rules::rule` compares with
    `==`.
    `SCHEMA_VERSION` goes to 5 for the reason it exists: an editor notices, in
    the way that matters most to it.

<a id="decision-187"></a>

187. **A policy rule sees types in the dialect's spelling.**
    `column.no-deprecated-type` matched `spec.ty.base` against `text`, `ntext`
    and `image`, and `national text` *is* `ntext` — the alias is in
    `types.rs`. A default-on rule was bypassed by writing the deprecated type
    the long way.
    `pbps-policy` does not depend on any dialect and should not: what a type
    *is* is dialect knowledge, what to think of it is the policy's. So the
    normalizing happens at the boundary, in the caller that already holds a
    dialect — `validate_findings` hands the evaluator a schema whose column
    types are canonical. The rule is unchanged, and so is every future rule
    that names a type.
    A type the dialect cannot normalize is passed through exactly as written:
    it is already an error from `refuse_invalid_declarations`, and rewriting
    what a finding quotes would make the message name something the file does
    not say.

<a id="decision-188"></a>

188. **Each rule's schema is that rule's own shape.** 172 closed the rule
    *catalogue* so an editor could not bless `naming.tabel`. Every rule then
    got the same `RuleSetting` schema, so the editor still blessed
    `naming.table: {rows: 5}` — a parameter that rule does not take and
    `Policies::check` refuses. Each entry is generated from the catalogue's
    own `params` now, with `additionalProperties: false`, so the schema and
    the checker draw the same line. The boolean form is `false` alone: `true`
    says nothing about the severity and is refused.
    Two things stay open on purpose. The **severity word** is a plain string,
    not an enum, for 172's reason — `Severity::from_str` trims and lowercases,
    and a schema stricter than the loader is the mirror image of the bug being
    fixed. And a rule's **required** parameters are not expressed as a JSON
    Schema `required`, because "required unless the severity is off" needs the
    same case-insensitive test and would refuse `naming.table: Off`.
    A parameter's *type* is still written down once, in `RuleConfig`; the
    per-rule schemas say which parameters, never what they are. A test ties
    every generated entry back to `rules::RULES` and fails if a parameter is
    added to the catalogue without a shape.
