//! `bootstrap`: building the whole schema into an empty database, and its
//! refusals.

use super::*;

/// `pbps bootstrap` — build the whole schema from the declarations.
///
/// Two halves that are useful separately: `--sql` writes the CREATE script for a
/// DR runbook or an air-gapped host, and a target executes it. Neither implies
/// the other — the script is worth having on a machine that cannot reach the
/// database at all.
/// Refuses declarations `validate` would reject, before a single statement is
/// written for a real database (DECISIONS 141).
///
/// The commands below hand SQL to a database that is not a rehearsal. A
/// declaration the dialect refuses — `execute` granted on a table, say — is
/// not a plan that fails to convert; it is a plan whose *last* statements
/// fail, and in staged mode the ones before them have already committed.
///
/// With `json`, the refusal is the findings `validate` reports for the same
/// declarations, as one findings document: a refusal the operator can act on
/// is not an operational failure, and `plan.failed` would hide which
/// declaration to fix (#573).
pub(super) fn refuse_invalid_declarations(
    loaded: &pbps_load::Loaded,
    dialect: &dyn Dialect,
    json: bool,
) -> anyhow::Result<()> {
    let problems = crate::declaration_problems(loaded, dialect);
    if problems.is_empty() {
        return Ok(());
    }
    if json {
        return crate::output::Report::plain(
            "plan",
            problems
                .into_iter()
                .map(|(id, problem)| crate::output::Finding::error(id, problem))
                .collect(),
        )
        .emit_json();
    }
    bail!(
        "the declarations have {} problem(s) that would reach the database:\n  {}\n\
         `pbps validate` reports these with their source lines.",
        problems.len(),
        problems
            .iter()
            .map(|(_, problem)| problem.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

// Immediate bootstrap has no reviewed source artifact, including before its
// spelling probes. Recheck before execution too (DEC-2181.2).
pub(super) async fn bootstrap_type_pins(
    conn: &mut Conn,
    schema: &Schema,
    changes: &pbps_model::ChangeSet,
    project: &Project,
) -> anyhow::Result<crate::engine::Prerequisites> {
    let pins = crate::engine::seal_type_pins(
        conn,
        &[schema],
        changes,
        &project.config.trusted_roles,
        project.config.pin_type_routines,
    )
    .await?;
    if !crate::engine::type_input_routines(
        conn,
        &pins.types,
        &project.config.trusted_roles,
        project.config.pin_type_routines,
        false,
    )
    .await?
    .is_empty()
    {
        bail!(
            "immediate bootstrap cannot run type routines without a reviewed saved plan; baseline an empty target, then plan, review and apply (DEC-2181.2)"
        );
    }
    Ok(pins)
}

/// A missing uid makes the differ skip an object that the declaration record
/// still claims. Modules use names rather than uids (ADR-0009 §1).
pub(super) fn refuse_unidentified_bootstrap_objects(
    schema: &Schema,
    ids: &IdsFile,
) -> anyhow::Result<()> {
    // No `..`: a new object family must choose its identity rule here.
    let Schema {
        tables,
        types,
        // Text search objects take identities with the next part (#2728).
        search: _,
        modules: _,
        roles,
        extensions,
    } = schema;
    let unidentified: Vec<String> = extensions
        .keys()
        .filter(|name| ids.extension_uid(name).is_none())
        .map(|name| format!("extension {}", name.shown()))
        .chain(
            tables
                .keys()
                .filter(|name| ids.table_uid(name).is_none())
                .map(|name| name.shown().to_string()),
        )
        .chain(
            roles
                .keys()
                .filter(|name| ids.role_uid(name).is_none())
                .map(|name| format!("role {}", name.shown())),
        )
        .chain(
            types
                .keys()
                .filter(|name| {
                    matches!(
                        TypeResolver::new(ids, ids).resolve(TypeSide::Desired, name),
                        TypeIdentity::ExternalOrUnrecorded(_)
                    )
                })
                .map(|name| format!("type {}", name.shown())),
        )
        .collect();
    if !unidentified.is_empty() {
        bail!(
            "the identity file does not know {}: {}.\n\
             Run `pbps plan` first to mint identities for the declarations.",
            if unidentified.len() == 1 {
                "a declared object"
            } else {
                "these declared objects"
            },
            unidentified.join(", ")
        );
    }
    Ok(())
}

/// [`refuse_nonempty_bootstrap`] over what the declarations can occupy: a kind held by name
/// only counts only where `declared` names it, since bootstrap would have to create that one.
/// One nothing names is not an object of this project, and an empty database that holds one
/// is still empty for it (#2855, DEC-2855.1).
pub(super) fn refuse_occupied_bootstrap(
    existing: &Schema,
    declared: &Schema,
    limitations: &[String],
    dialect: &dyn Dialect,
    label: &str,
) -> anyhow::Result<()> {
    refuse_nonempty_bootstrap(
        &existing.clone().with_name_only_restricted_to(declared),
        limitations,
        dialect,
        label,
    )
}

/// Refuse before attempting a transaction: only a proven empty managed set
/// justifies the empty snapshot recorded after a rolled-back bootstrap.
pub(super) fn refuse_nonempty_bootstrap(
    schema: &Schema,
    limitations: &[String],
    dialect: &dyn Dialect,
    label: &str,
) -> anyhow::Result<()> {
    // Derive emptiness from the diagnostic inventory so the two cannot differ.
    let Schema {
        tables,
        types,
        search,
        modules,
        roles,
        extensions,
    } = schema;
    let names: Vec<String> = tables
        .keys()
        .map(|name| name.shown().to_string())
        .chain(search.keys().map(|name| name.shown().to_string()))
        .chain(modules.keys().map(|id| id.shown().to_string()))
        // PostgreSQL cluster roles with no managed grants are prerequisites,
        // rather than objects bootstrap creates (DECISIONS 211).
        .chain(
            roles
                .iter()
                .filter(|(_, role)| dialect.manages_roles() || !role.grants.is_empty())
                .map(|(name, _)| format!("role {}", name.shown())),
        )
        .chain(types.keys().map(|name| format!("type {}", name.shown())))
        .chain(
            extensions
                .keys()
                // Exact PostgreSQL packages are separately checked prerequisites,
                // like standing cluster roles, not objects we create (DEC-2447.2).
                .filter(|_| dialect.name() != "postgres")
                .map(|name| format!("extension {}", name.shown())),
        )
        // Omitted definitions still occupy names in the managed set.
        .chain(limitations.iter().cloned())
        .collect();
    if !names.is_empty() {
        bail!(
            "`{label}` already has {} of the declared object(s): {}.\n\
             Bootstrap builds into an empty database; use `pbps plan --db` and `pbps \
             apply` to migrate it instead.",
            names.len(),
            names.join(", "),
            label = pbps_model::escape::Escaped(label),
        );
    }
    Ok(())
}

/// Bootstrap may adopt packages it verifies, but cannot install or change them.
pub(super) fn refuse_extension_prerequisite_drift(
    installed: &Schema,
    desired: &Schema,
    ids: &IdsFile,
    dialect: &dyn Dialect,
    operation: &str,
) -> anyhow::Result<()> {
    for name in desired.extensions.keys() {
        anyhow::ensure!(
            ids.extension_uid(name).is_some(),
            "extension `{}` has no identity; select it with `pbps pull --extension` before {operation}",
            name.shown()
        );
    }
    let errors = pbps_diff::prerequisite_differences(
        pbps_diff::Side {
            schema: installed,
            ids,
        },
        pbps_diff::Side {
            schema: desired,
            ids,
        },
        dialect.name(),
    );
    if !errors.is_empty() {
        bail!(
            "{operation} extension prerequisites differ:\n{}",
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    Ok(())
}

/// Keep the read-back and authored definitions together: the former proves
/// drift, while the latter preserves expressions the catalog respells.
pub(super) fn bootstrap_record(
    built: pbps_diff::Scoped,
    held: IdsFile,
    loaded: &pbps_load::Loaded,
    operator: &str,
) -> anyhow::Result<StateSnapshot> {
    // `built` is the recorded form already: the read it came from keeps a kind held by
    // name only only where `loaded` names it (`managed_state_once`, DEC-2855.1).
    let mut snapshot = StateSnapshot::new(StateKind::Bootstrap, built.schema, held, operator);
    snapshot.baseline_evidence = built.baseline_evidence;
    snapshot.module_deps = loaded
        .hints
        .annotations
        .wire()
        .map_err(anyhow::Error::msg)?;
    // Every object was created from these declarations (ADR-0009 §2.2).
    snapshot.declared = pbps_model::Declared::from_schema(&loaded.schema);
    Ok(snapshot)
}

pub(super) fn bootstrap(
    project: &Project,
    target: Option<&Target>,
    sql_out: Option<&std::path::Path>,
    declarations: services::Declarations,
    dialect: crate::emission::SelectedDialect,
    operator: &str,
) -> anyhow::Result<Option<services::Bootstrapped>> {
    match db::runtime()?.block_on(bootstrap_async(
        project,
        target,
        sql_out,
        declarations,
        dialect,
        operator,
        CatalogWarnings::Human,
    ))? {
        services::Bootstrap::Script => Ok(None),
        services::Bootstrap::Built(built) => Ok(Some(built)),
        services::Bootstrap::Failed { error, released } => {
            // The command's own error is returned; the owned test run reports
            // the same outcome as a finding instead of this warning.
            if let Some(target) = target {
                warn_unreleased(&target.label, &released);
            }
            Err(error)
        }
    }
}

pub(super) async fn bootstrap_async(
    project: &Project,
    target: Option<&Target>,
    sql_out: Option<&std::path::Path>,
    declarations: services::Declarations,
    dialect: crate::emission::SelectedDialect,
    operator: &str,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<services::Bootstrap> {
    pbps_cli::with_plan_dialect!(dialect, |mut dialect| {
        let services::Declarations { mut loaded, ids } = declarations;
        refuse_invalid_declarations(&loaded, dialect.as_ref(), false)?;
        refuse_unidentified_bootstrap_objects(&loaded.schema, &ids)?;

        // A connected generation proves nothing about a later replay target.
        // Keep the input-path proof in connected execution (DEC-2181.1).
        if sql_out.is_some() && (
            crate::engine::type_references(&loaded.schema).iter().any(|r| !loaded.schema.types.contains_key(r))
            || loaded.schema.types.values().any(|t| matches!(t, pbps_model::ManagedType::Domain { default, checks, .. } if default.is_some() || !checks.is_empty()))
        ) {
            bail!(
                "bootstrap --sql cannot validate referenced type input paths for its replay target; run connected bootstrap without --sql, or use plan/apply against the target (DEC-2181.1)"
            );
        }

        // Every declared text, as the engine reads it, before anything is built
        // from them: a spelling it would read back differently would be recorded
        // as the engine spells it and drift from the declaration on the next plan
        // (DECISIONS 101), and a date or time it reads one way only is built,
        // scripted and recorded as that reading (DEC-1883.1) — so this is asked
        // ahead of the diff, not inside the build where it used to be, since the
        // statements and the `--sql` script are made from what it answers. Into
        // an empty database, so nothing is under an older name: the declared
        // names are the only ones the catalog could have. Not under the lock: the
        // engine's reading of a text does not turn on what the database holds.
        // Without a target nothing can be asked, and the script carries the
        // declared text, as it always has.
        if let Some(target) = target {
            loaded.schema = Box::pin(async {
                let mut conn = db::connect(target).await?;
                dialect = crate::engine::planning_dialect_for(&mut conn, &dialect).await?;
                bootstrap_type_pins(&mut conn, &loaded.schema, &Default::default(), project)
                    .await?;
                let mut unasked = Vec::new();
                let refused = refuse_misspelt(
                    &mut conn,
                    &loaded.schema,
                    &Default::default(),
                    &mut unasked,
                    false,
                )
                .await;
                // Printed before the refusal, beside it, as the plan prints them.
                warn_unasked(&unasked);
                refused
            })
            .await?;
        }
        for notice in crate::row_security::binding_notices(&loaded.schema) {
            eprintln!("warning: {notice}");
        }
        let declared_modules = managed_modules(None, Some(&loaded.schema));

        // Every ordinary object is created; exact preinstalled packages are
        // prerequisites, checked under the lock and at read-back (DEC-2447.2).
        let (prerequisites, prerequisite_ids) = pbps_diff::bootstrap_base(pbps_diff::Side {
            schema: &loaded.schema,
            ids: &ids,
        });
        let cs = pbps_diff::diff(
            pbps_diff::Side {
                schema: &prerequisites,
                ids: &prerequisite_ids,
            },
            pbps_diff::Side {
                schema: &loaded.schema,
                ids: &ids,
            },
            dialect.as_ref(),
            // Bootstrap builds into an empty database, so there are no rows for an
            // online operation to spare and the strategies are dropped. The module
            // dependencies are not: `depends_on:` exists for the edges the
            // identifier scan cannot see, and discarding them here would make
            // bootstrap emit a dependent module first and fail on declarations that
            // plan perfectly well.
            &pbps_model::Hints {
                row_guard_defaults: Default::default(),
                retained_expressions: Vec::new(),
                enum_value_renames: loaded.hints.enum_value_renames.clone(),
                enum_comments: Default::default(),
                domain_array_holders: Default::default(),
                enum_dependents: Default::default(),
                // Nothing is created here: the packages are the prerequisites.
                extension_requires: Default::default(),
                extension_drops: Default::default(),
                extension_updates: Default::default(),
                extension_moves: Default::default(),
                strategies: Default::default(),
                annotations: loaded.hints.annotations.clone(),
                // Carried for the same reason, and one more: it is what decides
                // whether the routines this bootstrap creates arrive executable
                // by every principal in the cluster (ADR-0010 §5). Dropping it
                // here would make the first apply into an empty database the one
                // apply that leaves the engine default standing.
                public_execute: loaded.hints.public_execute.clone(),
            },
        )
        .map_err(|errs| {
            let errors = crate::Unexpressible(errs);
            errors.print_human();
            errors
        })?;
        // What a new table calls goes ahead of it, by the same reordering passes
        // `plan --db` runs (#1585, DEC-942.1, DEC-1364.1): a default or generated
        // column calling a declared function. Into an empty database there is no
        // catalog to read, so the facts are none: nothing is dropped or rebuilt,
        // no column is released, and `weave` has nothing to add. The passes that
        // read nothing run, so the `--sql` script gets them too (#1955).
        let cs = crate::dependents::schedule::schedule(
            cs,
            &crate::dependents::schedule::Declarations {
                schema: &loaded.schema,
                sources: crate::dependents::Sources {
                    before: pbps_diff::Side {
                        schema: &Schema::default(),
                        ids: &IdsFile::default(),
                    },
                    after: &ids,
                },
                annotations: &loaded.hints.annotations,
            },
            &crate::dependents::schedule::Facts::default(),
            dialect.as_ref(),
        )?
        .changes;
        // And the check `plan --db` runs on that final order (DEC-1576.1): an
        // expression naming, in a literal, a relation created only after it is
        // refused with the two-plan remedy, before any script or DDL, rather
        // than rolled back by the engine. A qualified name is refused offline.
        // An unqualified one may be found ahead on its path, and which name an
        // unnamed key's index takes depends on what holds its candidates now
        // (#1619, #1645), which only the target can say: asked of it with
        // `--db`, before any script is written, and refused without one. One
        // refusal names them all.
        let mut refused = Vec::new();
        if dialect.name() == "postgres" {
            let offline = crate::dependents::names_a_later_relation(
                &cs,
                dialect.as_ref(),
                None,
                &Schema::default(),
            );
            let prefixes = crate::dependents::key_name_prefixes(&cs);
            match target {
                Some(target)
                    if !prefixes.is_empty() || offline.iter().any(|n| !n.searched.is_empty()) =>
                {
                    Box::pin(async {
                        let mut conn = db::connect(target).await?;
                        let holders = pbps_pg::catalog::key_name_holders(&mut conn, &prefixes)
                            .await
                            .map_err(|e| anyhow::anyhow!("reading what holds key names: {e}"))?;
                        for name in crate::dependents::names_a_later_relation(
                            &cs,
                            dialect.as_ref(),
                            Some(&holders),
                            &Schema::default(),
                        ) {
                            if name.searched.is_empty()
                                || !crate::engine::resolves_now(&mut conn, &name).await?
                            {
                                refused.push(name);
                            }
                        }
                        Ok::<_, anyhow::Error>(())
                    })
                    .await?;
                }
                Some(_) | None => refused = offline,
            }
        }
        crate::dependents::later_relation_refusal(&refused)
            .map_err(|why| anyhow::anyhow!("module_dependents (PostgreSQL): {why}"))?;

        // Rendered now; with a target, written as the build's last step inside
        // its transaction, after the ledger row: a script written first
        // overwrote the file with one the same command then refused (#1684).
        // Refusals come before the lock, under it (two roles the collation reads
        // as one name, DECISIONS 123) and after the build (a definer routine
        // without a pinned path, #322), so only the end of the build is past all
        // of them. A write that fails there rolls the build back with it.
        //
        // Emitted now and rendered at the write: the pins carry the zone the
        // build ran in, which a target tells only once connected (DEC-1916.1).
        let script = sql_out
            .map(|_| {
                crate::statements(
                    &cs.clone()
                        .ordered(pbps_model::PlanMode::Transactional, dialect.as_ref()),
                    &dialect,
                )?
                .bootstrap_script(&loaded.schema, &project.config.trusted_roles)
            })
            .transpose()?;
        let write_script = |zone: &SessionZone| -> anyhow::Result<()> {
            if let (Some(path), Some(statements)) = (sql_out, &script) {
                let script = crate::sql_script(statements, "an empty database", zone)?;
                std::fs::write(path, script)
                    .with_context(|| format!("cannot write `{}`", path.display()))?;
                println!("wrote {}", path.display());
            }
            Ok(())
        };

        let Some(target) = target else {
            if sql_out.is_none() {
                bail!(
                    "bootstrap needs somewhere to go: pass --sql {}, --db or --env",
                    crate::report::placeholder("file")
                );
            }
            // No target to ask: UTC until #1918 (DEC-1916.1).
            return write_script(&SessionZone::offline_utc()).map(|()| services::Bootstrap::Script);
        };

        let statements = crate::statements(
            &cs.clone()
                .ordered(pbps_model::PlanMode::Transactional, dialect.as_ref()),
            &dialect,
        )?;

        Box::pin(async {
            let mut conn = db::connect(target).await?;
            let bootstrap_pins =
                bootstrap_type_pins(&mut conn, &loaded.schema, &cs, project).await?;
            // A collation the server lacks is refused here, by name, rather than
            // by the `CREATE TABLE` that names it after the ones before it ran
            // (#1175).
            crate::engine::refuse_unknown_collations(&mut conn, &loaded.schema).await?;
            // A key to a declared history under another spelling (#1625).
            crate::engine::refuse_history_references_alike(
                &mut conn,
                &loaded.schema,
                dialect.as_ref(),
            )
            .await?;
            // A history retention or a cascading key the server lacks (#1502).
            crate::engine::refuse_unsupported_temporal(&mut conn, &cs).await?;
            crate::engine::permission_support(&mut conn, &cs, &ids).await?;
            crate::engine::lock(&mut conn, operator).await?;
            let mut transaction_attempted = false;
            let result = async {
                refuse_wrongly_spelt_schemas(&mut conn, &loaded.schema).await?;

                // The empty-target check is protected by the same lock as the
                // build. Checking before it would let two bootstraps both observe
                // emptiness, then let the second continue after the first had
                // committed and released the lock.
                //
                // No rows are read here: the question is whether any declared
                // object stands, and a table that does is refused before its rows
                // matter.
                let existing = managed_state_full(
                    &mut conn,
                    &ids,
                    &declared_modules,
                    &pbps_diff::external_parents(&loaded.schema),
                    target.catalog_policy(project.config.unmanaged),
                    &BTreeMap::new(),
                    crate::engine::Read::Snapshot,
                    warnings,
                )
                .await?;

                // Unsupported grants are outside role.grants, so an apparently
                // empty role is not proof that the database is empty (110).
                refuse_unexpressible(&existing.scoped, &target.label, "bootstrap again")?;
                crate::engine::refuse_missing_cluster_roles(
                    conn.driver(),
                    &existing.scoped.missing_roles,
                )?;
                // This command builds what it grants on, so the role running it
                // owns every object the declarations name. A grant to that role
                // is the impossible line a connected plan is refused for, and
                // here there is nothing downstream to catch it: the engine writes
                // only the owner's default ACL entry, the pull reads that entry
                // as the zero point (DECISIONS 371), and this command compares no
                // declared grant against what it built. Unrefused it reports
                // success and records a snapshot that does not hold the grant
                // (#261).
                crate::engine::owned_targets(
                    conn.driver(),
                    &cs,
                    &ids,
                    &existing.scoped.owners,
                    &existing.scoped.session_role,
                )?;

                // Only names the plan creates need to be free (DECISIONS 118).
                // PostgreSQL grants use existing cluster roles; their existence
                // is outside this database's ownership (DECISIONS 211).
                let (wanted, vacated) = role_name_expectations(
                    &cs.clone()
                        .ordered(pbps_model::PlanMode::Transactional, dialect.as_ref()),
                    dialect.as_ref(),
                    pbps_model::PlanMode::Transactional,
                    0,
                )?;
                refuse_taken_role_names(&mut conn, &wanted, &vacated).await?;
                // A history name already taken: SQL Server takes an existing
                // table whose columns match as the history, with its rows,
                // instead of refusing (measured on 17.0, #1176).
                refuse_taken_history_names(&mut conn, &cs, &target.label).await?;

                refuse_extension_prerequisite_drift(
                    &existing.scoped.schema,
                    &loaded.schema,
                    &ids,
                    dialect.as_ref(),
                    "bootstrap",
                )?;
                refuse_occupied_bootstrap(
                    &existing.scoped.schema,
                    &loaded.schema,
                    &existing.limitations,
                    dialect.as_ref(),
                    &target.label,
                )?;

                crate::engine::recheck_type_prerequisites(
                    &mut conn,
                    &bootstrap_pins,
                    &cs,
                    &project.config.trusted_roles,
                    false,
                )
                .await?;
                transaction_attempted = true;
                execute_transaction_body(
                    &mut conn,
                    dialect.as_ref(),
                    &statements,
                    &bootstrap_pins,
                    &cs,
                    &project.config.trusted_roles,
                )
                .await?;
                // Bootstrap creates routines as `apply` does, so the same form is
                // required of a definer among them (#322).
                crate::engine::refuse_unsafe_definers(
                    &mut conn,
                    &crate::engine::written_routines(&cs),
                    crate::engine::Absent::Refused,
                )
                .await?;

                // Read-back and the success ledger row are part of the same
                // transaction as the DDL. If either fails, the database is still
                // empty and bootstrap can be retried honestly.
                //
                // The state recorded is what the engine actually built, read back —
                // not what was declared. Expressions come back in the engine's
                // stored form, and only that form compares equal on the next drift
                // check (SPEC §8.2). The rows too, under the declared scope:
                // bootstrap inserted them, and the engine's spelling of each value
                // is what the record has to hold.
                let built = managed_state(
                    &mut conn,
                    &ids,
                    &declared_modules,
                    &pbps_diff::external_parents(&loaded.schema),
                    target.catalog_policy(project.config.unmanaged),
                    &loaded.schema.data_scopes(),
                    &loaded.schema,
                    crate::engine::Read::InsideOwnTransaction,
                    warnings,
                )
                .await?;
                // A database-side trigger can add a privilege during the build.
                // Never commit a snapshot that silently omits it (110, 147).
                refuse_unexpressible(&built, &target.label, "bootstrap again")?;
                refuse_extension_prerequisite_drift(
                    &built.schema,
                    &loaded.schema,
                    &ids,
                    dialect.as_ref(),
                    "bootstrap",
                )?;
                crate::engine::refuse_missing_cluster_roles(conn.driver(), &built.missing_roles)?;
                // And it can take one back. `ddl_command_end` fires on `GRANT`
                // and on `REVOKE` (measured), so a trigger already in the
                // database can reverse what this build just settled about
                // `PUBLIC` before this read — and no other check here would see
                // it, because what `PUBLIC` holds is in neither the schema nor
                // the unexpressible list (DECISIONS 371, 517). The same
                // postcondition the apply paths ask, at the read this command
                // records.
                refuse_unmet_public_execution(&cs, &built, &target.label)?;
                // Only the uids of what this build made. The ids file may name
                // tables, types and roles these declarations do not hold; recording
                // those as held would make the next plan skip them (#1600, #1606).
                crate::engine::prove_tables_absent(&mut conn, &built.missing).await?;
                let gone = crate::engine::absent_roles(&mut conn, &built.missing_roles).await?;
                let mut held = ids
                    .without(&built.missing)
                    .without_roles(&gone)
                    .without_types(&built.missing_types);
                held.extensions.clone_from(&prerequisite_ids.extensions);
                crate::engine::recheck_type_prerequisites(
                    &mut conn,
                    &bootstrap_pins,
                    &cs,
                    &project.config.trusted_roles,
                    true,
                )
                .await?;
                let snapshot = with_provenance(
                    project.root(),
                    bootstrap_record(built, held, &loaded, operator)?,
                );
                let id = crate::engine::record(&mut conn, &snapshot).await?;
                write_script(&pin_zone(&conn))?;
                Ok::<_, anyhow::Error>((id, snapshot))
            }
            .await;
            let result = if transaction_attempted {
                finish_transaction(&mut conn, dialect.as_ref(), result).await
            } else {
                result
            };
            if transaction_attempted && let Err(error) = &result {
                record_failed_bootstrap(&mut conn, project.root(), operator, error).await;
            }
            let unlocked = crate::engine::unlock(&mut conn).await;
            let (id, snapshot) = match result {
                Ok(built) => built,
                Err(error) => {
                    return Ok(services::Bootstrap::Failed {
                        error,
                        released: unlocked,
                    });
                }
            };
            Ok(services::Bootstrap::Built(services::Bootstrapped {
                entry: id,
                tables: snapshot.schema.tables.len(),
                target: target.label.clone(),
                released: unlocked,
            }))
        })
        .await
    })
}

/// Best-effort audit row for a bootstrap whose transactional work was rolled
/// back. The empty state was established under the lock before the build, so it is
/// the last trustworthy state to carry forward; the declared identity mapping
/// has not become an environment mapping until the build succeeds.
///
/// `reason` goes through [`crate::engine::ledger_safe_reason`] for the same
/// reason `failed_apply_snapshot`'s does: bootstrap fails the same way apply
/// does, through the same `execute_statements` (DECISIONS 455).
pub(super) async fn record_failed_bootstrap(
    conn: &mut Conn,
    root: &std::path::Path,
    operator: &str,
    error: &anyhow::Error,
) {
    let mut failed = StateSnapshot::new(
        StateKind::Failed,
        Schema::default(),
        IdsFile::default(),
        operator,
    );
    failed.git_sha = db::git_sha(root);
    failed.reason = Some(
        crate::engine::truncate_reason(conn.driver(), &crate::engine::ledger_safe_reason(error))
            .into(),
    );
    match crate::engine::record(conn, &failed).await {
        Ok(id) => eprintln!("Bootstrap failure recorded as ledger entry #{id}."),
        Err(audit_error) => eprintln!(
            "warning: the failed bootstrap could not be added to the ledger: {audit_error}"
        ),
    }
}
