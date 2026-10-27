//! Native setup-connection work inside reconstruction compile ranges.
use super::*;
use pbps_mssql::resolver::reconstruct::AuxiliaryStep;

pub(super) struct Chaining {
    // The producer fixed this key before preparation; late stages never reload it.
    pub key: pbps_db::fingerprint::EnvironmentFingerprintKey,
    pub pending: BTreeSet<capture::ObjectIdentity>,
    pub managed_ids: BTreeSet<capture::ObjectIdentity>,
    pub deferred: Option<String>,
    pub managed: Vec<pbps_model::Schema>,
    pub context: auth::AuthorizationContext,
    pub token: String,
}

pub(super) async fn execute(
    binding: &Binding,
    prepared: &mut Prepared,
    target: &mut Conn,
    at: usize,
) -> Result<(), Error> {
    let kind = binding
        .reconstruction
        .auxiliary(at)
        .expect("an auxiliary executor is selected");
    if let AuxiliaryStep::Rendered(id) = kind
        && prepared.staged.compared().contains(id)
    {
        return Ok(());
    }
    let mut setup = prepared.reach.open().await?;
    let before = externals::inventory(&mut setup, &prepared.reach.sandbox)
        .await
        .map_err(read)?;
    let schemas = prepared
        .chaining
        .context
        .schemas
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let authority = auth::read(&mut prepared.compile, &schemas)
        .await
        .map_err(read)?;
    let result = match kind {
        AuxiliaryStep::Rendered(_) => binding
            .reconstruction
            .compile_external(&mut setup, at)
            .await
            .map_err(compile_failed),
        AuxiliaryStep::Deferred => {
            let script = prepared
                .chaining
                .deferred
                .as_deref()
                .expect("deferred step has its file section");
            run_deferred(&mut setup, script).await
        }
    };
    // Read even after a partially successful deferred file; the complete
    // inventory joins the run's cleanup obligation before returning its error.
    let after = externals::inventory(&mut setup, &prepared.reach.sandbox)
        .await
        .map_err(read)?;
    let created = externals::created(&before, &after);
    prepared.inventory = after;
    result?;
    // Inventory uses physical names. Resolve the new roots under the pinned
    // key before asking the target, whose generated names can be different.
    let canonical = capture::capture_declared(
        &mut setup,
        &capture::CaptureScope {
            retained: created.clone(),
            candidates: BTreeSet::new(),
        },
        &binding.declarations,
        &prepared.chaining.key,
    )
    .await
    .map_err(capture_error)?
    .scope()
    .retained
    .clone();
    for id in &canonical {
        prepared.chaining.pending.remove(id);
    }
    let mut roots = prepared.staged.scope().retained.clone();
    roots.extend(canonical.iter().cloned());
    let scope = capture::CaptureScope {
        retained: roots,
        candidates: BTreeSet::new(),
    };
    let captured = capture::capture_declared(
        target,
        &scope,
        &binding.declarations,
        &prepared.chaining.key,
    )
    .await
    .map_err(capture_error)?;
    let context = &mut prepared.chaining.context;
    let target_owners =
        auth::OwnerPartition::for_staging(&captured, context, &prepared.chaining.managed_ids)
            .map_err(|e| Error::Externals(vec![format!("{e:?}")]))?;
    auth::read_staged_permissions(target, context, &target_owners)
        .await
        .map_err(read)?;
    let scope::Principals::Mssql(map) = &mut prepared.map else {
        unreachable!("MSSQL preparation")
    };
    map.extend_staged(&mut setup, context, &prepared.chaining.token)
        .await
        .map_err(read)?;
    if matches!(kind, AuxiliaryStep::Rendered(_)) {
        auth::reconstruct_owners(&mut setup, map, &target_owners.restricted_to(&canonical))
            .await
            .map_err(read)?;
    }
    let mut local = context.clone();
    if !context.principal.superuser {
        local.principal.effective = map
            .deployer(context)
            .ok_or_else(|| Error::Scope("no mapped deployer".into()))?;
    }
    let current = capture::capture_declared(
        &mut setup,
        &scope,
        &binding.declarations,
        &prepared.chaining.key,
    )
    .await
    .map_err(capture_error)?;
    let current_owners =
        auth::OwnerPartition::for_staging(&current, &local, &prepared.chaining.managed_ids)
            .map_err(|e| Error::Externals(vec![format!("{e:?}")]))?;
    target_owners.verify(&current_owners).map_err(read)?;
    auth::replay_staged_permissions(&mut setup, map, context, &target_owners, &current_owners)
        .await
        .map_err(read)?;
    if auth::read(&mut prepared.compile, &schemas)
        .await
        .map_err(read)?
        != authority
    {
        return Err(Error::Externals(vec![
            "deferred externals changed the deployer's authorization".into(),
        ]));
    }
    let managed = prepared.chaining.managed.iter().collect::<Vec<_>>();
    let accepted = capture::compare_staged_pending(
        &mut setup,
        target,
        &created,
        &managed,
        &binding.declarations,
        &prepared.chaining.key,
        &prepared.chaining.pending,
    )
    .await
    .map_err(stage_error)?;
    prepared.staged.extend(accepted).map_err(Error::Externals)?;
    Ok(())
}

async fn run_deferred(setup: &mut Conn, script: &str) -> Result<(), Error> {
    for (index, batch) in externals::batches(script).map_err(read)?.iter().enumerate() {
        setup.execute(batch).await.map_err(|error| {
            Error::Externals(vec![format!(
                "deferred externals statement {} failed: {error}",
                index + 1
            )])
        })?;
    }
    Ok(())
}
