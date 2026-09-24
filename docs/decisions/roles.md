# Roles, grants and permissions

Managed roles, what they are granted, and how permissions are compared and
changed. Part of the [decision record](../DECISIONS.md), which says how to add
an entry here.

<a id="decision-56"></a>

56. **A role change has no table; `Change::table()` became an `Option` and
    `subject()` is the label.** Every earlier change acted on an object in the
    tables-and-modules namespace, and `table()` returning a name unconditionally
    encoded that. A role is a principal. Returning a synthetic
    `TableName::new("role", name)` would have compiled and grouped plans
    correctly, and would have put a fake object name into strategy lookups and
    the edition check. The `Option` makes every caller say which it wanted.

<a id="decision-57"></a>

57. **`grant-widen` is a risk class that is never gated.** `RiskClass::is_gated`
    exists so a class can be labelled for both review layers without being a
    flag: `unapproved_risks`, the `--allow` advice under a plan, `apply`'s
    refusal and `explain`'s approval command all use `gated_risks()`. A flag
    typed on every deployment that adds a permission is a flag typed by rote,
    which protects nothing and teaches the wrong habit.

<a id="decision-58"></a>

58. **Grants are compared under the target's post-plan name, and never revoked
    on an object the plan drops.** The base side's grants are brought forward
    through the plan's table renames by uid before the per-target comparison,
    because `sp_rename` carries the permissions with the object; without that a
    renamed table produced a `REVOKE` on a name that no longer exists at the
    point the revokes run. A `REVOKE` on a table or module this plan drops is
    skipped for the mirror reason: the drop removes the permission, and the
    statement would fail after it.

<a id="decision-59"></a>

59. **Inside a managed role, only grants on managed objects are compared.** The
    declarations may not name an undeclared object (the `validate` rule), so a
    grant on one could never be declared — and comparing it would have every
    plan revoke it. `scope()` drops such grants from the live side; schema
    grants stay, and a role outside the ids file is unmanaged like a table. The
    cost is that a hand-made grant on somebody else's table is invisible to
    `verify`, which is the same line the managed set already draws for the
    table itself.

<a id="decision-63"></a>

63. **A dropped role's members are written into the connected plan, never
    found at apply time.** The engine refuses `DROP ROLE` while members
    remain, and the two obvious answers were both wrong: leaving it to the
    engine made every role drop fail in production, and removing members with
    dynamic SQL at apply time would run statements the reviewed plan never
    showed. `plan --db` reads `sys.database_role_members` for the roles it
    drops and puts each member into `DropRole::members`, so the plan lists
    who is removed and the emitter writes one `ALTER ROLE ... DROP MEMBER`
    per name before the drop. Membership stays undeclared and uncompared;
    this is the consequence of a drop the user asked for with a reason.

<a id="decision-72"></a>

72. **A securable this plan drops and creates again is granted from
    nothing.** `DROP` takes an object's permissions with it, so a table
    replaced under the same name, or a module changing kind, comes back bare;
    the grant differ compared the base's grants to the declared ones as
    text, found them equal, and wrote no `GRANT` — a successful apply that
    silently removed a role's access. On a dropped target the base side is
    now empty, so every declared permission is a `GRANT`, ordered after the
    `CREATE`, and the `REVOKE` stays unwritten as before (58).

<a id="decision-76"></a>

76. **Role files live in `roles/`, not under a `.role.yml` suffix.** A table
    `app_reader.role` is a legal name and its file is `app_reader.role.yml`
    — the role `app_reader`'s file exactly — so `pull` wrote the role over
    the table without a word. No table file is ever written under `roles/`,
    so the two can no longer name one path; the loader finds every `.yml`
    under the schema directory and tells a role by its content, so a
    hand-written role file anywhere still loads.

<a id="decision-78"></a>

78. **A role name may contain a dot.** `[app.reader]` is a legal principal
    name; the loader refused it as "not in a schema", and `pull` wrote it
    back as it is, so the freshly pulled project failed to load. The emitter
    quotes the name; nothing parses a dot in it.

<a id="decision-83"></a>

83. **A connected plan refuses to drop a role that owns a securable.** The
    engine refuses the `DROP ROLE`, and a staged apply would already have
    committed every `DROP MEMBER` before finding out — users without access,
    the role still there. Ownership is read with the memberships and the
    plan is refused by name with the `ALTER AUTHORIZATION` to run by hand:
    moving ownership is a decision about who owns a schema, not a
    consequence of a drop pbps gets to make.

<a id="decision-88"></a>

88. **The owned-securable check names every class the catalog carries an
    owner for.** 83 listed the classes that came to mind — schema, object,
    type, assembly, certificate, keys — and missed a role that owns another
    role (`sys.database_principals.owning_principal_id`), exactly the drop a
    staged apply would commit every `DROP MEMBER` for before failing. The
    list is now the catalog views with a `principal_id` or
    `owning_principal_id` column, read off a live server rather than
    recalled: roles, XML schema collections, full-text catalogs and
    stoplists, search property lists, the Service Broker objects,
    database-scoped credentials, event notifications, external languages
    and libraries. The views that arrived after 2008 are probed for first,
    so an older engine answers with the classes it has. A class enumerated
    from memory is a filter nobody re-reads.

<a id="decision-89"></a>

89. **A grant's permissions are checked against what the target is, with the
    engine's table.** `GRANT EXECUTE` on a table and `GRANT SELECT` on a
    procedure both pass the name checks and are both refused by the engine
    (Msg 4606) — in a staged apply after every earlier change has committed.
    `validate` now applies the engine's rule, measured on a live SQL Server
    2025 for every pair a declaration can spell: a table, a view and an
    inline table-valued function take everything but `execute`; a procedure
    and a scalar function take `execute`, `references`, `alter` and
    `view-definition`; a multi-statement table-valued function takes
    `select`, `references`, `alter` and `view-definition`; a trigger takes
    nothing (Msg 15151: `GRANT` cannot even name it); a schema takes all. The
    three kinds of function are told apart by the `RETURNS` clause, read off
    the code so a comment or a literal cannot pass for it. An object the
    declarations do not have is the model's finding and gets no kind here.

<a id="decision-91"></a>

91. **A role rename faces the `rename` gate.** It keeps its membership, which
    is why it is a rename and not drop + add — but the old name is gone the
    same way a table's is, and a module or an application that asks
    `IS_ROLEMEMBER('old')` breaks the moment the statement commits. Nothing
    about keeping the members makes that safe, so `RenameRole` carries
    `RiskClass::Rename` and a plan with one needs `--allow rename`, like
    every other rename.

<a id="decision-92"></a>

92. **`apply` reads a dropped role's members and ownership again before
    statement one.** The plan lists the members at `plan --db` time so a
    reviewer sees who loses the role (ADR-0005 item 6), and the baseline
    checksum cannot notice a member added since — membership is outside the
    managed state on purpose. A staged apply would then commit every `DROP
    MEMBER` the reviewer saw and fail on the one nobody did: users without
    access, the role still there. So the preflight compares the live
    membership with the listed one and refuses on any difference, added or
    gone, asking for a new plan; ownership (83) is asked about again the
    same way. The transactional apply would have rolled back, but a refusal
    that names the member beats a failed statement that does not.

<a id="decision-93"></a>

93. **A statement that renames a role says so, as one that renames a table
    does.** `Statement::renames` exists because a staged checkpoint has to
    find an object under the name the catalog has *now* (ADR-0002 staged
    mode); `ALTER ROLE ... WITH NAME` moved a name the same way and said
    nothing, so a checkpoint after it scoped the role out under its old
    name and a resume could not see what changed on it while paused. The
    second instance of a shape the first one had already named.

<a id="decision-95"></a>

95. **A grant `WITH GRANT OPTION` is unexpressible drift, never the plain
    grant.** The catalog spells it `W`, and folding it into the role's set
    beside a `G` let `verify` compare a role that can now delegate equal to
    the recorded plain grant and say "no drift"; a warning on stderr does not
    make that verdict safe. It is left out of the set and carried beside the
    comparison (`Scoped::unexpressible`), where `verify` reports it as drift
    with the `REVOKE GRANT OPTION FOR` to run by hand and `plan --db` refuses
    to plan over it — restating the plain `GRANT` would leave the option in
    place and the plan never converging. Representing the bit in the model
    is a format change with an emitter half (`WITH GRANT OPTION` on the way
    out) and belongs to an ADR, not a review fix.

<a id="decision-97"></a>

97. **Every permission the model cannot hold on a managed role is
    unexpressible drift, not a warning.** 95 made the grant option so and
    left a DENY, a column-level grant, a permission outside the closed set
    and a grant on an unmodelled object as warnings on stderr — and a
    managed role that gained a column-level `SELECT` on a sensitive column
    out of band compared equal on the sets that remained. All of them now
    travel the same way: left out of the set, carried beside the comparison,
    reported by `verify` as drift and refused by `plan --db`. `pull` still
    prints them as warnings, since nothing is being compared yet. The
    second instance of 95's shape, swept the same day.

<a id="decision-102"></a>

102. **A staged resume re-checks a role drop for the members whose
    statements have not run.** 92 read the membership again before
    statement one; a checkpoint taken between two `DROP MEMBER`s left the
    same window open until `--resume`, whose drift check cannot see a
    member (membership is outside the checksum on purpose). The expectation
    is counted off the emitter's own statements — every listed member, minus
    the ones whose `DROP MEMBER` committed, and none once the `DROP ROLE`
    has — so the resume asks about the role as the plan left it.

<a id="decision-105"></a>

105. **The permission read takes every class, and the ones the model does
    not hold are unexpressible drift.** The catalog query kept classes 1 and
    3 (an object, a schema), so a `GRANT CONTROL TO role` or `GRANT CREATE
    TABLE TO role` (class 0, the database itself) never reached 97's
    handling: it was filtered out before anything could report it, and a
    managed role that had gained the whole database compared equal on the
    grants it still held. The query filters nothing by class now; the
    assembler names the database-level ones and, for every other class, the
    catalog's own class name. The third instance of 95's shape — a
    permission the model cannot hold, dropped before the comparison — found
    one filter further up each time.

<a id="decision-110"></a>

110. **A permission the declarations cannot express stops every command that
    records a state, not only `plan --db`.** 95 carried a managed role's
    `WITH GRANT OPTION`, and 97 and 105 the column-level, `DENY` and
    database-level grants, beside the comparison rather than inside it, and
    `plan --db` refused to plan over them. `snapshot` and `baseline` still
    wrote the schema down without them, so a privilege change could be
    recorded as a clean state that the very next `verify` reported as drift.
    All three now stop at one guard. `snapshot --force` is not an escape: it
    answers "record a state that differs from the recorded one", and this is
    "record a state pbps cannot express at all", which has no right answer to
    force.
    `apply` stops at the same guard, before statement one rather than at its
    closing snapshot: the baseline checksum cannot see such a permission
    either, so one that appeared between plan and apply arrives as a clean
    baseline and would be written down by the entry that closes the
    deployment — and a refusal after the statements committed would be the
    worse failure of the two.

<a id="decision-118"></a>

118. **A declared role's name is checked against every database principal
    before a connected plan or a bootstrap is written.** The managed set
    knows the roles, and a role named like an existing *user* or
    application role looked free — SQL Server keeps users, roles and
    application roles in one namespace, and the `CREATE ROLE` failed after
    the tables and rows ordered before it had run. `sys.database_principals`
    of every type but `R` is read once, and a taken name is refused with
    the principal's kind, before anything runs.

<a id="decision-119"></a>

119. **Which principal holds a role's name is the engine's call, for a
    rename's target as much as a creation's, and it is asked again before
    apply.** 118 read the principals into a map and looked the created names
    up in it: a rename onto a user's name was never looked up, and `Shadow`
    was free to the map while `shadow` was taken to the database, whose
    collation says they are one name. The names a plan's remaining
    statements need free (`CREATE ROLE`, the new name of a rename) and the
    ones they free first (`DROP ROLE`, the old name of a rename) go to the
    engine in one query, compared under `COLLATE DATABASE_DEFAULT`, and the
    answer names the holder in its own spelling. Asked by `plan --db` and
    `bootstrap`, and again by `apply` before statement one and on a staged
    resume: a principal is outside the managed state, so the checksum cannot
    see one created in between, as 92 and 102 say of a member.

<a id="decision-126"></a>

126. **Two spellings of one grant target in a role file are refused, not
    merged.** `SCHEMA::dbo` and `schema::dbo`, or a target with
    surrounding whitespace, parse to one `GrantTarget`, and the map kept
    whichever the loader met last: the other's permissions were gone, and
    the next connected plan revoked them. The loader remembers the spelling
    each parsed target was first written in and refuses the second by both
    spellings, the way a column named twice is refused; merging the two
    lists would hide a declaration that says two different things.

<a id="decision-127"></a>

127. **Roles dropped together are dropped parent before member.** A connected
    plan removes a dropped role's members by name before its `DROP ROLE`, and
    every `DropRole` sorted at the same rank — so the order fell to the name
    tiebreaker. Dropping member role `a` before parent role `z` left `ALTER
    ROLE [z] DROP MEMBER [a]` naming a principal that was already gone, and
    the engine refuses that by name: an otherwise valid transactional apply
    rolled back in full. Measured on a live server rather than reasoned: the
    `DROP MEMBER` after the member is gone fails, while dropping a role that
    is a *member* of another succeeds and takes the membership with it. The
    rank is the depth among the dropped roles alone, so a chain of three is
    ordered too, and a member the plan does not drop contributes nothing.

<a id="decision-139"></a>

139. **The role drops are re-ordered after `plan --db` fills their members.**
    127 ranked the drops parent before member in the differ's sort — where
    every `DropRole` still has no members, because they are read from the
    environment afterwards and written into the plan. The rank was every
    role at depth zero, the name tiebreaker decided, and the live case that
    measured 127 passed because it handed the emitter changes with the
    members already in. The same ranking is now applied again, over the
    slots the drops already hold, once the members are known; a plan whose
    dropped roles hold none of each other keeps its order, and nothing else
    moves. Measured end to end this time, through `plan --db` and `apply`,
    with names chosen so that the name order is the wrong one.

<a id="decision-142"></a>

142. **A `schema::` grant target the database spells differently is refused,
    before a plan is written that could never converge.** Every other name in
    a declaration is matched by an identity: a table renamed or recased is the
    same uid, so the two sides agree by construction. A schema has no uid
    (ADR-0002) and a grant target names it as text, so `schema::DBO` against a
    database whose schema is `dbo` is compared as text and differs. Measured
    rather than reasoned: on the case-insensitive test server `GRANT SELECT ON
    SCHEMA::[DBO]` succeeds, the catalog reports the schema as `dbo`, and the
    plan immediately after a successful bootstrap proposes `revoke select on
    schema::dbo` and `grant select on schema::DBO` — for ever, since applying
    it changes nothing about how it reads back. Case-folding the comparison
    would be the wrong fix twice over: on a case-sensitive database the two
    are different schemas, and only the server knows which kind it is. So the
    server is asked — `SCHEMA_NAME(SCHEMA_ID(N'DBO'))` — and a spelling that
    is not the one it returns is refused by `plan --db` and `bootstrap`, the
    sibling of `refuse_misspelt` (101) for the one name with no identity
    behind it. Absent is a different answer from differently spelt, and keeps
    its own message: create it, rather than write it as the database does. The
    pre-flight probe asks the same question again at apply time, for a schema
    created or renamed since the plan was reviewed.

    The sweep found the worse instance: the schema half of a qualified table
    name. The table itself is matched by uid, but the *managed set* is scoped
    by name, so `DBO.customer` on a database whose schema is `dbo` was created
    as `dbo.customer`, recorded as a state holding no tables at all —
    "Bootstrapped: 0 table(s) created", reported as success — and named as
    drift by the `verify` immediately after. Both halves ask the server the
    same question, so both are refused by the same check.

<a id="decision-156"></a>

156. **A role the plan touches is exempt down to the permissions it moves, and
    no further.** 153 made this correction for a table's rows and left the
    other half of the model as 150 had it: a role named by any change was
    exempt whole. So a plan that adds one grant covered every *other*
    permission that role holds — and nothing in the run speaks for those. The
    statements grant and revoke what the plan asked for and say nothing about
    what they left alone; the pinned checksum is answered before they run. A
    session that revokes the role's unchanged `SELECT` in between therefore
    had it recorded as this plan's own result, with `verify` clean against the
    revocation ever after.
    The comparison now takes each target the role holds permissions on, on
    either side, subtracts the permissions this plan grants or revokes there,
    and requires what is left to be equal. `Change::grant` names them, the
    counterpart of `Change::row` and for the same caller. Both ends of a role
    rename are paired first: `ALTER ROLE ... WITH NAME` keeps the grants, and
    the grant changes beside a rename name the role as it will be
    (`order_key` puts the rename first).
    Measured through the CLI, on the shape that made it stageable: `REVOKE`
    runs inside an `AFTER INSERT` trigger. With the whole-role exemption in
    place, a revision that widens the role while inserting a row reports
    `Applied 2 change(s) ... recorded as entry #4` and blesses the
    revocation; with the fix the transaction rolls back naming the role, and
    the grant the trigger took is back.
    **The shape, twice now:** an exemption written for "the plan is answerable
    for this object" is wider than the thing the plan is actually answerable
    for. The unit of a plan's responsibility is what its statements name — a
    row, a permission on a target — never the container those live in.

<a id="decision-157"></a>

157. **A grant target is spelled the way the plan will leave it, before the
    roles are compared.** Measured on SQL Server 2025: `sp_rename` carries an
    object-level grant to the new name — `GRANT SELECT ON dbo.rn_old` reads
    back as a grant on `rn_new` with nothing else changed. So a plan that
    renames a granted table changes no permission, `diff_roles` emits no grant
    change, and the role is not one any change names.
    Which is exactly why 150's comparison broke on it. The role went down the
    *untouched* path and was compared whole, with the baseline holding
    `dbo.old` and the read-back holding `dbo.new` — one grant read as two, and
    **every rename of a granted table refused and rolled back**. A guard
    written to catch someone else's change invented one of its own, which is
    the worse direction of the two.
    So the baseline's grants are re-keyed through the plan's renames before
    either comparison — the whole-role one and 156's per-target one. The
    planned grants need no such treatment: `order_key` puts renames first, so
    a `Grant` or `Revoke` beside one already names the object as it will be.
    **The sweep this missed.** 150 paired both ends of a table rename and 156
    paired both ends of a role rename, because an object is one object under
    two names. A grant *target* is a third place the same rename shows up, and
    naming the first two made it look done. When a rename can be seen from
    three sides, fixing two of them is not fixing it.

<a id="decision-176"></a>

176. **An unsupported permission on somebody else's object is somebody else's
    business, exactly as the ordinary one beside it is.**
    `pbps_diff::scope` drops a managed role's grant on an object outside the
    managed set, and its reason is recorded there: "a grant on somebody else's
    table is that table's business, and comparing it would have the next plan
    revoke a permission the declarations were never allowed to name." But
    `pulled.unexpressible` carried only `(role, message)` — the securable was
    rendered into the text and then gone — so the filter beside it could ask
    one question, and asked the only one it could. A `DENY`, a column-level
    grant or a `WITH GRANT OPTION` on an unmanaged table therefore made
    `verify` report drift and `plan --db` refuse, while the *plain* grant on
    that same table was dropped without comment. One securable, two answers.
    The fix is to keep the target: `Unexpressible { role, target, what }`, and
    one filter both callers use — `verify`'s and `status`'s copies had been
    written twice and could have drifted apart, which is the second half of
    this entry and the reason the helper is shared rather than corrected
    twice.
    Three things stay, and each for its own reason. A **schema** target is
    declarable (`grants: schema::dbo:`), so a DENY on one is a difference the
    declarations genuinely cannot hold. A **targetless** permission — on the
    database itself, or of a class the model cannot name — belongs to no
    object at all, and a role that gained one has changed (DECISIONS 105).
    And `pull` still warns about **every** one of them unfiltered: it is
    writing the declarations rather than comparing them, so there is no
    managed set yet for anything to be outside of.
    Membership is tested against the managed set as *declared* — the ids
    file's tables and the module set — and not against the cut schema. A
    managed module the catalog could not read back is missing from the second
    and is still ours (491edd9); testing against what came back would have
    excused exactly the case that commit exists for.

<a id="decision-210"></a>

210. **`Permission` is the union of the engines' words, and each dialect
    refuses the ones its engine lacks — in three places, from one table.**
    `usage`, `create`, `truncate`, `trigger` and `maintain` join SQL Server's
    eight; `alter` and `view-definition` stay, and become PostgreSQL's to
    refuse. Not a per-dialect enum, for the reason ADR-0010 §6 gives:
    inviolable constraint 1 needs one model in which two identical schemas
    compare equal, and a dialect inside the type would break that. The new
    words are appended, because the derived order is the order `fmt` writes a
    grant's permissions in, and inserting one would rewrite every role file.
    On SQL Server the engine's set is one constant, `validate::PERMISSIONS`,
    and three consumers apply it: `validate_role` refuses a word by name on
    any target (the engine's parser stops at the word before it looks at the
    securable — measured, `GRANT USAGE ON dbo.t TO r` and each of the other
    four, on an object and on a schema, are Msg 102 "Incorrect syntax", not
    Msg 4606), `emit` returns `Unsupported` rather than render a statement
    that parser would stop at, and the catalog read-back reports a parsed
    word the engine lacks as unexpressible rather than fold it into a role.
    The third is belt and braces: measured, `sys.fn_builtin_permissions`
    names none of the five in any class, so SQL Server cannot return one
    today — but a model that spells more words than the engine has is a new
    shape, and the read-back is the one consumer where "parses" used to mean
    "is this engine's". The editor schema lists the union (schema version 7):
    an editor that accepted any string blessed `contrl`, and one listing a
    single engine's words would refuse a PostgreSQL project's `usage`; which
    word a dialect lacks is `validate`'s finding, not the editor's.

<a id="decision-211"></a>

211. **Role existence is a dialect capability, `Dialect::manages_roles`, and
    SQL Server's answer is `true`.** A SQL Server database role lives inside
    the one database the tool is connected to, so ADR-0005 manages its
    existence and nothing here moves. A PostgreSQL role is a cluster object,
    granted in every database of the cluster and visible from each; a tool
    whose blast radius is one database must not own an object whose blast
    radius is the cluster (ADR-0010 §3). That dialect will answer `false`, and
    on it `plan --db` refuses a declared role the cluster lacks with the
    `CREATE ROLE` to run by hand, while `drop-role` revokes the declared
    grants and leaves the `DROP ROLE` to a human. Nothing reads the answer
    yet — the CLI's role-existence paths are written for one engine — and
    the reading lands with the dialect that first answers `false`, where the
    live suite can watch it. Grants are managed either way: the capability is
    about the principal, not what it holds.

<a id="decision-370"></a>

370. **PostgreSQL answers `manages_roles` with `false`, and the differ builds
    no `CreateRole`, `DropRole` or `RenameRole` on such a dialect.** ADR-0010
    §3 and DECISIONS 211 said the answer; this is the reading of it, which
    211 deferred to "the dialect that first answers `false`".

    The three identity changes are conditional and every `Grant` and `Revoke`
    is not: what a role holds in *this* database is the tool's business either
    way, and only the principal is the cluster's.

    - **A declared role with no base entry is granted, not created.** Emitting
      a `CreateRole` and refusing it would refuse the only way a role ever
      comes under management on this engine — a DBA creates it in the cluster
      and the project then declares it. Whether the cluster actually has it is
      a connected question, asked by `pbps_pg::roles::missing_roles` and
      answered with the `CREATE ROLE` to run by hand.
    - **A dropped role has its declared grants revoked and is left standing.**
      A plan that said "drop role" and ran nothing would leave a principal
      holding every permission pbps was managing; one that really dropped it
      would reach every other database in the cluster. The revokes skip a
      target this same plan drops, the rule the per-target comparison already
      applies.
    - **A rename emits nothing.** On this engine an ACL entry holds the role's
      oid, not its name, so a rename performed in the cluster carried every
      grant with it and there is nothing to re-grant. The grant comparison
      then runs against the new name, which is right because the two names are
      one principal.

    The emitter keeps all five arms: `Grant` and `Revoke` render SQL, and the
    other three refuse with the exact statement a human runs — the second lock,
    for a plan that arrived some other way (measured refusal texts in
    ADR-0010 §3, §4).

<a id="decision-371"></a>

371. **The engine's default ACL is the zero point: expanded, reported, and
    never compared as a grant.** ADR-0010 §5 measured this for `PUBLIC` and
    refused to route it down the unexpressible path, because every function
    pbps creates arrives with `EXECUTE` to `PUBLIC` and the very next
    `plan --db` would have refused. The same argument settles the **owner**,
    whom that section does not name: every table pbps creates arrives owned by
    the deploying account with the owner's whole set, so comparing that set
    would have the plan after a successful apply revoke what the apply had
    just produced.

    So the read expands a NULL ACL with the engine's own
    `acldefault(kind, owner)` — never a table of defaults written into this
    crate, which would have been wrong on one of the two servers the suite now
    runs: measured, the owner's default relation ACL is `arwdDxt` on 16.15 and
    `arwdDxtm` on 18.6 — and then draws the line at *who put the entry there*.
    An entry out of `acldefault`, and an entry whose grantee is the object's
    owner, are the zero point; `PUBLIC` is context (§5); everything else is a
    grant, compared for a managed role.

    Nothing is dropped. A revocation on this engine is the **absence** of an
    entry rather than a row — measured, `REVOKE EXECUTE … FROM PUBLIC` leaves
    `{postgres=X/postgres}` — so the pull reports both halves as context: the
    routines `PUBLIC` can execute, and the routines it can no longer execute,
    which is the one act that leaves no trace to list.

<a id="decision-372"></a>

372. **`GRANT … ON ROUTINE` is the only word that covers what this model calls
    a routine, and which word a bare object target takes is read off the
    permissions.** Measured on 18.6: `GRANT EXECUTE ON FUNCTION gr.p(integer)`
    on a *procedure* is `gr.p(integer) is not a function`, while `ON ROUTINE`
    takes a function and a procedure alike; `ON TABLE` takes a table and a
    view.

    `Change::Grant` carries no schema, so the emitter cannot look the target's
    kind up. It reads the permission set instead — a set containing `execute`
    is a routine's, one without is a table's — and `validate_role` is what
    makes that sound: measured word by word against kind, no kind on this
    engine takes `EXECUTE` and any of the table words, so a declaration that
    mixed them is refused before a plan exists.

<a id="decision-373"></a>

373. **A grant's routine signature is spelled with `unnest(proargtypes)`, not
    with `pg_get_function_identity_arguments`.** The obvious call is the wrong
    one: measured, it renders a procedure's argument as `IN integer`, mode and
    all, while the module pull renders the same routine's identity as
    `integer`. The managed-set filter compares a `GrantTarget::Routine`
    against a `ModuleId::Routine`, so two spellings of one signature would
    have every grant on a procedure read as a grant on an object the
    declarations do not have — and be revoked by the next plan.

<a id="decision-376"></a>

376. **Every catalog that holds an `aclitem[]` is read, and the two kind
    alphabets are kept apart by construction.** Two holes, one shape, both
    found by sweeping the read this step had just written.

    The relation arm filtered `relkind IN ('r','v','S')` — the kinds the model
    declares. Measured, `GRANT SELECT` on a **materialized view** and on a
    **partitioned table** both land in `relacl`, so the filter reported a role
    as holding nothing on either: *absent* reading as *empty*, which is the
    member of that set that reads as good news. The filter is now "not an index
    and not a TOAST table" — the two that take no `GRANT` at all — and every
    other kind is read and reported.

    Widening it exposed the second hole. `pg_class.relkind` and
    `pg_proc.prokind` overlap: `f` is a foreign table in one alphabet and a
    function in the other, `p` a partitioned table and a procedure. Carried as
    one `char`, a grant on a foreign table would have been read back as a grant
    on a *function* of that name — a target the declarations may well have, and
    therefore one the next plan would compare and revoke. `RawGrant::kind` is a
    `GrantedKind` naming the catalog as well as the letter, so the two cannot
    be read as one.

    And the object catalogs are not the only ones. Enumerated from the engine
    rather than from memory — the rule `crate::modules::ATTACHED_BY_ADDRESS`
    already earned here — PostgreSQL 18 has **fourteen** `aclitem[]` columns in
    `pg_catalog`. Three carry a target a declaration can name; eight more carry
    a real grant on a target it cannot (a type, a language, a foreign server, a
    configuration parameter, a large object, this database, a column), and each
    is reported per role, class and permission rather than dropped: a role that
    gained `USAGE ON LANGUAGE c` out of band has changed, and a reader that
    never looked would compare the grants it did see and call it clean
    (DECISIONS 105). Three are deliberately not read as grants and say why —
    `pg_default_acl` is a standing instruction rather than a grant,
    `pg_init_privs` records what an extension's objects had at *install*, and
    `pg_tablespace` is a cluster object whose question is `pg_shdepend`'s. A
    live test runs the enumerating query and compares it with that list, so a
    fifteenth column in a later release fails there instead of going unnoticed.

<a id="decision-377"></a>

377. **A rename is elided only where the *old* name is gone from the cluster.**
    370 has the differ build no `RenameRole` on a dialect that does not own the
    principal, on the ground that an ACL entry holds the role's oid and every
    grant followed the rename. That is true of a rename; it is not true of a
    name.

    If both names exist, `to` is a different principal. The plan then emits
    nothing — the two grant sets compare equal — while `from` goes on holding
    everything pbps was managing and `to` holds none of it, and the apply
    records `to` as holding it all. A wrong recording with a single deployer,
    which is the shape the finding rules always fix.

    So the elision has a precondition, and it is not "the new name exists":
    `pbps_pg::roles::rename_evidence` reads all four states and only
    `Done` — old gone, new present — lets the rename pass. `BothPresent`,
    `NotRunYet` and `NeitherPresent` each refuse. Only `NotRunYet` names the
    `ALTER ROLE … RENAME TO …` to run; the other states require the operator
    to resolve the name collision or missing principal before planning again.

    The remaining ambiguity is stated rather than hidden: `Done` cannot tell a
    rename from a drop-and-create. It does not have to. If the old role was
    dropped, its grants went with it, the pull shows the new role holding
    nothing, and every declared grant is planned here anyway — so the plan is
    right either way. Proving identity outright needs the role's oid in the
    recorded state, which is a format change and not this step's.

<a id="decision-378"></a>

378. **A routine's arguments travel as rows, never as a rendered signature.**
    A signature aggregated into one string has to be split again to be used,
    and the separator is not a separator. **Measured**: a type named
    `amount,type` renders as `cm."amount,type"`, so
    `pg_get_function_identity_arguments` and any `string_agg` of `format_type`
    both hand back a comma that belongs *inside* an argument.

    Split on it, every fragment failed `RoutineArg`, and a valid grant on a
    managed routine became targetless unexpressible state — which refuses the
    connected plan, so a legal declaration could not be applied at all. The
    grants query therefore returns the `pg_proc` oid and the arguments come
    from their own query, one row per argument in order, the shape
    `module_args_query` already uses. Nothing re-parses one string into a list,
    which is what makes the whole class unrepresentable rather than handled.

<a id="decision-379"></a>

379. **On PostgreSQL a bare grant target is read in the namespace its
    permissions name, not in the relations first.** Relations and routines are
    two catalogs on this engine and one name may be in both: **measured on
    18.6**, a table `co.f` and a function `co.f(integer)` coexist, `GRANT
    SELECT ON TABLE co.f` lands in `pg_class.relacl` and `GRANT EXECUTE ON
    ROUTINE co.f` lands in `pg_proc.proacl`.

    `emit::securable` already read the class off the permission set — a set
    with `execute` in it is a routine's — and `validate::target_kind` answered
    "a table" whenever a table of that name existed. The two disagreed exactly
    where the engine allows both, and the offline check refused a grant the
    engine runs. They now read the same fact the same way; a mixed set is
    still refused, because the kind check follows the namespace the
    permissions chose (372).

<a id="decision-380"></a>

380. **A grant is folded into a role only when the pull recorded the object it
    is on, and only when the target survives being written out.** The assembly
    before `add_roles` leaves objects out — a `bit(3)` column is a spelling
    read back as a different type (issue #130), a routine argument may be one
    `RoutineArg` cannot hold — and their ACL rows arrive all the same.
    Recorded, the role names a target the project does not declare and
    `pbps_model::role::check` refuses the very schema `pull` just wrote.

    The second half is 205's shape on this engine: `app."sales(archive)"` is a
    legal table name this dialect writes back unchanged, and its grant target
    parses back as the routine `app.sales(archive)` — a different object. Both
    are reported as unexpressible with the structured target kept, so the
    managed-set cut still applies to them; dropped instead, `pull` would write
    a role narrower than the database holds and the next plan would revoke what
    nobody removed (105).

<a id="decision-381"></a>

381. **One spelling per engine for a routine grant, and it is the one the
    catalog gives back.** On PostgreSQL that is the signature, whatever the
    statement used: measured, `GRANT EXECUTE ON ROUTINE app.solo` runs where
    the name is not overloaded, and the pull reads it back out of `pg_proc` as
    `app.solo(integer)` — nothing remembers which spelling was granted. A
    declaration spelling it `app.solo` therefore differs from the database on
    every comparison, and `diff_roles` compares targets by key: each plan
    revokes the signature and grants the bare name again, for ever, and no
    apply converges.

    So `validate::role` refuses a bare `Object` target that names a routine —
    not only the overloaded one 372 refuses for the engine's own `routine name
    "app.f" is not unique` — with the signature to write instead. It is the
    mirror of the refusal on the other engine, where nothing overloads and a
    signature is the spelling *its* catalog cannot produce
    (`pbps_mssql::validate::role`). Normalizing the two spellings instead would
    have had to be repeated at every comparison site — the differ, the
    post-apply verification, the managed-set cut — and a fold nobody repeats is
    the drift that returns.

    Which namespace the bare name is in is still 379's question: `select` on a
    name that is a table and a routine is the table's and is accepted, and only
    a set that chose the routine namespace reaches the refusal.

<a id="decision-382"></a>

382. **The pull's own existence check reads the target's namespace too.** 380
    accepted any module answering to an `Object` target's name, and a routine
    is not a relation here (379). A hidden table `app.f` beside a surviving
    routine `app.f(integer)` therefore had its `SELECT` folded into the role
    against the routine — and `validate::role` reads that bare relation name in
    the relation namespace, finds nothing, and refuses the schema `pull` had
    just written. An `Object` comes from a relation row and nothing else, so it
    is answered for by a recorded table or view alone.

<a id="decision-383"></a>

383. **The `public` schema is reachable without a grant on it, so §1 does not
    apply there.** `initdb` grants `USAGE` on `public` to PUBLIC in every
    database it makes: measured on 18.6, its `nspacl` is
    `{pg_database_owner=UC/pg_database_owner,=U/pg_database_owner}` — the
    second entry is PUBLIC's, and it is not `acldefault`'s doing
    (`acldefault('n', ...)` is `{owner=UC/owner}` alone). A role holding only
    `SELECT` on `public.pubt` reads it, measured.

    PUBLIC is not a role a project can declare (ADR-0010 §5), so no
    `schema::public: [usage]` line could appear in a pull — and requiring one
    refused every project whose tables live where PostgreSQL puts them,
    including the one `pull` writes from such a database. A DBA who revokes
    that `USAGE` makes the check silent where it would have had something to
    say; that limit is already the check's, because `USAGE` also arrives
    through a membership, which is never declared, compared or touched
    (ADR-0005). What it catches is the ordinary mistake — a project's own
    schema with no `usage` line.

<a id="decision-384"></a>

384. **The managed-set cut reads a grant target's namespace too.** `scope`
    kept a grant on `Object(app.f)` because *some* managed module answered to
    the name `app.f`, and on this engine a routine `app.f(integer)` is not the
    table `app.f` (379). A project managing the routine therefore had a grant
    on an unmanaged table compared, and the differ built a `REVOKE ... ON TABLE
    app.f` against an object outside the ids file — the one thing this cut
    exists to prevent.

    The filter is the id's own shape and needs no dialect: an id carrying a
    signature is a routine on an engine that overloads, and an engine that
    overloads cannot be keeping those objects where a relation's name is
    unique. On SQL Server every kind shares `sys.objects`, a routine grant
    arrives as `Object(dbo.f)`, and its ids are `Named` — a declared signature
    is refused there — so nothing is filtered out. `unexpressible_permissions`
    asks the same question of the limitation beside the grant and gets the same
    answer (176).

<a id="decision-419"></a>

419. **A completed cluster role rename changes the connected baseline's names,
    before its drift gate.** 377's four-state evidence now runs through the
    CLI seam. The old and new names are paired only by the reviewed role UID;
    only old-gone/new-present passes. The planner scopes the catalog under
    those new names and the checkout-free apply repeats that projection from
    the ledger and the plan's pinned ids. Neither alters a cluster role.

    Done cannot distinguish rename from drop-and-create, as 377 records. For
    these roles the planner compares the actual grants with the declarations
    and plans what is missing; it still refuses unexpressible grants and drift
    on every other object. The actual grants participate in the saved baseline
    checksum, so changes after planning still refuse at apply. Evidence is
    checked again inside the transactional apply, before execution and before
    recording. The staged baseline and its checkpoints use the same names.

    A pure rename emits no SQL, but is not an empty deployment: its identity
    mapping must reach the ledger or every later verify reports the old name
    missing. An empty PostgreSQL plan carrying roles therefore connects and
    checks under the deployment lock; it records only if the mapping moved.
    Other empty plans keep their connection-free path. No plan or ledger format
    change is needed: the approved plan already carries the final role UIDs.

<a id="decision-421"></a>

421. **An identity-only PostgreSQL deployment includes role additions and
    removals, not only renames.** 419's rename-only condition left an added
    grantless role outside the ledger forever: apply called it empty, and
    verify never watched the newly declared role. Removing the last such role
    has the same problem with an empty final role map. Every empty PostgreSQL
    plan therefore checks the role mapping under the deployment lock, and
    records a read-back when it changed. SQL Server retains its empty fast path.

    A newly managed role's actual grants participate in both the connected
    diff and its pinned baseline, even before the UID is in the ledger. Missing
    roles refuse, extra grants can be planned away, and a grant changed after
    planning invalidates the checksum. The drift gate still compares all
    previously managed objects. At the read-back a removed cluster role is
    outside the managed set, as its declaration requests; it is not dropped
    from the cluster. Staged checkpoints adopt the same final role scope.

<a id="decision-429"></a>

429. **Permission support is checked on the server that will execute the plan.**
    PostgreSQL's MAINTAIN grant arrived in 17. The connected engine facade asks
    `roles::unsupported_permissions` about the typed plan's Grant changes,
    before `plan --db` writes an artifact and before bootstrap or apply takes
    the ledger lock. This also covers staged execution and resume: a saved
    plan can move from a newer server to an older one, and a permission that
    fails after preceding DDL is not an acceptable preflight. Revoke-only and
    unrelated plans are not refused for permissions they do not grant.
    SQL Server explicitly reports this PostgreSQL version check as inapplicable;
    its own dialect still validates its permission vocabulary.

    Connected `plan --format json` now reports the same count summary as the
    offline command, plus named connected checks and adoption/policy findings.
    This supersedes decision 47's refusal of that flag: operational answers
    need a machine-readable surface, but the summary never duplicates the typed
    ChangeSet. `--out` remains the only saved artifact accepted by apply; the
    report is not stored in the plan or included in its risk or checksum.
    The two-version CLI regression pins rejection before writes and successful
    ordinary/revoke-only execution on 16 and MAINTAIN execution on 18 (#305, #321).

<a id="decision-517"></a>

517. **A routine arrives closed to `PUBLIC`, and a declaration is what opens it
     again (issue #318).**

     On PostgreSQL every function is created with `EXECUTE` to `PUBLIC`: the
     catalog holds no ACL at all and `acldefault('f', owner)` supplies one
     (ADR-0010 §5). For a `SECURITY DEFINER` routine that reads "any principal
     that can reach this schema may act as the owner", and pbps was producing
     exactly that state with its own `CREATE`. So a plan that creates a
     procedure or a function now also carries the change that takes the default
     away, and a declaration saying `public_execute: true` is what leaves it
     standing.

     **Every routine, not only the definer ones**, and the reason is the one
     that rules out the narrower fix rather than a preference for the wider
     one: which routines are `SECURITY DEFINER` is inside a body this tool
     never parses (SPEC §8.2), and the textual scan that would answer it is
     fooled by the words appearing in a comment or a string. A security
     control resting on a comparison that can be fooled is worse than one whose
     scope is stated plainly. The cost is stated too: an ordinary invoker
     routine that today relies on the default stops being callable by everyone
     at its next apply, and the one line that says otherwise goes through the
     merge request and into the plan's checksum like every other declaration.

     **A change of its own, not a `Revoke` from a role called `PUBLIC`.**
     ADR-0010 §5 named the gap as "a grantee the model does not have"; this is
     that grantee, given the narrowest shape that expresses the act —
     `Change::PublicExecution` carries a `RoutineId`, the decision, and which
     of the two acts brought the routine into being, and nothing else. A
     `String` holding the word would be a sentinel every reader had to know
     about, and a project may declare a role named `PUBLIC`.

     **Both decisions are written down, and both are written out.** The
     opt-in — `PublicAccess::Kept` — could have emitted nothing, on the
     reasoning that the `CREATE` has already left the default standing. That
     reasoning is false on a cluster whose deployment role has run
     `ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON ROUTINES FROM PUBLIC`: the
     `CREATE` then writes an explicit `proacl` that `PUBLIC` is not in
     (measured on 18.6), so a silent opt-in would apply a declaration and
     leave the declared state unreached — and 371 keeps what `PUBLIC` holds
     out of every comparison, so `verify` could not say so either. It emits
     `GRANT EXECUTE … TO PUBLIC`, which is exactly idempotent where nobody
     has tampered: on a routine whose `proacl` is still `NULL` the grant
     writes precisely `acldefault('f', owner)` — measured equal — so the
     rebuild guard, which compares against `acldefault`, sees no difference
     either. 371 is about what is *compared*, and the annotation is still
     never compared.

     Recording it matters separately from writing it. A plan that stayed
     silent about an opted-in routine could not be told apart from a plan that
     had no opinion about it, and the rebuild guard below needs exactly that
     distinction: a routine already closed by hand, whose declaration now asks
     for the default back and whose definition also changed, would otherwise
     have its valid rebuild refused.

     **The opt-in travels beside the model**, with `strategy:` and
     `depends_on:`, because 371 keeps what `PUBLIC` holds out of every
     comparison: a field inside `Module` would make a declared routine stop
     matching the identical routine read back from the catalog, which is
     inviolable constraint 1. What it says is therefore what the plan should
     *write*, never what the two sides should agree on — so adding or removing
     the key is not itself a change, and takes effect the next time the plan
     creates or rebuilds the routine.

     **A fresh create carries no risk class and a rebuild carries `revoke`.**
     Nobody held `EXECUTE` on an object that did not exist a statement earlier,
     so demanding `--allow revoke` in front of every plan that declares a
     function would be the friction-without-safety trade `GrantWiden` already
     refuses. A rebuild is the other case: on this engine every module edit is
     a drop and a create (ADR-0009 §3), the `CREATE` restores the default, and
     whether anybody was relying on it is not something the declarations can
     say — so the gate is asked. `RoutineOrigin` is what separates the two, an
     enum rather than a flag because telling them apart is its whole job, and
     `PublicAccess` is the second one for the same reason. Keeping the default
     is `GrantWiden` on either origin — labelled in the report, never gated,
     exactly as a widening grant is.

     **It also closes the deadlock 306 left standing.** A routine somebody had
     closed by hand used to refuse every later plan, because the rebuild would
     restore the default and nothing in the model could take it away again —
     hardening a managed routine and managing it were mutually exclusive. The
     rebuild guard now accepts exactly one missing default: `PUBLIC`'s
     `EXECUTE`, on a routine this plan has decided about — either decision,
     since both say the plan knows what the `CREATE` will leave behind. A
     different permission, or a different grantee, is still the refusal.

     **A staged run is refused while the plan closes one.** The `CREATE` and
     the revoke are two statements, and `--staged` commits each on its own —
     so between them the routine is committed, visible to the whole cluster
     and holding the default. The revoke sorts with the grants, after every
     row the plan writes, so that window is the rest of the plan rather than
     an instant. The same shape as the rebuild rule already in
     `require_transactional_rebuilds`, and refused on either driver: what
     makes it unsafe is the staging, not the engine. The opt-in's own window
     runs the other way — between its `CREATE` and its `GRANT` the routine is
     *less* reachable than the declaration asks for, a permission error rather
     than somebody else's privileges — so staging it is merely slow, and it is
     allowed.

     **It is counted with the routine it belongs to.** `--staged` applies one
     logical change (ADR-0003), and the differ appends this decision to every
     routine a plan creates or rebuilds — so counting plan entries would make
     a single routine two changes and refuse a staged creation that was legal
     before the decision existed. Both guards that enforce the rule, the one
     in the planner and the one that re-reads the saved artifact, count
     through the same helper: a count that differed between them would refuse
     the very file the planner had just written. A decision naming a routine
     the plan does not build is counted on its own, having nothing to be a
     companion of.

     **The origin is checked against the plan, not believed.** Re-deriving a
     saved plan's risks is what stops an edited `risks: []` from turning a
     destructive change into an ungated one, and it works because the
     derivation reads the typed change rather than the file's claim about it.
     `RoutineOrigin` is the first field that is *both*: the derivation reads
     it, so an artifact saying `created` over a rebuild derives no risk,
     agrees with itself, and still emits the revoke against a routine
     somebody was using. The plan already says which it is — an `AlterModule`
     for that routine, or a `DropModule` before its `CreateModule`, which is
     the shape a changed kind takes — so `validate_saved_plan` derives the
     origin the same way and refuses a decision the rest of the plan does not
     bear out. A decision with no companion at all is refused rather than
     guessed at: the differ never writes one.

     **And the plan is asked for a decision on every routine it builds.**
     Deleting the entry is the cheaper edit and leaves nothing inconsistent
     behind — the `CreateModule` derives the risks it always did — so every
     other check passes and the routine arrives holding the default, which
     371 keeps `verify` from reporting. An absence is not evidence of a
     decision, and on an engine whose `CREATE` hands a routine to `PUBLIC`
     there is no such thing as a routine the plan has no opinion about, so
     the absence is the finding. Asked of the dialect both times: only such
     an engine has a decision to make, and only one that rebuilds modules
     makes an `AlterModule` another `CREATE`.

     **The saved-plan version moves.** A version 8 plan is not broken, which
     is exactly the trouble: it was written before the grantee was anybody's
     decision, so it creates a routine and says nothing, and every check the
     new build runs on it passes. Applying it would leave open what this
     entry exists to close, and say nothing about that either. The version
     turns it away as a stale format instead, and the remedy is the one a
     stale artifact always had — plan again, and take the new plan through
     the gate.

     **The closing read holds the routine to it.** SPEC §7.6 does not promise
     a checkpoint catches a concurrent change to the field the plan is itself
     changing — the field is expected to move there, and the checkpoint cannot
     tell the plan's statement from the other session's — but it does promise
     the closing read catches it. A staged run commits the `CREATE` and the
     decision's own statement separately, so another session can reverse what
     the plan just wrote; and 371 keeps what `PUBLIC` holds out of every
     `Schema`, so the movement comparison had nowhere to see it. The decision
     is therefore checked against the read's own `public_execute` context, a
     postcondition of its own carried beside the comparison exactly as a
     `WITH GRANT OPTION` is (95). A resume does not mend it and does not
     pretend to: every statement has run, so what the read reports is a fact
     about the database and not a retryable hiccup.

     The same postcondition runs at the read `bootstrap` records, where no
     second session is needed at all: `ddl_command_end` fires on `GRANT` and
     on `REVOKE` (measured), so a trigger already in the database reverses
     what the build just settled inside the deployer's own transaction. The
     comment beside that read has said since 110 and 147 that a trigger can
     *add* a privilege there and a snapshot must never silently omit it; it
     can take one back too.

     **What a pull does with it.** The routines the database lets `PUBLIC`
     execute ride beside the pulled schema and are written into those
     declarations as `public_execute: true`. Still not a grant and still not
     compared — but a pull that recorded nothing would hand back a project
     whose first apply closes a routine the database has open.

<a id="decision-518"></a>

518. **A `REVOKE` carries one privilege, and revocability is decided per
     privilege.** 483 read "multiple original grantors on one grantee and
     target" as a single gap. Measured on PostgreSQL 18.6, it is two cases and
     only one of them is a gap. Two grantors on **one** privilege defeat any
     single statement: with `reader=ar/owner,reader=r/deploy`, the deployer's
     `REVOKE SELECT` removed its own entry and left the owner's, so the reader
     still held `SELECT`. Two grantors on **two** privileges do not: over
     `reader=a/owner,reader=r/deploy`, the same `REVOKE SELECT` took the
     deployer's `SELECT` away and left the owner's `INSERT` standing, which is
     exactly the narrowing a declaration that keeps `INSERT` asks for. Reading
     the rule per target refused that plan and refused `baseline` before it.

     So the catalog's revocability question is asked per grantee, target **and
     privilege** (`catalog::revocable_by_current_role`, shared with the doctor
     so the diagnosis and the read that refuses a plan cannot disagree).

     What makes that answer true of the statements this tool runs is the
     second half: the PostgreSQL emitter revokes **one privilege per
     statement**, where `GRANT` beside it still names the whole set. The
     engine selects the grantor once for the whole `REVOKE`, so a deployer
     inheriting both `ga` and `gb` running `REVOKE SELECT, INSERT` over
     `reader=r/ga,reader=a/gb` removed only the `SELECT` and warned `not all
     privileges could be revoked`; the same two privileges revoked one
     statement each removed both (measured). Worse than the warning, the
     apply's read-back did not refuse that half-done run — it recorded the
     narrowing as converged while the `INSERT` was still standing. A wider
     statement would therefore have the pull promise per privilege what the
     emitted statement settles per grantor, so the width is what goes rather
     than the promise (#251).
