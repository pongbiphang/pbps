//! Admitted public destination identity, never a redacted secret-bearing URL.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{Error, Result, git::Git, record::RepositoryIdentity};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    transport: String,
    host: Option<String>,
    port: Option<u16>,
    principal: Option<String>,
    repository: String,
    repository_identity: Option<RepositoryIdentity>,
}

fn simple(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

impl Destination {
    pub(super) fn parse(value: &str) -> Result<Self> {
        let refused = || {
            Error::new(
                "Compose needs a credential-free, supported destination; configure a public endpoint and an approved credential helper",
            )
        };
        if value.is_empty() || value.chars().any(char::is_control) || value.contains(['?', '#']) {
            return Err(refused());
        }
        let local = value.strip_prefix("file://").unwrap_or(value);
        if Path::new(local).is_absolute() {
            let path = Path::new(local).canonicalize().map_err(|_| refused())?;
            let repository = path.to_str().ok_or_else(refused)?.to_owned();
            return Ok(Self {
                transport: "file".into(),
                host: None,
                port: None,
                principal: None,
                repository,
                repository_identity: None,
            });
        }
        let (transport, authority, repository) =
            if let Some((scheme, rest)) = value.split_once("://") {
                if !matches!(scheme, "http" | "https" | "ssh") {
                    return Err(refused());
                }
                let (authority, rest) = rest.split_once('/').ok_or_else(refused)?;
                (scheme, authority, format!("/{rest}"))
            } else {
                let (authority, repository) = value.split_once(':').ok_or_else(refused)?;
                ("scp", authority, repository.to_owned())
            };
        if repository.is_empty() || repository.chars().any(char::is_whitespace) {
            return Err(refused());
        }
        let (principal, authority) = match authority.split_once('@') {
            Some((user, host)) if matches!(transport, "ssh" | "scp") && simple(user) => {
                (Some(user.to_owned()), host)
            }
            Some(_) => return Err(refused()),
            None if matches!(transport, "ssh" | "scp") => return Err(refused()),
            None => (None, authority),
        };
        // The first qualified forms use DNS/IPv4 hosts. Ambiguous IPv6/scp
        // parsing is a named unsupported form, not guessed normalization.
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (
                host,
                Some(
                    port.parse::<u16>()
                        .ok()
                        .filter(|p| *p != 0)
                        .ok_or_else(refused)?,
                ),
            ),
            None => (authority, None),
        };
        if !simple(host) {
            return Err(refused());
        }
        Ok(Self {
            transport: transport.to_owned(),
            host: Some(host.to_owned()),
            port,
            principal,
            repository,
            repository_identity: None,
        })
    }
}

impl Destination {
    pub(super) fn local_repository(&self) -> Option<&Path> {
        (self.transport == "file").then(|| Path::new(&self.repository))
    }

    pub(super) fn endpoint(&self) -> Result<String> {
        let host = self.host.as_deref().unwrap_or_default();
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        let principal = self.principal.as_deref().unwrap_or_default();
        match self.transport.as_str() {
            "file" => Ok(self.repository.clone()),
            "http" | "https" => Ok(format!(
                "{}://{host}{port}{}",
                self.transport, self.repository
            )),
            "ssh" => Ok(format!("ssh://{principal}@{host}{port}{}", self.repository)),
            "scp" if self.port.is_none() => Ok(format!("{principal}@{host}:{}", self.repository)),
            _ => Err(Error::new("Unsupported compose destination identity")),
        }
    }
}

pub(super) fn validate(destination: &Destination) -> Result<()> {
    // Loaded evidence must remain readable when a local remote is offline.
    // Live resolution separately checks identity before contacting it.
    if destination.transport == "file" {
        return if Path::new(&destination.repository).is_absolute()
            && destination.host.is_none()
            && destination.port.is_none()
            && destination.principal.is_none()
            && destination
                .repository_identity
                .as_ref()
                .is_some_and(|identity| {
                    identity.source == Path::new(&destination.repository)
                        && identity.common.is_absolute()
                })
            && !destination.repository.chars().any(char::is_control)
        {
            Ok(())
        } else {
            Err(Error::new("Invalid local compose destination"))
        };
    }
    if Destination::parse(&destination.endpoint()?)? != *destination {
        return Err(Error::new("Invalid compose destination identity"));
    }
    Ok(())
}

pub(super) fn resolve(git: &Git, remote: &str) -> Result<Destination> {
    if !simple(remote) || remote.starts_with('-') {
        return Err(Error::new("Select a configured remote name"));
    }
    let rewrites = git.output(
        &[
            "config",
            "--get-regexp",
            "^url\\..*\\.(insteadof|pushinsteadof)$",
        ],
        &[],
        None,
    )?;
    if rewrites.status.code() != Some(1)
        || !rewrites.stdout.is_empty()
        || !rewrites.stderr.is_empty()
    {
        return Err(Error::new(
            "Compose requires a destination without Git URL rewrite rules",
        ));
    }
    let value = git.line(&["remote", "get-url", "--push", "--all", remote])?;
    if value.lines().count() != 1 {
        return Err(Error::new("Compose requires exactly one push destination"));
    }
    let mut destination = Destination::parse(&value)?;
    if destination.transport == "file" {
        // A path can be reused for another repository with the same base tip.
        // Bind both its directory and effective Git common directory; a normal
        // checkout can keep its inode while its .git directory is replaced.
        destination.repository_identity = Some(RepositoryIdentity::capture(&Git {
            root: destination.repository.clone().into(),
            hooks: git.hooks.clone(),
            deadline: git.deadline,
        })?);
    }
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_refused_without_becoming_error_or_identity_data() {
        for value in [
            "https://FAKE_SECRET@host/repo",
            "https://host/repo?token=FAKE_SECRET",
            "ssh://git:FAKE_SECRET@host/repo",
            "https://host/repo#FAKE_SECRET",
        ] {
            let error = Destination::parse(value).unwrap_err();
            assert!(!error.to_string().contains("FAKE_SECRET"));
        }
        assert!(Destination::parse("ssh://host/repo").is_err());
        assert!(Destination::parse("ext::anything").is_err());
        let first = Destination::parse("git@host:team/repo.git").unwrap();
        let second = Destination::parse("alice@host:team/repo.git").unwrap();
        assert_ne!(first, second);
        assert!(Destination::parse("https://host/team/repo.git").is_ok());
    }
}
