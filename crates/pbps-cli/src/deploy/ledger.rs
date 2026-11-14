//! The commands that record or inspect the ledger without planning: `verify`,
//! `snapshot`, `baseline`, `prune` and `unlock`.

use super::*;

/// No `From` conversion: every fallible verify step must choose its own ID,
/// so a successful connection cannot relabel a later read failure (DEC-1804.1).
#[derive(Debug)]
pub(super) struct VerifyFailure {
    pub(super) id: &'static str,
    pub(super) error: anyhow::Error,
}

impl VerifyFailure {
    fn at(id: &'static str, error: impl Into<anyhow::Error>) -> Self {
        Self {
            id,
            error: error.into(),
        }
    }
}

pub(super) fn recorded_verify_state(
    found: Result<Option<pbps_db::LedgerEntry>, pbps_db::LedgerError>,
    label: &str,
) -> Result<pbps_db::LedgerEntry, VerifyFailure> {
    match found {
        Ok(Some(entry)) => Ok(entry),
        Ok(None) => Err(VerifyFailure::at(
            "state.no-entries",
            anyhow::anyhow!(
                "`{label}` has a ledger but no entries; there is nothing to compare against.\n\
                 Record one with `pbps snapshot` or `pbps baseline --reason ...`.",
                label = pbps_model::escape::Escaped(label),
            ),
        )),
        Err(error) => {
            let id = match &error {
                pbps_db::LedgerError::NotInitialized => "state.uninitialized",
                pbps_db::LedgerError::BadEntry { .. }
                | pbps_db::LedgerError::Db(_)
                | pbps_db::LedgerError::Locked(_) => "state.unreadable",
            };
            Err(VerifyFailure::at(id, error))
        }
    }
}

/// `pbps verify` — the drift check (SPEC §8.2).
///
/// The comparison is scoped and identified by the **recorded** state's identity
/// file, not the working tree's. The question this command answers is "has this
/// environment moved since pbps last recorded it", and answering it with a
/// mapping the environment has never seen would report every uncommitted local
/// rename as drift in production.
pub fn cmd_verify(project: &Project, target: &Target, json: bool) -> anyhow::Result<()> {
    let catalog = std::cell::RefCell::new(Vec::new());
    let dialect = crate::output::or_unanswerable(
        "verify",
        json,
        "project.unsupported-dialect",
        crate::dialect(project),
    )?;
    let checked_at = crate::now();

    let rt = crate::output::or_unanswerable("verify", json, "runtime.unavailable", db::runtime())?;
    let (report, unmanaged_refusal, unmanaged_inventory) = match rt.block_on(async {
        let mut conn = db::connect(target)
            .await
            .map_err(|error| VerifyFailure::at("environment.unreachable", error))?;
        let baseline =
            recorded_verify_state(crate::engine::latest(&mut conn).await, &target.label)?;

        let recorded_ids = baseline.snapshot.ids.clone();
        let recorded_modules = managed_modules(Some(&baseline.snapshot), None);
        // The rows, like the modules, under the *recorded* scope: the question
        // is whether this environment moved since pbps last recorded it, and a
        // table whose rows were never recorded has nothing to have moved from.
        let recorded_scopes = baseline.snapshot.schema.data_scopes();
        let managed = managed_state_full(
            &mut conn,
            &recorded_ids,
            &recorded_modules,
            &pbps_diff::external_parents(&baseline.snapshot.schema),
            // The unmanaged policy is applied below instead, beside the
            // finished report, so that neither verdict can erase the other.
            target.catalog_policy(pbps_config::Unmanaged::Ignore),
            &rows_to_read(&recorded_scopes),
            crate::engine::Read::Snapshot,
            CatalogWarnings::for_output(json, &catalog),
        )
        .await
        .map_err(|error| VerifyFailure::at("environment.unreadable", error))?;
        let mut scoped = managed.scoped;
        scoped.schema = scoped
            .schema
            .with_observed_rows(&managed.rows, &recorded_scopes, &baseline.snapshot.schema)
            .map_err(|error| VerifyFailure::at("verify.failed", error))?;

        // The live side is identified by what is actually there, not by the
        // recorded mapping. Comparing two sides that share one identity file
        // can only surface attribute changes on objects present in both — so a
        // hand-added or hand-dropped column, the two things a drift check most
        // needs to catch, would be exactly what it missed.
        let observed = pbps_diff::observed_ids(&scoped.schema, &recorded_ids);

        // `diff_partial`, not `diff`: a drift report wants both halves. The
        // `Result` form returns only the errors, and taking that branch meant
        // one table's altered `IDENTITY` silently deleted every expressible
        // difference the same comparison had found — an undercount in the
        // report, in the finding count, and in the hook's payload.
        // Both sides are reads, which know the sequence names they leave
        // out; compared unnamed, a renamed sequence would take the other
        // side's name and read as no change (#1631). The checksums below are
        // the reads' own.
        let mut recorded_read = baseline.snapshot.schema.clone();
        pbps_dialect::name_read_sequences(&mut recorded_read, dialect.as_ref());
        let mut live_read = scoped.schema.clone();
        pbps_dialect::name_read_sequences(&mut live_read, dialect.as_ref());
        live_read.keep_only_named_unidentified(&recorded_read);
        without_comments(&mut recorded_read, &mut live_read);
        // Compared by name here and not by the differ, which walks the new side only: a
        // recorded sequence that was dropped or renamed away is on neither side's list there.
        // Out of the differ's inputs afterwards, so none is reported twice, in the words of a
        // plan ("declared", "the target has none").
        let unidentified = pbps_diff::unidentified_drift(&recorded_read, &live_read);
        for read in [&mut recorded_read, &mut live_read] {
            read.search.clear();
            read.sequences.clear();
        }
        let diffed = pbps_diff::diff_partial(
            pbps_diff::Side {
                schema: &recorded_read,
                ids: &recorded_ids,
            },
            pbps_diff::Side {
                schema: &live_read,
                ids: &observed,
            },
            dialect.as_ref(),
            // Drift emits no SQL, so there is no execution to give a hint
            // about; passing the declarations' strategies here would put a
            // hint nobody can act on into a report about what already happened.
            &pbps_model::Hints::default(),
        );
        // Carried *in* the report, not raised as a separate outcome. The
        // database was reached and a difference was established — the differ
        // simply has no `Change` for it — so this is drift, and everything
        // downstream (findings, the envelope, the `on_drift` hook, exit 2)
        // must treat it as such. An earlier fix made it exit 2 but returned
        // early, which skipped the hook: right verdict, and the alert that
        // exists to carry that verdict never fired.
        let changes = diffed.changes;
        let mut unexpressible: Vec<String> =
            diffed.errors.iter().map(ToString::to_string).collect();
        unexpressible.extend(managed.limitations);
        unexpressible.extend(unidentified);
        // A managed role's permission the declarations cannot spell is the
        // same kind of difference as a catalog fact they cannot: established,
        // and with no `Change` to carry it (DECISIONS 95).
        unexpressible.extend(scoped.unexpressible.iter().cloned());
        // Unmanaged objects are outside the drift scope by definition. Keep
        // their policy verdict beside the completed drift report: neither can
        // erase the other when both are present. `report_unmanaged` currently
        // has only this one typed failure, but preserve any future operational
        // error as an inability to answer rather than misclassifying it as a
        // policy finding.
        let unmanaged_inventory = unmanaged_objects(
            &scoped,
            &managed.unreadable,
            &recorded_ids,
            &recorded_modules,
        );
        let unmanaged_refusal =
            match apply_unmanaged_policy(&unmanaged_inventory, project.config.unmanaged) {
                Ok(()) => None,
                Err(error) => match error.downcast::<UnmanagedPolicy>() {
                    Ok(policy) => Some(policy),
                    Err(error) => return Err(VerifyFailure::at("verify.failed", error)),
                },
            };

        Ok((
            pbps_model::DriftReport {
                version: pbps_model::drift::CURRENT_VERSION,
                environment: target.label.clone(),
                checked_at,
                baseline: pbps_model::DriftBaseline {
                    entry_id: baseline.id,
                    applied_at: baseline.applied_at.clone(),
                    checksum: pbps_model::drift_checksum(&baseline.snapshot.schema, &recorded_ids),
                },
                // The checksum compares like with like: both sides fingerprinted
                // with the recorded mapping, so a derived identity for a hand-added
                // column cannot by itself make the two differ.
                live_checksum: pbps_model::drift_checksum(&scoped.schema, &recorded_ids),
                changes,
                unmanaged: scoped.unmanaged,
                unexpressible,
            },
            unmanaged_refusal,
            unmanaged_inventory,
        ))
    }) {
        Ok(outcome) => outcome,
        Err(VerifyFailure { id, error }) => {
            // Unanswerable: `verify` was asked whether this database still
            // matches its recorded state, and it could not look. Without
            // this the JSON path returned before its own branch, leaving
            // stdout empty — so a consumer got the converter's generic
            // "produced no output" instead of a report naming the target
            // (SPEC §9.8).
            if json {
                let mut findings = vec![crate::output::Finding::error(
                    id,
                    format!("{}: {error:#}", pbps_model::escape::Escaped(&target.label)),
                )];
                findings.extend(catalog.take());
                crate::output::unanswerable("verify", findings);
            }
            return Err(error);
        }
    };

    // The hook always receives the bare report, whatever the human asked for on
    // stdout: a script's payload should not change shape because someone added
    // a flag for their own eyes, and it must not gain an envelope because
    // stdout did.
    let payload = format!("{}\n", serde_json::to_string_pretty(&report)?);

    let mut findings = catalog.into_inner();
    if report.has_drift() {
        findings.push(
            crate::output::Finding::error(
                "state.drift",
                format!(
                    // Both counted: a difference the differ cannot phrase is
                    // still a difference, and "0 difference(s)" beside a
                    // drift verdict reads as a bug in the tool rather than a
                    // fact about the database.
                    "`{}` no longer matches its recorded state ({} difference(s))",
                    pbps_model::escape::Escaped(&target.label),
                    report.changes.changes.len() + report.unexpressible.len()
                ),
            )
            .remedy(crate::report::DRIFT_REMEDY),
        );
    }
    // One finding per difference, because the differ returns one per
    // unexpressible change and the column name in it is the remedy.
    for e in &report.unexpressible {
        findings.push(
            crate::output::Finding::error(
                "state.drift-unexpressible",
                format!("{}: {e}", pbps_model::escape::Escaped(&target.label)),
            )
            .remedy(crate::report::DRIFT_REMEDY),
        );
    }
    // The typed report predates managed modules and carries tables only. JSON
    // findings use the complete policy inventory so readable and unreadable
    // modules cannot disappear from the machine view while stderr names them.
    for object in &unmanaged_inventory {
        findings.push(crate::output::Finding::note(
            "state.unmanaged",
            format!("{object} is in the database and outside the managed set; it was not compared"),
        ));
    }
    if let Some(ref refusal) = unmanaged_refusal {
        findings.push(
            crate::output::Finding::error(
                "state.unmanaged-refused",
                format!("{}: {refusal}", pbps_model::escape::Escaped(&target.label)),
            )
            .remedy("pbps pull, or relax `unmanaged:` in pbps.yml"),
        );
    }

    if json {
        // Preserve the established policy-only shape (`data` absent), while a
        // real drift still carries its complete report even when the unmanaged
        // policy also failed.
        let data = if unmanaged_refusal.is_some() && !report.has_drift() {
            None
        } else {
            Some(&report)
        };
        // The envelope, not the bare report: a consumer reading `verify` beside
        // `validate` should not need a second parser for one of them (SPEC
        // §14.1). The report itself is unchanged, one level down in `data`.
        println!(
            "{}",
            serde_json::to_string_pretty(&crate::output::Report::new("verify", findings, data))?
        );
    } else {
        if unmanaged_refusal.is_none() || report.has_drift() {
            print!("{}", crate::report::drift(&report));
        }
        if let Some(ref refusal) = unmanaged_refusal {
            eprintln!("{refusal}");
        }
    }

    if report.has_drift() {
        if let Some(hook) = &project.config.hooks.on_drift {
            // After the envelope under `--format json`: the hook's own output
            // must not become trailing bytes of that document (#493).
            let stdout = if json {
                crate::hooks::HookStdout::Stderr
            } else {
                crate::hooks::HookStdout::Shared
            };
            crate::hooks::run(hook, &payload, "on_drift", stdout);
        }
        // A distinct exit code so a scheduled pipeline can tell "the database moved"
        // from "the tool could not run" — the two need different people woken up.
        return Err(crate::Found::reported().into());
    }
    if unmanaged_refusal.is_some() {
        // The project's own policy was refused on a database that was read
        // successfully. `verify` answered — exit 2 and the schema owner's, not
        // exit 1 and CI's.
        return Err(crate::Found::reported().into());
    }
    Ok(())
}

/// `pbps snapshot` — record the current state, refusing to bless a difference.
pub fn cmd_snapshot(project: &Project, target: &Target, force: bool) -> anyhow::Result<()> {
    let operator = crate::operator(project.root())?;
    let ids = crate::read_ids(project)?;
    let (declared_modules, declared_data, loaded) =
        declared_scope(project, crate::dialect(project)?.as_ref())?;

    db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;
        crate::engine::lock(&mut conn, &operator).await?;
        let result = async {
            let ids = crate::engine::installation_ids(&mut conn, &ids).await?;
            let mut scoped = managed_state(
                &mut conn,
                &ids,
                &declared_modules,
                &pbps_diff::external_parents(&loaded.schema),
                target.catalog_policy(project.config.unmanaged),
                &declared_data,
                &loaded.schema,
                crate::engine::Read::Snapshot,
                CatalogWarnings::Human,
            )
            .await?;
            report_missing(&scoped);
            refuse_unexpressible(&scoped, &target.label, "snapshot again")?;

            // Comparing against the recorded state is the whole guard. A snapshot
            // that overwrites a state it differs from is exactly "somebody SSHed in
            // and changed the schema" being quietly adopted by a pipeline.
            match crate::engine::latest(&mut conn).await {
                Ok(Some(previous)) if !previous.snapshot.matches(&scoped.schema) && !force => {
                    bail!(
                        "`{}` differs from the state recorded at {} (entry #{}).\n\
                         `pbps verify` shows what changed. Then either put the database back and \
                         re-run, or accept it with `pbps baseline --reason ...`.\n\
                         `--force` records it anyway.",
                        pbps_model::escape::Escaped(&target.label),
                        previous.applied_at,
                        previous.id
                    );
                }
                Ok(Some(_)) => {}
                // Taking the lock initializes the tables, so a first snapshot
                // is now represented by an empty ledger rather than
                // `NotInitialized`. It remains an adoption decision.
                Ok(None) if !force => bail!(
                    "pbps has never recorded a state for `{}`.\n\
                     Adopt it deliberately with `pbps baseline --reason ...`, or `--force` to \
                     record it as it stands.",
                    pbps_model::escape::Escaped(&target.label)
                ),
                Ok(None) => {}
                Err(e) => return Err(e.into()),
            }

            // What the database holds, not every uid the ids file names: a
            // missing table or role recorded as held is one the next plan
            // never creates (#1600, #1606).
            crate::engine::prove_tables_absent(&mut conn, &scoped.missing).await?;
            let gone = crate::engine::absent_roles(&mut conn, &scoped.missing_roles).await?;
            // Only what the declarations name is recorded of the kinds with no identity record
            // yet: a sequence nobody declared is not this project's, and its dropping later is
            // not this project's drift (DEC-2721.1).
            scoped.schema.keep_only_named_unidentified(&loaded.schema);
            let held = ids
                .without(&scoped.missing)
                .without_roles(&gone)
                .without_types(&scoped.missing_types);
            let mut snapshot = with_provenance(
                project.root(),
                StateSnapshot::new(StateKind::Apply, scoped.schema, held, &operator),
            );
            snapshot.baseline_evidence = scoped.baseline_evidence;
            snapshot.module_deps = loaded
                .hints
                .annotations
                .wire()
                .map_err(anyhow::Error::msg)?;
            let tables = snapshot.schema.tables.len();
            let id = crate::engine::record(&mut conn, &snapshot).await?;
            Ok::<_, anyhow::Error>((id, tables))
        }
        .await;
        let released = crate::engine::unlock(&mut conn).await;
        let (id, tables) = match result {
            Ok(recorded) => recorded,
            Err(error) => {
                warn_unreleased(&target.label, &released);
                return Err(error);
            }
        };
        println!(
            "Recorded the state of `{}` as entry #{id} ({} table(s)).",
            pbps_model::escape::Escaped(&target.label),
            tables
        );
        // The ledger row is durable even when deleting the lock row fails.
        // Report that before surfacing cleanup so retrying cannot look safe.
        released?;
        Ok(())
    })
}

/// The table uids a drop intent in the working identity file names (#1852):
/// a tombstone's uid is the table's identity whatever it was renamed to
/// before the drop, so it is matched against the recorded state by uid.
pub(super) fn tables_a_drop_intent_names(ids: &IdsFile) -> Vec<&pbps_model::Uid> {
    ids.tombstones
        .keys()
        .filter(|uid| uid.kind() == pbps_model::UidKind::Table)
        .collect()
}

/// Refuses a baseline that would hand over a held partition with pending drop
/// intent (DEC-1852.1). Uids still decide what is held. ADR-0018's opaque
/// evidence can narrow that refusal to one live partition, or prove its drop;
/// missing/mismatched evidence retains the parent-and-bound refusal below.
/// An unreadable ledger cannot establish what is held and refuses the handover.
pub(super) fn refuse_handing_over_held_partitions(
    dropped: &[&pbps_model::Uid],
    working: &IdsFile,
    scoped: &pbps_diff::Scoped,
    previous: Result<Option<pbps_db::LedgerEntry>, pbps_db::LedgerError>,
    label: &str,
    catalog: Option<&pbps_model::baseline_evidence::BaselineCatalog>,
) -> anyhow::Result<()> {
    let lines: Vec<String> = match &previous {
        Ok(None) => return Ok(()),
        Ok(Some(previous)) => {
            let recorded = &previous.snapshot;
            dropped
                .iter()
                .filter_map(|uid| {
                    let name = recorded.ids.tables.get(*uid)?;
                    let of = recorded.schema.tables.get(name)?.partition_of.as_ref()?;
                    let parent = recorded
                        .ids
                        .tables
                        .iter()
                        .find(|(_, held)| **held == of.parent)
                        .map(|(parent, _)| parent)?;
                    let parent = working.tables.get(parent)?;
                    let siblings: Vec<(&TableName, &pbps_model::PartitionOf)> = scoped
                        .external
                        .iter()
                        .filter(|(_, live)| live.parent == *parent)
                        .collect();
                    if siblings.is_empty() {
                        return None;
                    }
                    let entry = previous.id;
                    if let Some(catalog) = catalog {
                        use pbps_model::baseline_evidence::HeldPartition;
                        match catalog.held_partition(
                            recorded.baseline_evidence.as_ref(),
                            name,
                            &of.parent,
                            parent,
                        ) {
                            HeldPartition::Dropped => return None,
                            HeldPartition::Present(live) => {
                                if !scoped.external.contains_key(&live) {
                                    return None;
                                }
                                let was = if name == &live {
                                    String::new()
                                } else {
                                    format!(", recorded as {}", name.shown())
                                };
                                return Some(format!(
                                    "{} is named by a drop intent and held by the recorded state \
                                     (entry #{entry}{was}); this baseline would leave it to its \
                                     external parent undropped",
                                    live.shown()
                                ));
                            }
                            HeldPartition::ParentRefusal => {}
                        }
                    }
                    Some(
                        match siblings.iter().find(|(_, live)| live.bound == of.bound) {
                            Some((live, _)) => {
                                let was = if name == *live {
                                    String::new()
                                } else {
                                    format!(", recorded as {}", name.shown())
                                };
                                format!(
                                    "{} is named by a drop intent and held by the recorded state \
                                     (entry #{entry}{was}); this baseline would leave it to its \
                                     external parent undropped",
                                    live.shown()
                                )
                            }
                            None => format!(
                                "{} is named by a drop intent and held by the recorded state \
                                 (entry #{entry}) as a partition of {}, whose partitions this \
                                 baseline would leave external ({}); none has its bound, and \
                                 which of them it is now cannot be told",
                                name.shown(),
                                parent.shown(),
                                siblings
                                    .iter()
                                    .map(|(live, _)| live.shown().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        },
                    )
                })
                .collect()
        }
        Err(e) => scoped
            .external
            .keys()
            .map(|name| {
                format!(
                    "{} is left to its external parent, and whether a drop intent names a table \
                     the recorded state holds there cannot be read ({e})",
                    name.shown()
                )
            })
            .collect(),
    };
    if lines.is_empty() {
        return Ok(());
    }
    bail!(
        "`{label}` holds {} partition(s) this baseline would hand over:\n  {}\n\
         A drop intent leaves a partition out of the identity file before it is dropped. \
         Finish the pending drop first with `pbps plan` then `pbps apply --plan <plan>` \
         (keeping the parent's partitions declared), then baseline; if it was dropped by hand, \
         revert the drop intent with `git revert <drop-intent-commit>` so the baseline records \
         it missing (DEC-1852.1).",
        lines.len(),
        lines.join("\n  "),
        label = pbps_model::escape::Escaped(label),
    );
}

/// `pbps baseline` — take the database as it stands as the new starting point.
pub fn cmd_baseline(
    project: &Project,
    target: &Target,
    reason: &pbps_model::Reason,
) -> anyhow::Result<()> {
    let operator = crate::operator(project.root())?;
    let ids = crate::read_ids(project)?;
    let dialect = crate::dialect(project)?;
    let (declared_modules, declared_data, loaded) = declared_scope(project, dialect.as_ref())?;

    db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;
        crate::engine::lock(&mut conn, &operator).await?;
        let result = async {
            let ids = crate::engine::installation_ids(&mut conn, &ids).await?;
            // A staged attach's temporary CHECK is taken out in the same
            // transaction as the read and the record. Dropped on its own, a
            // baseline that then refused left the database without it while
            // the newest staged checkpoint still recorded it, and `apply
            // --resume` refused that drift (#2127). Without one to remove,
            // the read is the snapshot it always was.
            let checks = crate::engine::standing_scan_checks(&mut conn, &ids).await?;
            let read = if checks.is_empty() {
                crate::engine::Read::Snapshot
            } else {
                conn.begin(&dialect.transaction_framing(&pin_zone(&conn)))
                    .await
                    .context("cannot open a transaction")?;
                crate::engine::Read::InsideOwnTransaction
            };
            let body = async {
                let removed = crate::engine::drop_scan_checks(&mut conn, &checks).await?;
                let mut scoped = managed_state(
                    &mut conn,
                    &ids,
                    &declared_modules,
                    &pbps_diff::external_parents(&loaded.schema),
                    target.catalog_policy(project.config.unmanaged),
                    &declared_data,
                    &loaded.schema,
                    read,
                    CatalogWarnings::Human,
                )
                .await?;
                // Adoption certifies only the selected exact packages, never
                // a different installed version or an unheld discovery (DEC-2447.3).
                refuse_extension_prerequisite_drift(
                    &scoped.schema,
                    &loaded.schema,
                    &ids,
                    dialect.as_ref(),
                    "baseline",
                )?;
                report_missing(&scoped);
                refuse_unexpressible(&scoped, &target.label, "baseline again")?;
                // A partition a drop intent names is out of the working identity
                // file before it is dropped, and the scope then reads it as one of
                // its external parent's (#1852). Asked only while an intent names a
                // table and a partition is left external, so a baseline that is the
                // remedy for an unreadable ledger, or a deliberate handover with no
                // intent, never reads the ledger here.
                let dropped = tables_a_drop_intent_names(&ids);
                if !dropped.is_empty() && !scoped.external.is_empty() {
                    let previous = crate::engine::latest(&mut conn).await;
                    let catalog = crate::engine::baseline_catalog(&mut conn).await?;
                    refuse_handing_over_held_partitions(
                        &dropped,
                        &ids,
                        &scoped,
                        previous,
                        &target.label,
                        catalog.as_ref(),
                    )?;
                }

                // What the database holds, as `snapshot` records it (#1600, #1606).
                crate::engine::prove_tables_absent(&mut conn, &scoped.missing).await?;
                let gone = crate::engine::absent_roles(&mut conn, &scoped.missing_roles).await?;
                // Only what the declarations name is recorded of the kinds with no identity record
                // yet: a sequence nobody declared is not this project's, and its dropping later is
                // not this project's drift (DEC-2721.1).
                scoped.schema.keep_only_named_unidentified(&loaded.schema);
                let held = ids
                    .without(&scoped.missing)
                    .without_roles(&gone)
                    .without_types(&scoped.missing_types);
                let mut snapshot = with_provenance(
                    project.root(),
                    StateSnapshot::new(StateKind::Baseline, scoped.schema, held, &operator),
                );
                snapshot.baseline_evidence = scoped.baseline_evidence;
                snapshot.module_deps = loaded
                    .hints
                    .annotations
                    .wire()
                    .map_err(anyhow::Error::msg)?;
                // The note on what was removed rides on the reason only where
                // the bounded column has room for both: it never turns a reason
                // that fits into a record that fails. The removal is printed
                // either way, once it has committed.
                let noted = format!("{reason} (first removed {})", removed.join(", "));
                snapshot.reason = Some(
                    if removed.is_empty()
                        || crate::engine::truncate_reason(conn.driver(), &noted) != noted
                    {
                        reason.to_string()
                    } else {
                        noted
                    }
                    .into(),
                );

                let tables = snapshot.schema.tables.len();
                let id = crate::engine::record(&mut conn, &snapshot).await?;
                Ok::<_, anyhow::Error>((id, tables, removed))
            }
            .await;
            let (id, tables, removed) = if checks.is_empty() {
                body
            } else {
                finish_transaction(&mut conn, dialect.as_ref(), body).await
            }?;
            for said in &removed {
                println!("Removed {}.", pbps_model::escape::Escaped(said));
            }
            Ok::<_, anyhow::Error>((id, tables))
        }
        .await;
        let released = crate::engine::unlock(&mut conn).await;
        let (id, tables) = match result {
            Ok(recorded) => recorded,
            Err(error) => {
                warn_unreleased(&target.label, &released);
                return Err(error);
            }
        };
        println!(
            "Baselined `{}` as entry #{id}: {} table(s) are now the starting point.",
            pbps_model::escape::Escaped(&target.label),
            tables
        );
        println!("Reason recorded: {reason}");
        // The baseline was recorded even if cleanup now reports an error.
        released?;
        Ok(())
    })
}

/// `pbps state prune` — drop old snapshots.
pub fn cmd_prune(target: &Target, keep: u32) -> anyhow::Result<()> {
    db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;
        let removed = crate::engine::prune(&mut conn, keep).await?;
        // The effective figure, not the one asked for: `--keep 0` still keeps
        // the newest entry, and reporting "0 remain" would describe an
        // environment with no baseline — which is not what happened.
        let kept = keep.max(1);
        println!(
            "Pruned {removed} old snapshot(s) from `{}`; the {kept} newest remain.",
            pbps_model::escape::Escaped(&target.label)
        );
        if keep == 0 {
            println!(
                "(--keep 0 still keeps the current baseline: dropping it would switch drift detection off.)"
            );
        }
        Ok(())
    })
}

/// `pbps unlock` — clear a lock left behind by a process that died.
pub fn cmd_unlock(target: &Target) -> anyhow::Result<()> {
    db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;
        let holder = crate::engine::lock_holder(&mut conn).await?;
        match crate::engine::unlock(&mut conn).await? {
            true => {
                let who = holder
                    .as_ref()
                    .map(|h| format!("`{}` since {}", h.locked_by, h.locked_at))
                    .unwrap_or_else(|| "an unnamed holder".to_owned());
                println!(
                    "Released the lock on `{}`, held by {who}.",
                    pbps_model::escape::Escaped(&target.label)
                );
                // Printed after the release as well as in every "locked by"
                // refusal before it: if the holder is in fact alive, this is
                // how to find it and stop it before it records over whatever
                // runs next (#1188).
                if let Some(lookup) = holder.and_then(|h| h.session_lookup) {
                    println!("{lookup}");
                }
                // Nothing about a released lock proves the operation it guarded
                // finished, and a half-finished apply is exactly what drift
                // detection is for.
                println!("Run `pbps verify` before applying anything else.");
            }
            false => println!(
                "`{}` was not locked.",
                pbps_model::escape::Escaped(&target.label)
            ),
        }
        Ok(())
    })
}
