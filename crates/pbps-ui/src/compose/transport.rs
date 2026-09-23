//! One admitted endpoint, one ref, and no effective HTTP redirect following.

use super::{
    Error, Result, destination,
    git::Git,
    process, random_id,
    record::{Description, oid},
    refs::{self, RefEvidence},
};

fn command(git: &Git, description: &Description) -> Result<(std::process::Command, String)> {
    if destination::resolve(git, &description.remote)? != description.destination {
        return Err(Error::new(
            "Compose destination changed; review a new candidate",
        ));
    }
    let endpoint = description.destination.endpoint()?;
    let alias = format!("pbps-compose-{}", random_id()?);
    let mut command = git.command();
    // A fresh invocation-only alias cannot inherit another configured remote's
    // fan-out, mirror or receive-pack settings. Only public identity is held.
    let settings = [
        (format!("remote.{alias}.url"), endpoint.clone()),
        (format!("remote.{alias}.pushurl"), endpoint.clone()),
        // Push options can trigger server actions outside the reviewed change.
        // An empty high-priority value clears Git's inherited multi-value list.
        ("push.pushOption".into(), String::new()),
        ("http.followRedirects".into(), "false".into()),
        (format!("http.{endpoint}.followRedirects"), "false".into()),
        (
            format!("http.{}/.followRedirects", endpoint.trim_end_matches('/')),
            "false".into(),
        ),
    ];
    command.env("GIT_CONFIG_COUNT", settings.len().to_string());
    for (n, (key, value)) in settings.into_iter().enumerate() {
        command
            .env(format!("GIT_CONFIG_KEY_{n}"), key)
            .env(format!("GIT_CONFIG_VALUE_{n}"), value);
    }
    Ok((command, alias))
}

fn local_evidence(git: &Git, description: &Description, reference: &str) -> Option<RefEvidence> {
    description.destination.local_repository().map(|root| {
        refs::observe(
            &Git {
                root: root.to_path_buf(),
                hooks: git.hooks.clone(),
                deadline: git.deadline,
            },
            reference,
        )
    })
}

pub(super) fn observe(git: &Git, description: &Description, reference: &str) -> RefEvidence {
    let read = || -> Result<RefEvidence> {
        let (mut command, alias) = command(git, description)?;
        // Git's advertisement omits dangling symrefs. A zero-value lease can
        // dereference one, so local destinations also require direct evidence.
        let local = local_evidence(git, description, reference);
        if matches!(local, Some(RefEvidence::Symbolic | RefEvidence::Unreadable)) {
            return Ok(local.unwrap());
        }
        command.args(["ls-remote", "--symref", "--refs", &alias, reference]);
        let output = process::run(command, &[], git.deadline)?;
        if !output.status.success() {
            return Ok(RefEvidence::Unreadable);
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|_| Error::new("Unreadable remote ref advertisement"))?;
        let mut found = None;
        for line in text.lines() {
            let Some((value, name)) = line.split_once('\t') else {
                return Ok(RefEvidence::Unreadable);
            };
            if name != reference {
                return Ok(RefEvidence::Unreadable);
            }
            if value.starts_with("ref: ") {
                return Ok(RefEvidence::Symbolic);
            }
            if !oid(value) || found.is_some() {
                return Ok(RefEvidence::Unreadable);
            }
            found = Some(RefEvidence::Direct(value.into()));
        }
        // Network absence relies on the server's ordinary direct-branch
        // contract, not proof against hidden remapping (DECISIONS 534).
        let advertised = found.unwrap_or(RefEvidence::Absent);
        if local.is_some_and(|local| local != advertised) {
            return Ok(RefEvidence::Unreadable);
        }
        Ok(advertised)
    };
    read().unwrap_or(RefEvidence::Unreadable)
}

/// Git's own `push.gpgSign` modes. Read once and passed to Git explicitly, so
/// the preflight and the push obey the same requirement (#775).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PushSigning {
    Never,
    IfAsked,
    Required,
}

impl PushSigning {
    fn flag(self) -> &'static str {
        match self {
            Self::Never => "--signed=no",
            Self::IfAsked => "--signed=if-asked",
            Self::Required => "--signed=yes",
        }
    }
}

pub(super) fn push_signing(git: &Git) -> Result<PushSigning> {
    // One read: a second `git config` would let a concurrent writer change
    // the value between them (#825). Git normalizes boolean spellings,
    // including a bare key, and returns any other string verbatim.
    let answer = git.output(
        &["config", "--type=bool-or-str", "--get", "push.gpgSign"],
        &[],
        None,
    )?;
    match answer.status.code() {
        Some(0) => match super::git::text(answer.stdout)?.as_str() {
            "true" => Ok(PushSigning::Required),
            "false" => Ok(PushSigning::Never),
            value if value.eq_ignore_ascii_case("if-asked") => Ok(PushSigning::IfAsked),
            // Anything else is a configuration Git itself refuses, not a
            // weaker requirement.
            _ => Err(Error::new("Unusable push signing policy")),
        },
        Some(1) if answer.stdout.is_empty() && answer.stderr.is_empty() => Ok(PushSigning::Never),
        _ => Err(Error::new("Could not determine push signing policy")),
    }
}

pub(super) enum Preflight {
    Unsupported,
    Unavailable,
}

/// A required push certificate the destination cannot accept refuses before
/// any remote attempt is recorded. A dry run sends no commands, so neither
/// probe can write; `--signed=if-asked` succeeding where `yes` failed isolates
/// the capability as the reason without parsing localized diagnostics. Git
/// signs only in the real push, so a failing signer surfaces there, after the
/// attempt, as uncertain delivery.
pub(super) fn preflight(
    git: &Git,
    description: &Description,
    commit: &str,
    signing: PushSigning,
) -> std::result::Result<(), Preflight> {
    if signing != PushSigning::Required {
        return Ok(());
    }
    let dry = |mode| run_push(git, description, commit, mode, true).is_ok();
    if dry(PushSigning::Required) {
        Ok(())
    } else if dry(PushSigning::IfAsked) {
        Err(Preflight::Unsupported)
    } else {
        Err(Preflight::Unavailable)
    }
}

pub(super) fn push(
    git: &Git,
    description: &Description,
    commit: &str,
    signing: PushSigning,
) -> Result<()> {
    run_push(git, description, commit, signing, false)
}

fn run_push(
    git: &Git,
    description: &Description,
    commit: &str,
    signing: PushSigning,
    dry_run: bool,
) -> Result<()> {
    let (mut command, alias) = command(git, description)?;
    if local_evidence(git, description, &description.output_ref)
        .is_some_and(|evidence| evidence != RefEvidence::Absent)
    {
        return Err(Error::new(
            "The local destination ref is not directly absent",
        ));
    }
    command.args([
        "push",
        "--porcelain",
        "--no-verify",
        "--no-follow-tags",
        "--recurse-submodules=no",
        signing.flag(),
    ]);
    if dry_run {
        command.arg("--dry-run");
    }
    command
        .arg(format!("--force-with-lease={}:", description.output_ref))
        .arg(alias)
        .arg(format!("{commit}:{}", description.output_ref));
    let result = process::run(command, &[], git.deadline)?;
    if result.status.success() {
        Ok(())
    } else {
        Err(Error::new(
            "The remote publication acknowledgement is unavailable",
        ))
    }
}
