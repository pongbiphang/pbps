//! The push of ADR-0015 decision 5: bounded to one commit, to it by name, and
//! to one destination.
//!
//! A remote may carry several push URLs, and `git push` sends to every one of
//! them while `ls-remote` and the lease each speak to one (**measured**: two
//! `pushurl`s, two destinations listed, and a push reaching both — so a lease
//! can hold at the first and fail at the second *after* the first has
//! published). So the UI requires exactly one URL and uses that URL, not the
//! remote's name: the name resolves to the fetch URL for `ls-remote` and to
//! the push URL for `push`, and a `pushurl` that differs would have the checks
//! look at one server and the push go to another.
//!
//! The URL never reaches a command line. A URL may carry a credential and `git
//! remote get-url` prints one verbatim (**measured**: `https://tok3n@…` came
//! back as typed), while a command line is readable by every user of the
//! machine through `/proc/<pid>/cmdline` and the environment by the process's
//! owner alone. So it is handed to `git` as a remote that exists only in the
//! environment of the two processes that use it, and every URL the page is
//! shown is not the URL with something taken out of it but a URL *rebuilt*
//! from the three parts that name the destination.

use std::collections::BTreeMap;
use std::ffi::OsString;

use super::git::Git;

#[derive(Debug)]
pub enum PushRefusal {
    /// More than one push URL, or none.
    NotOneDestination {
        urls: Vec<String>,
    },
    /// A rewrite rule would make the URL the checks spoke to and the URL the
    /// push goes to two different strings.
    Rewritten {
        rule: String,
        url: String,
    },
    /// The environment-only remote's name is already configured, where an
    /// entry would *add* to it rather than shadow it.
    NameTaken(String),
    /// The local branch is ahead of the remote's. A refspec bounds the
    /// destination ref, not the range (**measured**: a branch one unrelated
    /// commit ahead had that commit published under the intent commit).
    Unpushed {
        branch: String,
        local: String,
        remote: String,
    },
    Git(String),
}

impl std::fmt::Display for PushRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOneDestination { urls } => {
                if urls.is_empty() {
                    write!(f, "that remote has no push URL")
                } else {
                    write!(
                        f,
                        "that remote pushes to {} destinations ({}); compose publishes to one",
                        urls.len(),
                        urls.join(", ")
                    )
                }
            }
            Self::Rewritten { rule, url } => write!(
                f,
                "`{rule}` rewrites `{url}` on use, so the URL checked and the URL pushed to \
                 would be two different servers"
            ),
            Self::NameTaken(name) => write!(
                f,
                "a remote named `{name}` is already configured; compose needs a name of its own"
            ),
            Self::Unpushed {
                branch,
                local,
                remote,
            } => write!(
                f,
                "{branch} is at {local} here and {remote} on the remote; \
                 push what you already have before composing"
            ),
            Self::Git(detail) => write!(f, "{detail}"),
        }
    }
}

/// A destination the compose has resolved and checked.
#[derive(Debug, Clone)]
pub struct Destination {
    /// Never shown, never on a command line.
    url: String,
    /// `pbps-ui-<random>`, drawn per launch.
    name: String,
}

impl Destination {
    /// What the page is shown: rebuilt from the scheme, the host with its
    /// port, and the path. Userinfo, query and fragment are not carried,
    /// because each of them can hold a credential and only the first announces
    /// itself with an `@` — `https://host/repo.git?access_token=…` has no
    /// userinfo at all, and `git`'s transport prints a query verbatim.
    pub fn shown(&self) -> String {
        rebuild(&self.url)
    }

    fn environment(&self) -> BTreeMap<String, OsString> {
        BTreeMap::from([
            ("GIT_CONFIG_COUNT".to_owned(), OsString::from("1")),
            (
                "GIT_CONFIG_KEY_0".to_owned(),
                OsString::from(format!("remote.{}.url", self.name)),
            ),
            ("GIT_CONFIG_VALUE_0".to_owned(), OsString::from(&self.url)),
        ])
    }

    /// Replace every URL-shaped run in a line of `git` output with the
    /// rebuilding, so the page never shows a string the UI did not construct.
    pub fn safe(&self, output: &[u8]) -> String {
        redact(&String::from_utf8_lossy(output))
    }
}

/// Choose the destination for a remote, with every refusal decision 5 names.
pub fn destination(git: &Git, remote: &str, name: String) -> Result<Destination, PushRefusal> {
    // A remote's `url` is multi-valued and an environment entry *adds* to a
    // name the configuration already has rather than shadowing it
    // (**measured**: with a remote of that name already configured,
    // `get-url --all` listed both URLs and one push published to both).
    let taken = git
        .run(&[
            "config",
            "--get-regexp",
            &format!("^remote\\.{}\\.", regex_quote(&name)),
        ])
        .map_err(|e| PushRefusal::Git(e.to_string()))?;
    if taken.ok() {
        return Err(PushRefusal::NameTaken(name));
    }

    let listed = git
        .run(&["remote", "get-url", "--push", "--all", remote])
        .map_err(|e| PushRefusal::Git(e.to_string()))?;
    if !listed.ok() {
        return Err(PushRefusal::Git(
            String::from_utf8_lossy(&listed.stderr).trim().to_owned(),
        ));
    }
    let urls: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    let [url] = urls.as_slice() else {
        return Err(PushRefusal::NotOneDestination {
            urls: urls.iter().map(|u| rebuild(u)).collect(),
        });
    };

    // `git remote get-url --push` has applied one round of rewrite rules
    // already, and the environment-only remote's URL is rewritten *again* on
    // use by any rule whose value is a prefix of it (**measured**: with
    // `pushInsteadOf` rules chaining `src` to `mid` and `mid` to `fin`,
    // `get-url --push` answered `mid`, `ls-remote` spoke to `mid`, and the
    // push published to `fin`).
    let rules = git
        .run(&["config", "--get-regexp", "^url\\..*\\.(push)?insteadof$"])
        .map_err(|e| PushRefusal::Git(e.to_string()))?;
    if rules.ok() {
        for line in String::from_utf8_lossy(&rules.stdout).lines() {
            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            if !value.is_empty() && url.starts_with(value) {
                return Err(PushRefusal::Rewritten {
                    rule: key.to_owned(),
                    url: rebuild(url),
                });
            }
        }
    }

    Ok(Destination {
        url: url.clone(),
        name,
    })
}

/// The remote's tip for one branch, or `None` where the remote does not have
/// the branch yet.
pub fn remote_tip(
    git: &Git,
    destination: &Destination,
    branch: &str,
) -> Result<Option<String>, PushRefusal> {
    let answer = git
        .run_with_environment(
            &destination.environment(),
            &["ls-remote", &destination.name, branch],
        )
        .map_err(|e| PushRefusal::Git(e.to_string()))?;
    if !answer.ok() {
        return Err(PushRefusal::Git(destination.safe(&answer.stderr)));
    }
    Ok(String::from_utf8_lossy(&answer.stdout)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .map(str::to_owned))
}

/// Create a branch the remote does not have yet, at the tip the checkout
/// already has, as its own step the page names as such.
///
/// An earlier draft proved the tip was on the remote through a "witness" head
/// and leased that head in the same push; it is withdrawn because a refspec
/// the remote already has at that value is dropped from the push as up to
/// date, so the witness was never in the transaction it was meant to guard.
pub fn publish_branch(
    git: &Git,
    destination: &Destination,
    branch: &str,
    tip: &str,
) -> Result<(), PushRefusal> {
    let answer = git
        .run_with_environment(
            &destination.environment(),
            &[
                "push".to_owned(),
                "--no-verify".to_owned(),
                "--no-follow-tags".to_owned(),
                "--recurse-submodules=no".to_owned(),
                format!("--force-with-lease={branch}:"),
                destination.name.clone(),
                format!("{tip}:{branch}"),
            ],
        )
        .map_err(|e| PushRefusal::Git(e.to_string()))?;
    if answer.ok() {
        Ok(())
    } else {
        Err(PushRefusal::Git(destination.safe(&answer.stderr)))
    }
}

/// The push: the commit by id, the destination ref leased on the tip that was
/// recorded, and every flag that keeps it to those two.
///
/// `--recurse-submodules=no` because `push.recurseSubmodules=only` makes `git
/// push` skip the superproject's own refs and report success (**measured**:
/// under it the push said `Everything up-to-date` and the remote stayed at the
/// tip), and a UI that read that exit code would show a publication that never
/// happened. `--no-follow-tags` because `push.followTags=true` would send
/// every annotated tag reachable from the tip along with the branch.
/// `--no-verify` because a `pre-push` hook is a hook. Never a bare `git push`,
/// which under `push.default=matching` advanced two branches at once.
pub fn push(
    git: &Git,
    destination: &Destination,
    branch: &str,
    lease: &str,
    commit: &str,
) -> Result<String, PushRefusal> {
    let answer = git
        .run_with_environment(
            &destination.environment(),
            &[
                "push".to_owned(),
                "--no-verify".to_owned(),
                "--no-follow-tags".to_owned(),
                "--recurse-submodules=no".to_owned(),
                format!("--force-with-lease={branch}:{lease}"),
                destination.name.clone(),
                format!("{commit}:{branch}"),
            ],
        )
        .map_err(|e| PushRefusal::Git(e.to_string()))?;
    if answer.ok() {
        Ok(destination.shown())
    } else {
        Err(PushRefusal::Git(destination.safe(&answer.stderr)))
    }
}

/// Rebuild a URL from the parts that name a destination and nothing else.
pub fn rebuild(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let path = tail.split(['?', '#']).next().unwrap_or("");
        return format!("{scheme}://{host}{path}");
    }
    // An `scp`-like address, `user@host:path`, rebuilt as `host:path`. The
    // colon has to come before any slash, or this is an ordinary local path.
    if let Some(colon) = url.find(':')
        && url[..colon].find('/').is_none()
        && let Some(at) = url[..colon].rfind('@')
    {
        return format!("{}{}", &url[at + 1..colon], &url[colon..]);
    }
    url.to_owned()
}

/// Replace every URL-shaped run in text with its rebuilding.
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(&[':', '/', '/']) {
            // Walk back to the start of the scheme.
            let mut start = index;
            while start > 0 && is_scheme(bytes[start - 1]) {
                start -= 1;
            }
            // And forward to the end of the URL.
            let mut end = index + 3;
            while end < bytes.len() && !is_boundary(bytes[end]) {
                end += 1;
            }
            let url: String = bytes[start..end].iter().collect();
            out.truncate(out.len() - (index - start));
            out.push_str(&rebuild(&url));
            index = end;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

fn is_scheme(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

fn is_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\'' | '"' | '`' | ')' | ']' | ',' | ';')
}

/// The remote name is drawn by the UI from hex, so only the characters a
/// regular expression would take specially need escaping; this keeps the
/// `--get-regexp` query exact whatever the name turns out to be.
fn regex_quote(name: &str) -> String {
    name.chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                vec![c]
            } else {
                vec!['\\', c]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::scratch_repo::Scratch;

    #[test]
    fn a_url_is_rebuilt_from_the_parts_that_name_a_destination_and_nothing_else() {
        // Only userinfo announces itself with an `@`; a query can hold a
        // credential too, and `git`'s own transport prints one verbatim.
        assert_eq!(
            rebuild("https://tok3n@example.invalid/repo.git"),
            "https://example.invalid/repo.git"
        );
        assert_eq!(
            rebuild("https://example.invalid/repo.git?access_token=s3cret"),
            "https://example.invalid/repo.git"
        );
        assert_eq!(
            rebuild("https://user:pass@example.invalid:8443/a/b.git#frag"),
            "https://example.invalid:8443/a/b.git"
        );
        assert_eq!(
            rebuild("git@example.invalid:team/repo.git"),
            "example.invalid:team/repo.git"
        );
        assert_eq!(rebuild("/srv/git/repo.git"), "/srv/git/repo.git");
        assert_eq!(
            rebuild("ssh://example.invalid/repo.git"),
            "ssh://example.invalid/repo.git"
        );
    }

    #[test]
    fn every_url_shaped_run_of_git_output_is_replaced_by_the_rebuilding() {
        // The measured line: `git`'s transport strips userinfo from its own
        // diagnostics but prints a query verbatim.
        let said = "fatal: unable to access 'https://127.0.0.1:1/x.git?access_token=s3cret/': \
                    Could not resolve host";
        let shown = redact(said);
        assert!(!shown.contains("s3cret"), "{shown}");
        assert!(shown.contains("https://127.0.0.1:1/x.git"), "{shown}");
        assert!(shown.contains("Could not resolve host"));

        let two = redact("first https://a@h1.invalid/r.git then https://b@h2.invalid/s.git.");
        assert_eq!(
            two,
            "first https://h1.invalid/r.git then https://h2.invalid/s.git."
        );
    }

    fn with_remote(name: &str) -> (Scratch, String, String) {
        let scratch = Scratch::new(name);
        scratch.write("a.txt", b"one");
        let tip = scratch.commit("one");
        let bare = scratch.path("..").join("remote.git");
        std::process::Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&bare)
            .output()
            .unwrap();
        scratch.git(&["remote", "add", "origin", bare.to_str().unwrap()]);
        (scratch, tip, bare.display().to_string())
    }

    #[test]
    fn a_remote_with_two_push_urls_is_refused_with_both_named() {
        let (scratch, _, bare) = with_remote("push-two");
        scratch.git(&["config", "--add", "remote.origin.pushurl", &bare]);
        scratch.git(&[
            "config",
            "--add",
            "remote.origin.pushurl",
            "/srv/elsewhere.git",
        ]);
        let git = scratch.runner();

        let refusal = destination(&git, "origin", "pbps-ui-test".to_owned()).unwrap_err();
        let PushRefusal::NotOneDestination { urls } = &refusal else {
            panic!("expected two destinations to be refused, got {refusal}");
        };
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn a_rewrite_rule_whose_value_is_a_prefix_of_the_url_refuses_the_compose() {
        // The URL the checks spoke to and the URL the push goes to must be one
        // string, and a rule that would rewrite it makes them two.
        let (scratch, _, bare) = with_remote("push-rewrite");
        scratch.git(&[
            "config",
            &format!("url.{}-elsewhere.pushInsteadOf", bare),
            &bare,
        ]);
        let git = scratch.runner();

        let refusal = destination(&git, "origin", "pbps-ui-test".to_owned()).unwrap_err();
        assert!(
            matches!(refusal, PushRefusal::Rewritten { .. }),
            "got {refusal}"
        );
    }

    #[test]
    fn a_name_the_configuration_already_holds_is_refused_rather_than_added_to() {
        let (scratch, _, bare) = with_remote("push-name");
        scratch.git(&["remote", "add", "pbps-ui-taken", &bare]);
        let git = scratch.runner();

        let refusal = destination(&git, "origin", "pbps-ui-taken".to_owned()).unwrap_err();
        assert!(
            matches!(refusal, PushRefusal::NameTaken(_)),
            "got {refusal}"
        );
    }

    #[test]
    fn a_branch_the_remote_does_not_have_is_published_before_composing() {
        let (scratch, tip, _) = with_remote("push-new-branch");
        let git = scratch.runner();
        let destination = destination(&git, "origin", "pbps-ui-fresh".to_owned()).unwrap();

        assert_eq!(
            remote_tip(&git, &destination, "refs/heads/main").unwrap(),
            None
        );
        publish_branch(&git, &destination, "refs/heads/main", &tip).unwrap();
        assert_eq!(
            remote_tip(&git, &destination, "refs/heads/main").unwrap(),
            Some(tip.clone()),
            "the branch appeared at the tip the checkout already has"
        );

        // And the push that follows names the commit by id and leases the
        // destination on the tip it recorded.
        scratch.write("a.txt", b"two");
        let next = scratch.commit("two");
        let published = push(&git, &destination, "refs/heads/main", &tip, &next).unwrap();
        assert!(!published.is_empty());
        assert_eq!(
            remote_tip(&git, &destination, "refs/heads/main").unwrap(),
            Some(next)
        );
    }

    #[test]
    fn the_environment_only_remote_never_reaches_the_configuration_file() {
        let (scratch, tip, _) = with_remote("push-env");
        let git = scratch.runner();
        let destination = destination(&git, "origin", "pbps-ui-hidden".to_owned()).unwrap();
        publish_branch(&git, &destination, "refs/heads/main", &tip).unwrap();

        let configured = std::fs::read_to_string(scratch.path(".git/config")).unwrap();
        assert!(
            !configured.contains("pbps-ui-hidden"),
            "the name exists only in two processes' environment: {configured}"
        );
    }
}
