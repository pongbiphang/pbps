//! `plan --db`: planning against a connected database.

use super::*;
use crate::engine::PlanSession as _;

/// Advisory only: a reused name supplies no identity intent (SPEC 14.3).
pub(crate) fn type_replacement_warnings(
    base: &IdsFile,
    desired: &IdsFile,
    changes: &pbps_model::ChangeSet,
    json: bool,
) -> Vec<crate::output::Finding> {
    let resolver = TypeResolver::new(base, desired);
    changes.changes.iter().filter_map(|step| {
        let pbps_model::Change::DropType { uid: old, name, .. } = &step.change else { return None };
        let TypeIdentity::Declared(new) = resolver.resolve(TypeSide::Desired, name) else { return None };
        if old == new || !changes.changes.iter().any(|s| matches!(&s.change, pbps_model::Change::CreateType { uid, .. } if uid == new)) {
            return None;
        }
        let message = format!(
            "Type `{}` is replaced: old uid `{old}` and new uid `{new}` are different types to pbps, so the old uid's history does not carry over to the new one. To change the type while keeping its uid, keep the original declaration file and its uid and edit the definition there; rename with `renamed_from`. Enum labels (#1635 slice 3) and a domain's NOT NULL, default and checks (slice 4a) change in place under the same uid; a domain's base type or collation is rebuilt under its uid (slice 4b). This is a suggestion only; pbps never infers identity from the name.",
            name.shown(),
        );
        if !json { eprintln!("warning: {message}"); }
        Some(crate::output::Finding::warning("plan.type-replacement", message))
    }).collect()
}

/// A standing `NOT VALID` check of a declared domain compares equal to its
/// declaration, as the model holds no validation (DECISIONS 251, DEC-1635.4);
/// the plan says so and names the statement that validates it (DEC-1635.8).
pub(super) async fn domain_check_not_valid_warnings<D: crate::engine::PlanSession>(
    dialect: &D,
    conn: &mut Conn,
    recorded: &Schema,
    json: bool,
) -> anyhow::Result<Vec<crate::output::Finding>> {
    if !recorded
        .types
        .values()
        .any(|t| matches!(t, pbps_model::ManagedType::Domain { .. }))
    {
        return Ok(Vec::new());
    }
    let standing = dialect.standing_not_valid_domain_checks(conn).await?;
    Ok(standing
        .into_iter()
        .filter(|found| recorded.types.contains_key(&found.domain))
        .map(|found| {
            let message = format!(
                "Check `{}` of domain `{}` is NOT VALID: the engine has not confirmed the rows that were there when it was added, and pbps compares it equal to its declaration because a declaration holds no validation.",
                pbps_model::escape::Escaped(&found.check),
                found.domain.shown(),
            );
            if !json {
                eprintln!("warning: {message}\n  Run `{}` by hand.", found.remedy);
            }
            crate::output::Finding::warning("plan.domain-check-not-valid", message)
                .remedy(format!("run `{}`", found.remedy))
        })
        .collect())
}

/// `pbps plan --db` — the applyable plan, computed against the target
/// environment as queried (SPEC §7.3).
///
/// This is the deployment layer, so it changes no files. The identity file was
/// settled when the MR was reviewed; a `plan --db` that quietly rewrote it
/// would mean the artifact the gate approves was computed against a mapping
/// nobody read.
///
/// `carried` receives the warnings the plan had gathered when it failed, in
/// JSON mode, for `main` to put beside the failure in the one envelope; human
/// mode printed them when they were found (#1703, DEC-1726.1).
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_db(
    project: &Project,
    target: &Target,
    declarations: services::Declarations,
    dialect: crate::emission::SelectedDialect,
    script: bool,
    staged: bool,
    json: bool,
    resolver_selection: Option<pbps_config::resolver::ResolverSelection>,
    no_rehearse: bool,
    assertions: Option<&crate::plan_assert::Selection>,
    carried: &mut Vec<crate::output::Finding>,
) -> anyhow::Result<services::PlanResult> {
    db::runtime()?.block_on(plan_db_selected(
        project,
        target,
        declarations,
        dialect,
        script,
        staged,
        json,
        resolver_selection,
        no_rehearse,
        assertions,
        carried,
        None,
    ))
}

/// A plan with no target assertions: the scratch test runner plans this way
/// and runs its own assertions on its own database. Not an `async fn`: an
/// extra async layer would hold the whole planning future twice, which
/// overflows a test thread's stack in a debug build.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_db_async<'a>(
    project: &'a Project,
    target: &'a Target,
    declarations: services::Declarations,
    dialect: crate::emission::SelectedDialect,
    script: bool,
    staged: bool,
    json: bool,
    resolver_selection: Option<pbps_config::resolver::ResolverSelection>,
    no_rehearse: bool,
    carried: &'a mut Vec<crate::output::Finding>,
    owned: Option<&'a services::Progress>,
) -> impl std::future::Future<Output = anyhow::Result<services::PlanResult>> + 'a {
    plan_db_selected(
        project,
        target,
        declarations,
        dialect,
        script,
        staged,
        json,
        resolver_selection,
        no_rehearse,
        None,
        carried,
        owned,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn plan_db_selected(
    project: &Project,
    target: &Target,
    declarations: services::Declarations,
    dialect: crate::emission::SelectedDialect,
    script: bool,
    staged: bool,
    json: bool,
    resolver_selection: Option<pbps_config::resolver::ResolverSelection>,
    no_rehearse: bool,
    assertions: Option<&crate::plan_assert::Selection>,
    carried: &mut Vec<crate::output::Finding>,
    // An owned test run lends the collector its caller drains after the phase:
    // a cancelled or expired phase drops this future, and a collector local to
    // it would take the observed findings along (#2694). `None` is the command.
    owned: Option<&services::Progress>,
) -> anyhow::Result<services::PlanResult> {
    let quiet = owned.is_some();
    pbps_cli::with_plan_dialect!(dialect, |mut dialect| {
        let services::Declarations { mut loaded, ids } = declarations;
        refuse_invalid_declarations(&loaded, dialect.as_ref(), json)?;
        let created_at = crate::now();
        // Keep the runtime alive for this connection across identity resolution.
        // Target provenance must be checked before a stale local mapping can
        // obscure the deletions it would plan (DEC-2397.1).
        let checked_target = async {
            let mut conn = db::connect(target).await?;
            let Some(entry) = crate::engine::latest(&mut conn).await? else {
                bail!(
                    "`{}` has a ledger but no entries; there is nothing to plan against.\n\
                     Record one with `pbps baseline --reason ...`.",
                    pbps_model::escape::Escaped(&target.label)
                );
            };
            refuse_mid_deployment(&entry, &target.label)?;
            let revision = if !entry.snapshot.kind.establishes_schema() {
                crate::engine::deployed_revision(&mut conn, entry.id).await?
            } else {
                Some(pbps_db::ledger::StateRevision {
                    id: entry.id,
                    git_sha: entry.snapshot.git_sha.clone(),
                })
            };
            let removals =
                checkout::inventory(&entry.snapshot, &ids, &loaded.schema, &loaded.intents);
            let findings: Vec<_> =
                checkout::assess(project.root(), &target.label, revision.as_ref(), &removals)?
                    .into_iter()
                    .collect();
            Ok::<_, anyhow::Error>((conn, entry, findings, revision.and_then(|r| r.git_sha)))
        }
        .await;
        // A failed target read cannot produce a plan, but must not replace the
        // established local identity refusals with an environment failure
        // (SPEC 9.8). Retain the error until those checks finish; it is never
        // missing evidence that would permit planning to continue.
        let (checked_target, checkout_findings, retirement_revision) = match checked_target {
            Ok((conn, entry, findings, revision)) => (Ok((conn, entry)), findings, revision),
            Err(error) => (Err(error), Vec::new(), None),
        };
        if !json {
            eprint!("{}", crate::output::human(&checkout_findings));
        }
        if checkout_findings
            .iter()
            .any(|f| f.severity == crate::output::Severity::Error)
        {
            let report = crate::output::Report::plain("plan", checkout_findings);
            return Ok(services::PlanResult::Refused(report));
        }
        if json {
            carried.clone_from(&checkout_findings);
        }

        if let Ok((_, entry)) = &checked_target {
            let reversed = pbps_diff::rebuild::rebuilt_since(&ids, &entry.snapshot.ids);
            anyhow::ensure!(
                reversed.is_empty(),
                "this environment already applied an approved rewrite of {}; restore the approval before planning; neither a drop and create nor a reverse rebuild is planned",
                reversed
                    .iter()
                    .map(|t| t.shown().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        // Identity and the eventual diff use the same recorded snapshot: a
        // missing checkout UID supplies no drop reason (DEC-2414.1).
        let context = crate::context(project.root())?;
        let resolution = match checked_target.as_ref() {
            Ok((_, entry)) => pbps_diff::identity::resolve_with_baseline(
                &loaded.schema,
                &ids,
                &loaded.intents,
                loaded.intents.len(),
                pbps_diff::Side {
                    schema: &entry.snapshot.schema,
                    ids: &entry.snapshot.ids,
                },
                &context,
            ),
            // The target read's saved error still prevents a plan below.
            Err(_) => pbps_diff::resolve_with_annotations(
                &loaded.schema,
                &ids,
                &loaded.intents,
                loaded.intents.len(),
                &context,
            ),
        };
        let mut resolved = match resolution {
            Ok(r) => r,
            Err(blockers) => {
                if json {
                    return Ok(services::PlanResult::Refused(crate::output::Report::plain(
                        "plan",
                        checkout_findings
                            .iter()
                            .cloned()
                            .chain(crate::report::blocker_findings(&blockers))
                            .collect(),
                    )));
                }
                eprintln!("{}", crate::report::blockers(&blockers));
                bail!("some changes could not be decided automatically");
            }
        };
        // As `plan` prunes: a record whose type has moved on is stale
        // identity, and this plan must be computed against what was reviewed
        // (DEC-2331.1).
        pbps_diff::conversion::prune(&mut resolved.ids, &loaded.schema, dialect.as_ref());
        // Diagnose an unsupported package action before the generic stale-ids
        // gate: CREATE/DROP themselves change the tentative identity section.
        if let Ok((_, entry)) = &checked_target {
            let errors = pbps_diff::extension_differences(
                pbps_diff::Side {
                    schema: &entry.snapshot.schema,
                    ids: &entry.snapshot.ids,
                },
                pbps_diff::Side {
                    schema: &loaded.schema,
                    ids: &resolved.ids,
                },
                dialect.name(),
            );
            if !errors.is_empty() {
                bail!(
                    "{}",
                    errors
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
        // By value, not by layout: the declarations and identities come in as
        // memory (a test hop's revision is not on this project's disk), and the
        // plan embeds the identities it was made with. Whether the tree on disk
        // is what the mapping says is `plan --check`'s question (design §3.5).
        if resolved.ids != ids {
            let message = format!(
                "the identity file is out of date; run `pbps plan` locally and commit `{}`.\n\
             A deployment plan must be computed against the identity its reviewers read.",
                project.ids_dir().display()
            );
            // The same finding offline `plan --check` reports, so a pipeline
            // reads one id for one refusal whichever command met it (#573).
            if json {
                return Ok(services::PlanResult::Refused(crate::output::Report::plain(
                    "plan",
                    checkout_findings
                        .iter()
                        .cloned()
                        .chain([crate::output::Finding::error("identity.stale", message)
                            .at(project.ids_dir(), None)
                            .remedy("pbps plan")])
                        .collect(),
                )));
            }
            bail!("{message}");
        }

        let (mut conn, mut entry) = checked_target?;
        let mut dependencies = pbps_model::dependency::unexamined(&loaded.dependencies);
        let mut target_assertions = Vec::new();
        let connected = Box::pin(async {
        // First, before the resolver and rehearsal: a violation stops cheaply,
        // and a refused login never reaches the expensive steps (DEC-2401.4).
        // Run on fresh assertion connections; the planning session only names
        // the target they must match. `run` returns a boxed future so the
        // engines' large runner futures stay out of this block's frame.
        if let Some(selection) = assertions {
            match selection.run(&mut conn).await {
                Ok(passed) => target_assertions = passed,
                Err(crate::plan_assert::Stop::Refused(finding)) => {
                    return Ok(Connected::Refused { finding, findings: checkout_findings.clone() });
                }
                Err(crate::plan_assert::Stop::Unanswerable(finding)) => {
                    return Err(crate::plan_assert::Unanswerable(finding).into());
                }
            }
        }
        // The selected resolver entry's scratch, acquired at most once for the
        // resolver and the rehearsal both (#2379).
        let mut scratch = resolver_selection
            .as_ref()
            .map(|selection| crate::resolution::Entry::new(selection, project, target));
        dependencies = dialect.compare_dependencies(&mut conn, &loaded.dependencies).await?;
        // What the engine keeps for itself from the type prerequisites sealed
        // here to the misspelling probe below.
        let mut plan_state = Default::default();
        dialect
            .seal_declared_type_pins(
                &mut plan_state,
                &mut conn,
                &loaded.schema,
                &project.config.trusted_roles,
                project.config.pin_type_routines,
            )
            .await?;
        let mut findings = checkout_findings;

        let role_renames =
            dialect
                .external_role_renames(&mut conn, &entry.snapshot.ids, &resolved.ids)
                .await?
                .renames;
        let recorded_snapshot = entry.snapshot.clone();
        rename_snapshot_roles(&mut entry.snapshot, &role_renames);
        let recorded_ids = role_scope(&entry.snapshot.ids, &resolved.ids, dialect.as_ref());
        // The baseline's module scope is the **recorded** state's, never the
        // declarations': `apply` has only the plan file and the ledger, so a
        // scope that needed a checkout would make the two checksums disagree on
        // a host with none. A declared module that exists but was never
        // recorded is simply created again, which `CREATE OR ALTER` makes
        // harmless.
        let recorded_modules = managed_modules(Some(&entry.snapshot), None);
        // The policy is applied separately from the scope. The scope has to stay
        // the recorded set, or the two checksums below would be computed over
        // different object sets — but a module that this revision declares for
        // the first time and that already exists in the database is outside it,
        // and telling its author "this object is not declared" while planning
        // the `CREATE OR ALTER` for it would be both wrong and unactionable.
        //
        // The rows are read once, widely enough for both scopes that meet here
        // (ADR-0004): the recorded one, which the drift check below compares
        // against, and the declared one, which the differ measures the
        // declarations against — every row an `exact` declaration is about to
        // delete included.
        let recorded_data = entry.snapshot.schema.data_scopes();
        // Every declared text, as the engine reads it: a spelling it would
        // read back differently is refused before a plan is written that
        // could never converge (DECISIONS 101).
        for notice in crate::row_security::binding_notices(&loaded.schema) {
            if !json {
                eprintln!("warning: {notice}");
            }
            findings.push(crate::output::Finding::warning(
                "plan.policy-bindings-unchecked",
                notice,
            ));
        }
        let mut unasked = Vec::new();
        let probe_schema = pbps_diff::enum_labels::probe_schema(
            pbps_diff::Side { schema: &entry.snapshot.schema, ids: &recorded_ids },
            pbps_diff::Side { schema: &loaded.schema, ids: &resolved.ids },
            &loaded.hints,
        );
        let misspelt = dialect
            .misspelt(
                &plan_state,
                &mut conn,
                &probe_schema,
                &catalogued_as(&loaded.schema, &resolved.ids, &recorded_ids),
            )
            .await;
        let refused = refuse_misspelt_read(misspelt, &loaded.schema, &mut unasked);
        // In the envelope, not beside it: a warning-only result of a
        // read-only command stays in its one JSON report (SPEC §9.8; #1660).
        for w in unasked {
            if !json {
                eprintln!("warning: {}", w);
            }
            findings.push(crate::output::Finding::warning(
                "plan.partition-default-unasked",
                w,
            ));
        }
        // Printed already in human mode; in JSON they are carried to the
        // envelope of any failure from here on, the spelling refusal just
        // below included (#1702, #1703).
        if json {
            carried.clone_from(&findings);
        }
        // The declarations as the engine reads them, a date or time in its
        // own spelling (DEC-1883.1), from here to the plan's recorded scope.
        loaded.schema = refused?;
        // The declarations under the names the database has *now*: a table
        // this plan renames is read, scoped and measured under its old name,
        // and a declared scope keyed by the new one would find no table and
        // be skipped — an `ensure` -> `exact` switch in the same revision as
        // the rename then planned none of its deletes.
        let declared_live = tables_under(&loaded.schema, &resolved.ids, &recorded_ids);
        let declared_data = declared_live.data_scopes();
        let spelled = dialect
            .schema_spellings(&mut conn, &schemas_wanted(&loaded.schema))
            .await;
        refuse_wrongly_spelt(spelled, &loaded.schema)?;
        // A key the declarations renamed a label to is held by the row under
        // the label it had (DEC-2346.2).
        let mut to_read = pbps_model::data::read_scopes(&recorded_data, &declared_data);
        pbps_diff::enum_labels::read_before_the_renames(&mut to_read, &declared_live, &loaded.hints);
        let local_catalog = std::cell::RefCell::new(Vec::new());
        let catalog = owned.map_or(&local_catalog, |p| &p.findings);
        let managed = managed_state_full(
            &mut conn,
            &recorded_ids,
            &recorded_modules,
            // The recorded setting, as `verify` reads it: the live side is
            // what the ledger says pbps holds, and a declared switch is the
            // differ's to refuse (DEC-1533.1).
            &pbps_diff::external_parents(&entry.snapshot.schema),
            target.catalog_policy(pbps_config::Unmanaged::Ignore),
            &to_read,
            crate::engine::Read::Snapshot,
            CatalogWarnings::for_output(json, catalog),
        )
        .await;
        findings.extend(catalog.take());
        if json {
            carried.clone_from(&findings);
        }
        let managed = managed?;
        refuse_unexpressible(&managed.scoped, &target.label, "plan again")?;
        let mut advisories = permission_advisory_findings(&managed.permission_advisories, project);
        // Both report-only inventories must reach the same policy gate.
        let policies = project.config.policies();
        let rule = pbps_policy::rules::UNHELD_OBJECT;
        let today = pbps_policy::civil::at(crate::unix_seconds(), 0).date();
        if let Some(severity) = policies.effective(rule).severity
            && !policies.suppressed(rule, None, &today)
        {
            advisories.extend(managed.catalog_notices.iter().map(|message| {
                crate::policy_finding(&pbps_model::Finding::new(rule, severity, message.clone(), None))
            }));
        }
        if !json {
            eprint!("{}", crate::output::human(&advisories));
        }
        let refused_by_policy = advisories
            .iter()
            .any(|f| f.severity == crate::output::Severity::Error);
        findings.extend(advisories);
        if json {
            carried.clone_from(&findings);
        }
        if refused_by_policy {
            if json {
                if quiet { carried.clone_from(&findings); } else {
                    crate::output::Report::plain("plan", findings).emit_json()?;
                }
            }
            bail!(
                "a policy set to `error` refuses this plan; fix the declarations, or suppress \
                 the rule in pbps.yml with a reason"
            );
        }
        // A connected plan becomes an applyable artifact. If introspection
        // omitted a fact on a recorded table, its scoped checksum can still
        // match while the real database does not; planning from that partial
        // projection would bless the omission and an empty apply would never
        // reconnect to catch it. The same argument as the line above, about
        // the other half of what a projection can fail to hold.
        refuse_managed_limitations(&managed.limitations)?;
        // The declarations as this database reads them back: a collation
        // equal to its default is a column with none (#1175). Compared as
        // written, an unchanged column was altered to the collation it
        // already has on every plan, and a created one read back without
        // it failed the apply's own postcondition.
        let declared =
            declarations_as_compared(&loaded.schema, managed.database_collation.as_deref());
        dialect.refuse_unknown_collations(&mut conn, &declared).await?;
        // A key to a declared history under another spelling of its name,
        // which only the database's collation can join (#1625).
        dialect.refuse_history_references_alike(&mut conn, &declared).await?;
        // And now that the default is known, an absent collation facing a
        // named one on a foreign key is decidable (#1247 review).
        let mismatched =
            declared.foreign_key_collation_problems(managed.database_collation.is_some());
        if !mismatched.is_empty() {
            bail!(
                "the declarations have a foreign key the engine refuses (1757):\n  {}",
                mismatched.join("\n  ")
            );
        }
        // A declared name standing on an object introspection cannot express is
        // not something to plan around. It is absent from the scoped schema, so
        // the diff would emit an ungated `CreateModule` and `CREATE OR ALTER`
        // would replace it — removing, silently, the very options that made it
        // unreadable (ADR-0002: what pbps cannot reproduce, it does not touch).
        let colliding: Vec<&str> = managed
            .unreadable
            .iter()
            .filter(|(n, _)| loaded.schema.modules.keys().any(|id| n.matches_module(id)))
            .map(|(_, why)| why.as_str())
            .collect();
        if !colliding.is_empty() {
            bail!(
                "`{}` already holds {} declared object(s) that pbps cannot read back: {}.\n\
                 Planning would propose creating them, and `CREATE OR ALTER` would replace what \
                 is there. Remove the declaration, or recreate the object in a form pbps can \
                 express.",
                pbps_model::escape::Escaped(&target.label),
                colliding.len(),
                colliding.join(", ")
            );
        }
        let scoped = managed.scoped;
        let mut for_policy = scoped.clone();
        for_policy
            .unmanaged_modules
            .retain(|m| !loaded.schema.modules.contains_key(m));
        report_unmanaged(
            &for_policy,
            &managed.unreadable,
            &recorded_ids,
            &recorded_modules,
            project.config.unmanaged,
        )?;

        // The plan is computed against the environment *as queried*, so the
        // queried state had better be the recorded one. When it is not, the
        // plan would be pinned to a checksum that describes neither — and the
        // difference is drift, which has its own command and its own three
        // remedies. The rows are compared under the recorded scope, exactly as
        // `verify` compares them.
        let as_recorded = scoped.schema.clone().with_observed_rows(
            &managed.rows,
            &recorded_data,
            &entry.snapshot.schema,
        )?;
        let live = pbps_model::drift_checksum(&as_recorded, &recorded_ids);
        let mut expected = entry.snapshot.schema.clone();
        // Done also permits drop-and-create (377). Grants lost with the old
        // principal must be planned onto the new one, not rejected as drift.
        // Newly managed roles also start from their actual grants. Both sets
        // remain in the pinned baseline and the differ's input (421).
        let missing_roles = dialect.reconcile_cluster_roles(
            &recorded_snapshot.ids,
            &recorded_ids,
            &role_renames,
            &as_recorded,
            &mut expected,
        )?;
        // First declarations adopt the live definition, as newly managed
        // cluster roles do. Already recorded types still participate in drift.
        for (name, definition) in &as_recorded.types {
            if matches!(TypeResolver::new(&entry.snapshot.ids, &resolved.ids).resolve(TypeSide::Base, name), TypeIdentity::ExternalOrUnrecorded(_)) {
                expected.types.insert(name.clone(), definition.clone());
            }
        }
        let recorded = pbps_model::drift_checksum(&expected, &recorded_ids);
        if live != recorded {
            bail!(
                "`{}` has drifted from the state recorded at {} (entry #{}).\n\
                 Run `pbps verify --env/--db ...` to see how, and resolve it before planning.",
                pbps_model::escape::Escaped(&target.label),
                entry.applied_at,
                entry.id
            );
        }

        // A table whose rows the declarations cover for the first time is
        // measured against what it holds, not against nothing: the rows are
        // adopted, the ones that differ are updated, and — for `exact` — the
        // ones nobody declared are deleted behind the gate. The note names
        // the current row count beside the typed takeover below (DEC-1803.1).
        for (name, scope) in &declared_data {
            if recorded_data.contains_key(name) {
                continue;
            }
            let message = match managed.rows.get(name) {
                Some(observed) => format!(
                    "Reference data: the declarations take over the rows of {name} ({}); it holds \
                     {} row(s) now.",
                    scope.mode,
                    observed.rows.len(),
                    name = name.shown()
                ),
                None => format!(
                    "Reference data: {name} does not exist yet; its declared rows go in with it.",
                    name = name.shown()
                ),
            };
            if !json {
                println!("{}", message);
            }
            findings.push(crate::output::Finding::note("data.adoption", message));
        }
        // The comments the database holds that no declaration has a `description` for: left as
        // they are, and said once at the end of the plan (DEC-2729.1).
        if dialect.holds_descriptions() {
            let count = pbps_diff::undeclared_comments(&scoped.schema, &loaded.schema);
            if count > 0 {
                findings.push(crate::output::Finding::note(
                    "plan.undeclared-comments",
                    undeclared_comments_note(count),
                ));
            }
        }
        let read_back = pbps_model::data::plan_base(
            &scoped.schema,
            &managed.rows,
            &recorded_data,
            &declared_live,
        )?;
        // Observe the attaching identities under their current names before
        // the differ chooses their optional scan CHECK (DEC-2126.1). Its key
        // carries that choice through re-derivation, sealing, apply and resume.
        if staged {
            dialect = dialect.staged_attach_dialect(
                &mut conn,
                pbps_diff::Side { schema: &read_back, ids: &recorded_ids },
                pbps_diff::Side { schema: &declared, ids: &resolved.ids },
            ).await?;
        }
        // The engine respells every expression and module body it stores —
        // measured on SQL Server, a check declared `n > 0` reads back
        // `([n]>(0))` — so compared against the read-back an unchanged check
        // was dropped and re-added, and an unchanged filtered index rebuilt,
        // on every connected plan. The differ compares the declarations
        // against what was *declared* when each object was last written,
        // where the ledger recorded it (ADR-0009 §2.2, ADR-0013 §4,
        // DECISIONS 207–208).
        let mut base = entry.snapshot.declared.overlay(&read_back);
        // A read knows the sequence names it leaves out (#1631).
        pbps_dialect::name_read_sequences(&mut base, dialect.as_ref());
        // A removed module no longer has a declaration carrying its
        // `depends_on:` edge. The newest snapshot keeps those baseline
        // annotations so connected planning can still drop dependents first.
        let mut hints = loaded.hints.clone();
        hints.record_row_defaults(&entry.snapshot.schema);
        // The rehearsal rebuilds the base as the target holds it, so a kept
        // module is ordered by the annotation it was written under, not by
        // this revision's (see `rehearsal::connected::Plan::base_hints`).
        hints.annotations.record(entry.snapshot.object_deps.to_deps());
        // A rebuilt enum or domain keeps its comment, which `CREATE TYPE` does
        // not copy: it is a fact about the target, read before the differ
        // writes the swap that carries it (DEC-1635.7, DEC-1635.9). So is
        // which domains an array holds, which PostgreSQL refuses a new check
        // on in place and a rebuild takes instead.
        dialect
            .type_rebuild_hints(
                &mut conn,
                &base,
                &declared,
                &recorded_ids,
                &resolved.ids,
                &mut hints,
            )
            .await?;
        // SQL Server's computed columns that stand throughout are judged by
        // the catalog's edges below (`order_computed_by_edges`), under the
        // database's collation, not by the text screen (#1460).
        let screen = dialect.computed_column_screen();
        // plan_base uses the declared mode to compare newly managed rows.
        // Carry the takeover separately so matching rows still reach the
        // checksum/approval gate and the ordinary ledger write (DEC-1803.1).
        let takeovers: Vec<_> =
            pbps_model::data::takeovers(&recorded_data, &declared_data, &scoped.schema)
                .into_iter()
                .map(|(name, mode)| {
                    let change = pbps_model::Change::SetDataMode {
                        table: recorded_ids.resolved_in(&name, &resolved.ids),
                        from: None,
                        to: Some(mode),
                    };
                    let risks = dialect.change_risks(&change);
                    let mut planned = pbps_model::PlannedChange::new(change);
                    planned.risks = risks;
                    planned
                })
                .collect();
        // The union read is for row comparison, not evidence that a key was
        // already managed. Carry the recorded scope transition by identity
        // even when no DML is needed (DEC-1808.1).
        let ensure_keys: Vec<_> = pbps_model::data::ensure_key_changes(
            &recorded_data,
            &declared.data_scopes(),
            &recorded_ids,
            &resolved.ids,
        )
        .into_iter()
        .map(|change| {
            let risks = dialect.change_risks(&change);
            let mut planned = pbps_model::PlannedChange::new(change);
            planned.risks = risks;
            planned
        })
        .collect();
        // The zero-SQL changes the diff cannot see, carried by every plan this
        // read makes: the initial one, each re-derivation, and the resolver's
        // (#1516 review). One list, so a plan cannot carry one and not the
        // other.
        let zero_sql = pbps_diff::order::writebacks::ScopeInput::new(
            takeovers.into_iter().chain(ensure_keys).collect(),
        )
        .expect("connected scope inputs are data-mode and ensure-key transitions");
        // The read-back goes in beside the overlaid base, for the comparisons
        // the engine makes by parse tree (an attach's checks and generation
        // expressions, #1642 review).
        dialect
            .retained_expression_hints(
                scratch.as_mut(),
                &mut conn,
                target,
                pbps_diff::Side { schema: &base, ids: &recorded_ids },
                pbps_diff::Side { schema: &declared, ids: &resolved.ids },
                &read_back,
                &mut hints,
            )
            .await?;
        // The modules a rebuilt enum's columns carry, found in the planning
        // transaction and kept for every plan that follows it.
        let enum_dependents = std::cell::RefCell::new(std::collections::BTreeSet::new());
        let diff_here = |also: &std::collections::BTreeSet<pbps_model::ModuleId>| {
            let mut planning = hints.clone();
            planning.enum_dependents = enum_dependents.borrow().clone();
            pbps_diff::diff_read_back(
                pbps_diff::Side {
                    schema: &base,
                    ids: &recorded_ids,
                },
                pbps_diff::Side {
                    schema: &declared,
                    ids: &resolved.ids,
                },
                dialect.as_ref(),
                &planning,
                also,
                screen,
                &read_back,
            )
            .map(|changes| {
                // Re-derivations and resolver planning must retain the same
                // zero-SQL changes as the initial connected plan.
                zero_sql.append_to(changes, dialect.as_ref())
            })
        };
        let mut cs = diff_here(&std::collections::BTreeSet::new()).map_err(crate::Unexpressible)?;
        findings.extend(type_replacement_warnings(&recorded_ids, &resolved.ids, cs.changes(), json));
        findings.extend(domain_check_not_valid_warnings(&dialect, &mut conn, &base, json).await?);

        let needs_review = dialect
            .seal_plan_type_review(
                &mut conn,
                pbps_diff::Side { schema: &base, ids: &recorded_ids },
                pbps_diff::Side { schema: &declared, ids: &resolved.ids },
                &cs,
                &project.config.trusted_roles,
                project.config.pin_type_routines,
            )
            .await?;
        // DEC-1515.1's three cases, decided from this ordinary plan before any
        // pass asks the catalog about it: case 2 then opens nothing a
        // resolver owns, and case 3 hands the resolver the plan it re-derives.
        let base_side = pbps_diff::Side {
            schema: &base,
            ids: &recorded_ids,
        };
        let desired_side = pbps_diff::Side {
            schema: &declared,
            ids: &resolved.ids,
        };
        let mut resolver_assessment = None;
        // Case 3's answer, unsealed: the resolver's order goes through every
        // pass and check below, as an ordinary plan does, and is sealed in
        // the order they leave (#1516).
        let mut resolution = None;
        match crate::resolution::case(
            resolver_selection.as_ref(),
            base_side,
            desired_side,
            &cs,
            dialect.as_ref(),
        ) {
            crate::resolution::Case::Fallback => {}
            crate::resolution::Case::NotNeeded { assessment } => {
                resolver_assessment = Some(crate::resolution::ResolverAssessment::not_needed(
                    &assessment,
                ));
            }
            crate::resolution::Case::Required {
                selection,
                assessment,
            } => {
                if needs_review {
                    bail!("resolver execution would run unreviewed type routines; use a separate reviewed plan before resolver execution (DEC-2181.2)");
                }
                // A resolved plan is sealed transactional: its evidence
                // describes one atomic change (`SavedPlan::with_resolution`).
                // Refused before the cluster identity is read, so the gate
                // names itself to a role that may not read it.
                if staged {
                    let finding = crate::resolution::staged(selection, &assessment);
                    return Ok(Connected::Refused { finding, findings });
                }
                // The cluster this plan was read from: the resolver checks its
                // own target connection against it (#1685).
                let identity =
                    match crate::resolution::planning_identity(&mut conn, selection).await {
                        Ok(identity) => identity,
                        Err(crate::resolution::Refused::Finding(finding)) => {
                            return Ok(Connected::Refused { finding, findings });
                        }
                        Err(crate::resolution::Refused::Unanswerable(error)) => return Err(error),
                    };
                let request = crate::resolution::Request {
                    sources: Some(&loaded.sources),
                    dependencies: &loaded.dependencies,
                    selection,
                    assessment: &assessment,
                    target,
                    project,
                    base: base_side,
                    desired: desired_side,
                    hints: &hints,
                    target_identity: identity.as_deref(),
                };
                let asked = crate::resolution::named(&assessment);
                // Boxed like the retained comparison above: the resolver's
                // future is the largest in this block, and the bootstrap
                // prerequisites (DEC-2447.2) it now carries tipped the owned
                // lifecycle tests' native thread stacks over their limit.
                match Box::pin(crate::resolution::resolve(
                    &request,
                    scratch.as_mut().expect("a required resolution selected an entry"),
                )).await {
                    Ok(unsealed) => {
                        resolver_assessment =
                            Some(crate::resolution::ResolverAssessment::resolved(&assessment));
                        // The resolver's order, with the connected plan's
                        // zero-SQL changes after it, where `diff_here`
                        // keeps them.
                        cs = crate::resolution::Continuation::with_scope(
                            dialect.as_ref(), &zero_sql, &unsealed,
                        );
                        resolution = Some((selection.name.clone(), unsealed, asked));
                    }
                    Err(refused) => {
                        // The container the resolver used is confirmed gone
                        // before the refusal, as it was when the resolver
                        // owned it alone.
                        let released = scratch.as_mut().map(|entry| entry.finish());
                        if let Some(released) = released
                            && let Err(crate::resolution::Refused::Unanswerable(error)) =
                                released.await
                        {
                            return Err(error);
                        }
                        match refused {
                            crate::resolution::Refused::Finding(finding) => {
                                return Ok(Connected::Refused { finding, findings });
                            }
                            crate::resolution::Refused::Unanswerable(error) => return Err(error),
                        }
                    }
                }
            }
        }

        // Before anything is checked against the plan: a name it creates that
        // the database already uses, outside what this project records, is a
        // `CREATE` the engine will refuse (#316, #320, #323).
        dialect.refuse_occupied_relation_names(
            &cs,
            &scoped,
            &managed.unreadable,
            &managed.unmanaged_relations,
            &target.label,
        )?;
        dialect.require_transactional_rebuilds(&cs, staged)?;
        // The one admission rule of every site that runs code the plan holds
        // (DEC-2563.2). Review admits routines into it as the sites vet; the plan
        // shows and pins them when it seals.
        dialect.begin_site_review(&mut plan_state, crate::pins::admission(project));
        // What the positions inside the transaction and after it read of the
        // plan, besides its changes.
        let frame = crate::engine::Frame {
            label: &target.label,
            recorded: pbps_diff::Side { schema: &recorded_snapshot.schema, ids: &recorded_ids },
            written: pbps_diff::Side { schema: &loaded.schema, ids: &resolved.ids },
            base: pbps_diff::Side { schema: &base, ids: &recorded_ids },
            declared: pbps_diff::Side { schema: &declared, ids: &resolved.ids },
            hints: &hints,
            baseline: &entry.snapshot,
        };
        conn.begin(&dialect.transaction_framing(&pin_zone(&conn)))
            .await?;
        let checks = async {
            // In the read-only planning transaction, with everything below.
            dialect
                .check_names_against_catalog(&mut plan_state, &mut conn, &mut cs, frame, staged)
                .await?;
            let rename_evidence = dialect
                .external_role_renames(&mut conn, &recorded_snapshot.ids, &resolved.ids)
                .await?
                .check;
            // The plan carries what the resolver's run bound at creation, so
            // every pass that orders it again reads the same bindings. Only the
            // observer below judges the finished `ChangeSet`, which does not
            // (DEC-2143.8).
            #[cfg(debug_assertions)]
            let captured = resolution
                .as_ref()
                .map(|(_, unsealed, _)| unsealed.captured().clone())
                .unwrap_or_default();
            // First, so that what it adds is held to every check after it: a
            // view this plan now drops and recreates takes `before_a_rebuild`
            // like one the declarations edit (#314, ADR-0009 §4).
            let rediff = |also: &std::collections::BTreeSet<pbps_model::ModuleId>| {
                // The resolver rebuilds what its evidence shows reaching a
                // replaced input. A module it did not rebuild that still
                // depends on one the plan drops is outside that evidence, and
                // planning again without the resolver would drop its answer
                // (#1516).
                if resolution.is_some() {
                    let names: Vec<String> = also.iter().map(|id| id.shown().to_string()).collect();
                    bail!(
                        "the resolver's plan drops a module that {} depend(s) on, and its \
                         evidence does not cover rebuilding them; declare the dependency, or \
                         change the module in a plan of its own",
                        names.join(", ")
                    );
                }
                diff_here(also).map_err(|errors| crate::Unexpressible(errors).into())
            };
            // What stands on an enum this plan rebuilds is read first: the views,
            // bodies and triggers over its columns come down and go back through
            // the module rebuild below, and anything else is refused by name
            // (DEC-1635.7). The members of the roles it drops are read with it,
            // for the schedule to write into the plan.
            let reads = dialect
                .read_before_schedule(&mut conn, cs.changes(), &loaded.schema, &recorded_ids)
                .await?;
            let cs = if reads.enum_rebuild.is_empty() {
                cs
            } else {
                enum_dependents.borrow_mut().extend(reads.enum_rebuild);
                rediff(&std::collections::BTreeSet::new())?
            };
            let (mut cs, dependents) = dialect
                .schedule(
                    &mut plan_state,
                    &mut conn,
                    cs,
                    frame,
                    reads.role_members.as_ref(),
                    &rediff,
                )
                .await?;
            let ordered = dialect
                .order_by_catalog(&mut plan_state, &mut conn, &mut cs, frame)
                .await?;
            // Every pass that orders the plan has run: the verifier checks the
            // order that is saved and applied, in tests and debug builds
            // (#1954, DEC-1954.6), and the engine says when it already has.
            // Debug assertions alone, not `cfg(test)`: `pbps-diff` is compiled
            // without its own `cfg(test)` here, so a release test build has no
            // observer to call.
            #[cfg(debug_assertions)]
            if !ordered.verified { pbps_diff::order::shadow::observe_scheduled(
                pbps_diff::Side {
                    schema: &recorded_snapshot.schema,
                    ids: &recorded_ids,
                },
                pbps_diff::Side {
                    schema: &loaded.schema,
                    ids: &resolved.ids,
                },
                dialect.as_ref(),
                &hints.annotations.modules(&loaded.schema),
                &cs,
                pbps_diff::order::shadow::Stage::Final,
                Some(&ordered.facts.unwrap_or_default()),
                &captured,
            ); }
            // The observer's own gate: a release test build has `cfg(test)`
            // but no observer, so the facts are spent here too (#2089).
            #[cfg(not(debug_assertions))]
            let _ = (ordered.verified, ordered.facts);
            let checks = dialect
                .check_final_order(&plan_state, &mut conn, cs.changes(), frame)
                .await?;
            Ok::<_, anyhow::Error>((
                cs,
                rename_evidence,
                dependents,
                checks.rebuilds,
                checks.drop_blockers,
                checks.logical_slots,
                checks.numbered_column_fills,
            ))
        }
        .await;
        let rollback = conn
            .rollback(&dialect.transaction_framing(&pin_zone(&conn)))
            .await;
        let (mut cs, rename_evidence, dependents, rebuilds, drop_blockers, logical_slots, fills) =
            checks?;
        rollback?;
        let requirement_names = pbps_model::plan_names::PlanNames::of(&cs.changes().changes, &resolved.ids)?;
        // The expressions the plan introduces that run as the deployer are
        // vetted now, to learn which routines the review admits, so the plan
        // can show and pin them (DEC-2563.2); a refusal that waits for apply is
        // the engine's to say.
        dialect
            .vet_plan_sites(&plan_state, &mut conn, &requirement_names, &mut |note| {
                if !json {
                    eprintln!("warning: {note}");
                }
                findings.push(crate::output::Finding::warning(
                    "plan.table-expression-unbound",
                    &note,
                ));
            })
            .await?;
        // Rows that hold a label an enum rebuild removes, or that a domain's
        // narrower base cannot hold, are counted now, by the same probes the
        // apply runs, so the plan is refused by name and with a count before
        // anything is written (DEC-1635.7, DEC-1635.9).
        if let Some(result) = dialect
            .probe_stranded_rows(&plan_state, &mut conn, cs.changes(), frame, owned)
            .await?
        {
            if let Some(owned) = owned {
                findings.extend(owned.findings.take());
            }
            if json { carried.clone_from(&findings); }
            result?;
        }
        // Again, over what the dependents added: a rebuild the plan now
        // synthesizes needs the transaction as much as one it was given.
        dialect.require_transactional_rebuilds(&cs, staged)?;

        // The keys were matched to the rows under the type the key column has
        // now (71); a plan that changes that type would carry the mapping
        // into a type that does not make it (DECISIONS 108).
        let over_aliases = key_type_changes_over_aliases(
            &cs,
            &declared_live,
            &managed.rows,
            &resolved.ids,
            &recorded_ids,
        );
        if !over_aliases.is_empty() {
            bail!(
                "{}\n\
                 The declared keys were matched to the rows under the type the key column has \
                 now, and the new type may not read them the same way. Write each key as the \
                 engine spells it, apply that, then change the type.",
                over_aliases.join("\n")
            );
        }

        // A role this plan creates or renames to needs its name free of
        // every principal, not only of the roles the managed set knows:
        // users, roles and application roles share one namespace, and
        // `CREATE ROLE` on a name a user holds fails after everything
        // ordered before it has run (DECISIONS 118, 119).
        let (wanted, vacated) = role_name_expectations(
            &cs.clone().ordered(plan_mode(staged), dialect.as_ref()),
            dialect.as_ref(),
            plan_mode(staged),
            0,
        )?;
        refuse_taken_role_names(&mut conn, &wanted, &vacated).await?;

        // A role this plan drops, its members already written into it by the
        // schedule (above, in the planning transaction).
        if cs
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::DropRole { .. }))
        {
            // Ownership is refused, not planned around: the engine will not
            // drop an owning role, and moving ownership is a decision about
            // who owns a securable, not a consequence of a drop. Said here,
            // before anything runs — a staged apply would otherwise commit
            // every DROP MEMBER and then fail on the DROP ROLE.
            let owned = crate::engine::role_owned_securables(&mut conn)
                .await
                .context("cannot read what the roles own")?;
            for p in &cs.changes {
                if let pbps_model::Change::DropRole { name, .. } = &p.change
                    && let Some(securables) = owned.get(name.raw())
                    && !securables.is_empty()
                {
                    bail!(
                        "role `{name}` cannot be dropped in `{}`: it owns {}.\n\
                         Move the ownership first (`ALTER AUTHORIZATION ON {} TO dbo;`, \
                         by hand, since pbps does not decide who owns a securable), or keep \
                         the role.",
                        pbps_model::escape::Escaped(&target.label),
                        securables.join(", "),
                        securables[0],
                        name = name.shown(),
                    );
                }
            }
        }

        // The plan rules (ADR-0008), the connected ones included: this is the
        // one place a change window has a moment and a target to be measured
        // against. An `error` refuses the plan here, before anything is
        // written; `apply` never sees a policy.
        let policy = crate::attach_policy_findings(
            &mut cs, project, dialect.as_ref(), true,
            pbps_diff::Side { schema: &base, ids: &recorded_ids },
            pbps_diff::Side { schema: &declared, ids: &resolved.ids },
            retirement_revision.as_ref(),
        );
        if !json {
            for f in &policy {
                eprintln!("{}: {} — {}", f.severity_word(), f.id, f.message);
            }
        }
        if policy
            .iter()
            .any(|f| f.severity == crate::output::Severity::Error)
        {
            if json {
                // This is an answered policy question, before any artifact is
                // written. Keep the rule's typed fields and any earlier notes.
                findings.extend(policy);
                if quiet { carried.clone_from(&findings); } else {
                    crate::output::Report::plain("plan", findings).emit_json()?;
                }
            }
            bail!(
                "a policy set to `error` refuses this plan; fix the declarations, or suppress \
                 the rule in pbps.yml with a reason"
            );
        }

        findings.extend(policy);
        let crate::engine::SupportRows {
            permission_support,
            generation_support,
            owned_targets,
            unrevocable_grants,
        } = dialect
            .support_rows(
                &mut conn,
                &cs,
                &resolved.ids,
                crate::engine::SupportReads {
                    owners: &scoped.owners,
                    session_role: &scoped.session_role,
                    unrevocable: &managed.unrevocable,
                },
            )
            .await?;

        // The edition is a connection-time fact, and it is the only place the
        // two edition-dependent questions of ADR-0003 can be answered
        // honestly; a temporal table the server cannot take is answered with
        // it (#1502). Both are read here; what to do with them is below.
        let crate::engine::ServerLimits {
            edition: verdict,
            temporal,
        } = dialect.server_limits(&mut conn, &cs).await?;
        crate::engine::refuse_answered_temporal(&temporal, json, &mut findings)?;
        if !verdict.refused_online.is_empty() {
            let refusal = format!(
                "`strategy: online` is declared for {}, and `{}` runs {}, which has no online \
                 index operations.\n\
                 The statement would fail partway through the apply. Remove the hint, or deploy \
                 this change to an edition that supports it.",
                verdict.refused_online.iter().map(|subject| pbps_model::escape::Escaped(subject).to_string()).collect::<Vec<_>>().join(", "),
                pbps_model::escape::Escaped(&target.label),
                verdict.runs
            );
            if json {
                // An answered question, like the policy refusal above: the
                // edition was read and it says no (DECISIONS 26, 485). Its own
                // id rather than `plan.edition` at error severity, which a
                // consumer could only tell from the advisories by the severity
                // (#574).
                findings.push(
                    crate::output::Finding::error("plan.online-unsupported", refusal.clone())
                        .remedy(
                            "remove `strategy: online` from these tables, or deploy to an edition \
                         with online index operations",
                        ),
                );
                if quiet { carried.clone_from(&findings); } else {
                    crate::output::Report::plain("plan", findings).emit_json()?;
                }
            }
            bail!("{refusal}");
        }
        for w in &verdict.warnings {
            if !json {
                eprintln!("warning: {w}");
            }
            findings.push(crate::output::Finding::warning("plan.edition", w));
        }

        // What the plan is pinned to is wider than what the drift check
        // compared: the rows of a table the declarations cover for the first
        // time were read (the differ measured against them) but are not in
        // the recorded scope, and a baseline that left them out would let a
        // row inserted between plan and apply slip past `apply`'s check —
        // and, for `exact`, survive the approved deletes. `apply` reads the
        // same union (`pinned_scopes`) under the same reference.
        let pinned = scoped.schema.with_observed_rows(
            &managed.rows,
            &pinned_baseline_scopes(
                &recorded_data,
                &declared_data,
                cs.changes(),
                &resolved.ids,
                (&entry.snapshot.schema, &entry.snapshot.ids),
            ),
            &entry.snapshot.schema,
        )?;
        let baseline = pbps_model::drift_checksum(&pinned, &recorded_ids);
        let cost = dialect.operational_cost(&mut conn, &cs, &resolved.ids).await;
        let crate::engine::SealedFacts {
            type_pins,
            provider_pins,
            reviewed,
            routine_pins,
        } = dialect
            .seal_plan_facts(
                &plan_state,
                &mut conn,
                project,
                target,
                frame,
                &cs,
                &managed_modules(Some(&entry.snapshot), Some(&loaded.schema)),
            )
            .await?;
        let mut domain_vet_unavailable = false;
        let mut domain_code = None;
        match dialect
            .declared_domain_code(
                scratch.as_mut(),
                &mut conn,
                target,
                project,
                desired_side,
                cs.changes(),
            )
            .await?
        {
            crate::engine::DomainCode::NotAsked => {}
            crate::engine::DomainCode::Passed(row) => domain_code = Some(row),
            crate::engine::DomainCode::Refused(finding) => {
                if let Some(entry) = scratch.as_mut() {
                    entry.finish().await.map_err(|refused| match refused {
                        crate::resolution::Refused::Unanswerable(error) => error,
                        crate::resolution::Refused::Finding(finding) => anyhow::anyhow!(finding.message),
                    })?;
                }
                return Ok(Connected::Refused { finding, findings });
            }
            crate::engine::DomainCode::Unavailable { row, finding } => {
                domain_vet_unavailable = true;
                domain_code = Some(row);
                if let Some(finding) = finding {
                    // Keep the warning with the approved typed change as
                    // well as in the command's findings envelope.
                    pbps_diff::order::writebacks::findings(&mut cs, &[(0, finding.clone())]);
                    let finding = crate::policy_finding(&finding);
                    if finding.severity == crate::output::Severity::Error {
                        return Ok(Connected::Refused { finding, findings });
                    }
                    if !json { eprintln!("{}: {}", finding.severity_word(), finding.message); }
                    findings.push(finding);
                }
            }
        }
        // Case 3, last: the target is read again, after every read above,
        // and must still be what the resolver's run read; then the plan is
        // sealed in the order the passes above left (DEC-1516.1).
        let resolution = match resolution {
            None => None,
            Some((name, unsealed, asked)) => {
                // A session of its own, as the run's was: one target name
                // can reach another cluster on a new connection, which the
                // recheck must see and refuse (DEC-1685.1).
                let mut fresh = pbps_db::Conn::connect(conn.driver(), target.connection())
                    .await
                    .with_context(|| format!("cannot reach `{}` for the recheck", pbps_model::escape::Escaped(&target.label)))?;
                let rechecked = match unsealed.recheck(&mut fresh).await {
                    Ok(rechecked) => rechecked,
                    Err(refused) => {
                        let error = refused.error;
                        if !matches!(error, pbps_cli::resolver::server::Error::Changed(_)) {
                            return Err(error.into());
                        }
                        let finding =
                            crate::output::Finding::error("resolver.recheck", error.to_string())
                                .remedy("plan again");
                        return Ok(Connected::Refused { finding, findings });
                    }
                };
                let sealed = rechecked
                    .seal(cs.clone(), base_side, desired_side)
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "the resolver's evidence could not be sealed over the final plan: {e}; \
                         this is a bug in pbps, please report it"
                        )
                    })?;
                resolver_assessment
                    .as_mut()
                    .expect("a successful resolver run has an assessment")
                    .record_authorization(sealed.evidence.qualification().authorization);
                cs = sealed.changes;
                Some(Resolution {
                    name,
                    asked,
                    evidence: sealed.evidence,
                })
            }
        };
        let crate::engine::DeclaredTypes {
            pins: declared_type_pins,
            validation: domain_validation,
            deferred_domain_code,
        } = dialect
            .declared_types(
                &mut conn,
                &read_back,
                &recorded_ids,
                &declared,
                &resolved.ids,
                cs.changes(),
            )
            .await?;
        // The rehearsal, last: it runs the plan as it will be written, on the
        // scratch the resolver used or would have used (#2379).
        let course = crate::rehearsal::connected::course(
            // An unavailable advisory scratch must not become a fatal retry
            // through the independent rehearsal (DEC-2309.1).
            scratch.is_some() && !domain_vet_unavailable,
            crate::rehearsal::connected::switched_off(project, target.environment(), no_rehearse),
        );
        use crate::rehearsal::connected::Course;
        // A create runs code pbps cannot read, so its members are read from a
        // rehearsal and pinned: without one there is nothing to pin (DEC-2447.6).
        if course != Course::Rehearse && let Some((name, done)) = changed_extensions(&cs).next() {
            anyhow::bail!(
                "extension `{}` is {done} by this plan, which runs the package's script; its members are read from a rehearsal on the resolver scratch and pinned into the plan, and this plan is not rehearsed ({}). Select a resolver scratch entry and do not turn the rehearsal off (`rehearse: false`, `--no-rehearse`)",
                name.shown(),
                match course {
                    Course::Off => "the rehearsal is turned off",
                    Course::Skip | Course::Rehearse => "no scratch is available to rehearse on",
                }
            );
        }
        let mut manifests = crate::rehearsal::connected::Manifests::new();
        let rehearsal = match (course, scratch.as_mut()) {
            (Course::Off, _) => Some(pbps_model::Rehearsal::Off),
            (Course::Rehearse, Some(entry)) => {
                let ordered = cs.clone().ordered(plan_mode(staged), dialect.as_ref());
                let lowered = crate::statements(&ordered, &dialect)?;
                let mut base_hints = hints.clone();
                base_hints.annotations = hints.annotations.baseline();
                // The same refusal the plan meets below, before a rehearsal
                // can meet the statement instead.
                if !staged {
                    reject_non_transactional(&lowered)?;
                }
                let verdict = crate::rehearsal::connected::run(
                    entry,
                    &mut conn,
                    target,
                    &crate::rehearsal::connected::Plan {
                        dialect: &dialect,
                        base: base_side,
                        desired: desired_side,
                        declared: &loaded.schema,
                        hints: &hints,
                        base_hints: &base_hints,
                        dependencies: &loaded.dependencies,
                        ordered: &ordered,
                        operations: &pbps_dialect::PlanPayload::operations(&ordered),
                        statements: &lowered,
                        staged,
                    },
                )
                .await;
                // Confirmed gone before the plan goes on or is refused.
                let released = entry.finish().await;
                let verdict = verdict?;
                if let Err(crate::resolution::Refused::Unanswerable(error)) = released {
                    return Err(error);
                }
                match verdict {
                    crate::rehearsal::connected::Verdict::Sealed(record, sealed) => {
                        manifests = sealed;
                        Some(record)
                    }
                    crate::rehearsal::connected::Verdict::Refused(finding) => {
                        return Ok(Connected::Refused { finding, findings });
                    }
                }
            }
            (Course::Rehearse | Course::Skip, _) => None,
        };
        if let Some(entry) = scratch.as_mut()
            && let Err(crate::resolution::Refused::Unanswerable(error)) = entry.finish().await
        {
            return Err(error);
        }
        let crate::engine::ExtensionSeals {
            changes: extension_changes,
            pins: extension_pins,
            drops: extension_drops,
        } = dialect
            .extension_seals(&mut conn, &cs, manifests, &declared, &resolved.ids)
            .await?;
        Ok(Connected::Ordinary(Box::new((
            cs,
            // The zone this session started in: what every offset-less
            // value above was read in, and what the apply pins (DEC-1916.1).
            (
                baseline,
                managed.database_collation.clone(),
                conn.session_zone().cloned(),
            ),
            format!("{} as queried (entry #{})", target.label, entry.id),
            vec![
                permission_support,
                generation_support,
                owned_targets,
                unrevocable_grants,
                drop_blockers,
                missing_roles,
                rename_evidence,
                dependents,
                rebuilds,
                logical_slots,
                fills,
            ]
            .into_iter()
            .chain(domain_validation)
            .chain(domain_code)
            .collect::<Vec<_>>(),
            findings,
            cost,
            routine_pins,
            type_pins,
            provider_pins,
            declared_type_pins,
            reviewed,
            resolver_assessment,
            resolution,
            (rehearsal, extension_pins, extension_drops, extension_changes, deferred_domain_code),
        ))))
    }).await?;
        let (
            cs,
            (baseline_checksum, baseline_collation, baseline_zone),
            baseline_description,
            connected_checks,
            findings,
            cost,
            routine_pins,
            type_pins,
            provider_pins,
            declared_type_pins,
            reviewed,
            resolver_assessment,
            resolution,
            (rehearsal, extension_pins, extension_drops, extension_changes, deferred_domain_code),
        ) = match connected {
            Connected::Ordinary(planned) => *planned,
            Connected::Refused {
                finding,
                mut findings,
            } => {
                if json {
                    findings.push(finding);
                    return Ok(services::PlanResult::Refused(crate::output::Report::plain(
                        "plan", findings,
                    )));
                }
                let report = crate::output::Report::plain("plan", vec![finding]);
                eprint!("{}", crate::output::human(&report.findings));
                return Ok(services::PlanResult::Refused(report));
            }
        };

        if staged && pbps_diff::order::row_security::requires_transaction(&cs) {
            bail!(
                "row-security changes require a transactional plan; --staged is unsupported (DEC-1173.3)"
            );
        }
        let statements = crate::statements(
            &cs.clone().ordered(plan_mode(staged), dialect.as_ref()),
            &dialect,
        )?;
        if staged {
            // A staged plan is one logical change isolated in a deployment of its
            // own (ADR-0003). The limit is the whole point: what cannot be rolled
            // back must not be able to take four unrelated changes down with it,
            // and a resume that had to reason about which of five changes were
            // half-done would be guessing.
            let logical = crate::logical_changes(&cs);
            if logical > 1 {
                bail!(
                    "--staged applies one logical change, and this plan has {logical}.\n\
                 Stage the change that needs it in a revision of its own; the rest can go \
                 through an ordinary transactional apply."
                );
            }
            if cs.is_empty() {
                bail!("there is nothing to stage: this plan is empty");
            }
        } else {
            // §7.5: a statement that cannot run inside a transaction fails **here**,
            // not halfway through an apply with no way back.
            reject_non_transactional(&statements)?;
        }

        let names = pbps_model::plan_names::PlanNames::of(&cs.changes, &resolved.ids)?;
        let (tables, modules) = crate::report::touched(&names);
        let data = crate::PlanData {
            dependencies: dependencies.clone(),
            baseline: baseline_description.clone(),
            time_zone: baseline_zone.as_ref().map(ToString::to_string),
            changes: cs.changes.len(),
            tables,
            modules,
            roles: crate::report::touched_roles(&names),
            risks: cs.risks().iter().map(|risk| risk.as_str()).collect(),
            connected_checks: {
                let mut checks = connected_checks;
                if !checks
                    .iter()
                    .any(|check| check.name == "declared-domain-code")
                    && let Some(row) = deferred_domain_code
                {
                    checks.push(row);
                }
                crate::engine::in_contract_order(checks)
            },
            type_routines: reviewed.0.clone(),
            type_routine_approval: (!reviewed.0.is_empty())
                .then(|| crate::pins::TYPE_ROUTINE_APPROVAL.to_owned()),
            origins: crate::report::origins(&cs),
            cost: Some(cost),
            resolver_selection,
            resolver_assessment,
            rehearsal: rehearsal.as_ref().map(crate::explain::rehearsal_detail),
            target_assertions: target_assertions.clone(),
        };
        if !json {
            print!("{}", crate::dependencies::render(&dependencies));
            println!("Baseline: {baseline_description}");
            if !target_assertions.is_empty() {
                // Said where the approver reads it: this is not a check apply repeats.
                println!(
                    "Target assertions: {} passed while planning; apply does not run them.",
                    target_assertions.len()
                );
            }
            if let Some(zone) = &baseline_zone {
                // Named because nothing in a statement shows it: the same
                // `2026-02-01` is another instant in another zone (DEC-1916.1).
                println!("Offset-less date/time values read in {zone}");
            }
            // Case 1 has no resolver to report and claims no verification.
            if let Some(resolution) = &resolution {
                println!(
                    "Resolver profile: {}; it answered the binding questions of {}, and the \
                 target was rechecked after its run.",
                    resolution.name, resolution.asked
                );
                if let Some(notice) = data
                    .resolver_assessment
                    .as_ref()
                    .and_then(|a| a.authorization_notice)
                {
                    println!("Notice: {notice}");
                }
            } else if let (Some(selection), Some(assessment)) =
                (&data.resolver_selection, &data.resolver_assessment)
            {
                println!(
                    "Resolver profile: {} ({:?}); not needed for binding, so the resolver did not run: this plan moves no name a kept binding could reach ({} unaffected, {} rebuilt from their declarations).",
                    selection.name, selection.source, assessment.unaffected, assessment.rebuilt,
                );
            }
            if let Some(detail) = &data.rehearsal {
                println!("Rehearsal: {}", crate::explain::rehearsal_line(detail));
                print!("{}", crate::explain::rehearsal_render(detail));
            }
            print!("{}", crate::report::plan(&cs, &names));
            if let Some(note) = findings.iter().find(|f| f.id == "plan.undeclared-comments") {
                println!("{}", note.message);
            }
            for check in &data.connected_checks {
                println!("{}: {}", check.name, check.message);
            }
            if let Some(cost) = &data.cost {
                print!("{}", crate::cost::render(cost));
            }
        }

        let mut plan = pbps_model::SavedPlan::new(
            pbps_model::PlanOrigin::Database,
            dialect.name(),
            created_at,
            pbps_model::PlanBaseline {
                description: baseline_description.as_str().into(),
                checksum: baseline_checksum,
                database_collation: baseline_collation,
                time_zone: baseline_zone,
            },
            cs.changes().clone(),
            resolved.ids,
        );
        plan.object_deps = loaded
            .hints
            .annotations
            .wire()
            .map_err(anyhow::Error::msg)?;
        plan.dependencies = dependencies;
        plan.git_sha = db::git_sha(project.root());
        // The rows the state recorded after the apply has to cover: the
        // declarations' scope, carried so that `apply` needs no checkout.
        plan.data = loaded.schema.data_scopes();
        plan.routine_pins = routine_pins;
        plan.declared_type_pins = declared_type_pins;
        plan.type_pins = type_pins;
        plan.provider_pins = provider_pins;
        plan.pin_type_routines = project.config.pin_type_routines;
        (plan.type_routines, plan.type_routine_pins) = reviewed;
        if !json {
            print!("{}", crate::pins::reviewed_sources(&plan.type_routines));
        }
        plan.trusted_roles = project.config.trusted_roles.clone();
        plan.rehearsal = rehearsal;
        plan.extension_pins = extension_pins;
        plan.extension_drops = extension_drops;
        plan.target_assertions = target_assertions;
        plan.extension_changes = extension_changes;
        // A type the plan expects its own package to bring must be one the
        // rehearsal found in that package (DEC-2447.12).
        dialect
            .prove_plan_pins(
                &plan.type_pins,
                &plan.provider_pins,
                &plan.extension_pins,
                &plan.extension_changes,
            )
            .map_err(anyhow::Error::msg)?;
        if let Some(resolution) = resolution {
            plan = plan.with_resolution(resolution.evidence).map_err(|e| {
            anyhow::anyhow!(
                "the resolver's evidence does not describe this plan: {e}; this is a bug in pbps, \
                 please report it"
            )
        })?;
        }
        if staged {
            plan = plan.staged();
            if !json {
                println!(
                    "\nThis is a staged plan: {} statement(s) will run outside a transaction, each \n\
             recorded in the ledger as it completes. Apply it with `pbps apply --staged \n\
             --checksum {}`, and continue an interrupted run with \n\
             `--staged --resume`.",
                    statements.len(),
                    crate::report::placeholder("approved-checksum")
                );
            }
        }

        let script = script.then(|| -> anyhow::Result<String> {
        Ok(format!(
            "-- Generated by pbps against {}\n-- Applyable only through `pbps apply --plan`; never hand-edit this file.\n\n{}",
            plan.baseline.description.shown(),
            statements.render(
                // The zone the plan recorded: the script is the plan's, and
                // runs where the plan was computed (DEC-1916.1).
                plan.baseline
                    .time_zone
                    .as_ref()
                    .unwrap_or(&SessionZone::utc()),
            )?
        ))
    }).transpose()?;
        let plan = pbps_diff::order::seal(cs.ordered(plan.mode, dialect.as_ref()), plan)?;
        Ok(services::PlanResult::Planned(Box::new(services::Planned {
            artifact: Box::new(plan),
            script,
            statements: statements.len(),
            data,
            findings,
        })))
    })
}

/// What the connected half of `plan` ended with (DEC-1515.1).
pub(super) enum Connected<T> {
    /// The plan and everything checked against it: the ordinary plan
    /// (cases 1 and 2), or the resolver's, rechecked and sealed (case 3).
    Ordinary(Box<T>),
    /// Case 3, refused by an answer: unresolved, or measured incompatible.
    Refused {
        finding: crate::output::Finding,
        findings: Vec<crate::output::Finding>,
    },
}

/// Case 3's sealed answer: the resolver that gave it, the binding questions
/// it answered, and the evidence the plan carries (#1516).
pub(super) struct Resolution {
    name: String,
    asked: String,
    evidence: pbps_model::resolver::ResolverEvidence,
}
