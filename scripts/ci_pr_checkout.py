#!/usr/bin/env python3
"""Pin one current PR merge commit after approval; edited-event SHAs can be stale."""
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import time
import urllib.error
import urllib.request
import urllib.parse


class CheckoutError(RuntimeError):
    """Unavailable or ambiguous checkout evidence must fail CI admission."""


def sha(value):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{40}", value):
        raise CheckoutError("Missing or invalid commit SHA")
    return value


@dataclass(frozen=True)
class Checkout:
    checkout_sha: str
    base_sha: str
    head_sha: str
    base_ref: str
    merge_ref: str


def pr_state(value, number, expected_head, expected_base):
    if not isinstance(value, dict) or value.get("number") != number or value.get("state") != "open":
        raise CheckoutError("The requested PR is missing or no longer open")
    try:
        head = sha(value["head"]["sha"])
        base = sha(value["base"]["sha"])
        base_ref = value["base"]["ref"]
    except (KeyError, TypeError) as error:
        raise CheckoutError("Incomplete PR head/base evidence") from error
    if head != expected_head or base_ref != expected_base:
        raise CheckoutError("PR head or intended base ref changed since this event")
    merge = value.get("merge_commit_sha")
    if merge is not None:
        merge = sha(merge)
    return {"head_sha": head, "base_sha": base, "base_ref": base_ref, "merge_sha": merge}


def ref_sha(value, merge_ref):
    try:
        if value["ref"] != merge_ref or value["object"]["type"] != "commit":
            raise CheckoutError("Unexpected merge ref or object type")
        return sha(value["object"]["sha"])
    except (KeyError, TypeError) as error:
        raise CheckoutError("Incomplete merge-ref evidence") from error


def select_checkout(api, repository, number, expected_head, expected_base, expected_base_sha,
                    *, timeout=60, attempts=30, clock=time.monotonic, sleep=time.sleep):
    """Wait for matching live PR/ref/parent snapshots, then share that immutable SHA.

    DEC-1457.1: edited events can retain the former base's GITHUB_SHA. PR
    association alone therefore cannot prove what was tested. Head/base intent
    and its ancestry floor are event-bound; movement of the same base branch
    is allowed without rebasing even when the synthetic merge ref stays behind.
    """
    sha(expected_head)
    sha(expected_base_sha)
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise CheckoutError("Invalid repository")
    if not isinstance(number, int) or isinstance(number, bool) or number <= 0 or not isinstance(expected_base, str) or not expected_base:
        raise CheckoutError("Missing PR number or intended base ref")
    deadline = clock() + timeout
    merge_ref = f"refs/pull/{number}/merge"
    base_ref = "refs/heads/" + expected_base
    base_path = "heads/" + urllib.parse.quote(expected_base, safe="/")
    prefix = f"repos/{repository}"
    def read(path):
        if clock() >= deadline:
            raise CheckoutError("Merge-ref selection exceeded its time bound")
        result = api(path)
        if clock() >= deadline:
            raise CheckoutError("Merge-ref selection exceeded its time bound")
        return result
    def base_contains(ancestor, descendant):
        if ancestor == descendant:
            return True
        comparison = read(f"{prefix}/compare/{ancestor}...{descendant}")
        try:
            if sha(comparison["base_commit"]["sha"]) != ancestor:
                raise CheckoutError("Unexpected ancestry comparison base")
            merge_base = sha(comparison["merge_base_commit"]["sha"])
            status = comparison["status"]
        except (KeyError, TypeError) as error:
            raise CheckoutError("Incomplete base-ancestry evidence") from error
        if status not in ("ahead", "identical", "behind", "diverged"):
            raise CheckoutError("Unexpected base-ancestry status")
        return merge_base == ancestor and status in ("ahead", "identical")
    for _ in range(attempts):
        if clock() >= deadline:
            break
        before = pr_state(read(f"{prefix}/pulls/{number}"), number, expected_head, expected_base)
        if before["merge_sha"] is None:
            sleep(2)
            continue
        current_base = ref_sha(read(f"{prefix}/git/ref/{base_path}"), base_ref)
        # The event's base is the earliest admissible integration tree.
        # GitHub need not advance a PR merge ref every time master moves.
        if not base_contains(expected_base_sha, before["base_sha"]):
            sleep(2)
            continue
        if not base_contains(before["base_sha"], current_base):
            sleep(2)
            continue
        current = ref_sha(read(f"{prefix}/git/ref/pull/{number}/merge"), merge_ref)
        if current != before["merge_sha"]:
            sleep(2)
            continue
        commit = read(f"{prefix}/git/commits/{current}")
        try:
            parents = [sha(parent["sha"]) for parent in commit["parents"]]
            if sha(commit["sha"]) != current or len(parents) != 2:
                raise CheckoutError("The merge ref lacks an exact two-parent commit")
        except (KeyError, TypeError) as error:
            raise CheckoutError("Incomplete merge-commit parent evidence") from error
        # PR.base.sha is cached independently of the synthetic merge ref.
        # Qualify the commit's actual base parent against the event floor and
        # intended live branch; equality with that cached field rejects a
        # valid merge ref after ordinary same-branch advancement.
        selected_base = parents[0]
        if (parents[1] != expected_head
                or not base_contains(expected_base_sha, selected_base)
                or not base_contains(selected_base, current_base)):
            sleep(2)
            continue
        after = pr_state(read(f"{prefix}/pulls/{number}"), number, expected_head, expected_base)
        confirmed = ref_sha(read(f"{prefix}/git/ref/pull/{number}/merge"), merge_ref)
        confirmed_base = ref_sha(read(f"{prefix}/git/ref/{base_path}"), base_ref)
        if before == after and current == confirmed and current_base == confirmed_base:
            return Checkout(current, selected_base, expected_head, expected_base, merge_ref)
        sleep(2)
    raise CheckoutError("No stable merge ref matching the intended PR head and base within the bound")


def github_api(token, api_url):
    def read(path):
        request = urllib.request.Request(
            api_url.rstrip("/") + "/" + path,
            headers={"Authorization": "Bearer " + token, "Accept": "application/vnd.github+json",
                     "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "pbps-ci-checkout"},
        )
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                return json.load(response)
        except (urllib.error.URLError, OSError, ValueError) as error:
            # Tokens and response bodies are deliberately absent from diagnostics.
            raise CheckoutError(f"GitHub API read unavailable for {path}: {type(error).__name__}") from error
    return read


def main():
    if os.environ["GITHUB_EVENT_NAME"] == "pull_request":
        payload = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
        try:
            pr = payload["pull_request"]
            evidence = select_checkout(github_api(os.environ["GITHUB_TOKEN"], os.environ["GITHUB_API_URL"]),
                                       os.environ["GITHUB_REPOSITORY"], payload["number"],
                                       pr["head"]["sha"], pr["base"]["ref"], pr["base"]["sha"])
        except (KeyError, TypeError) as error:
            raise CheckoutError("Incomplete immutable pull-request event intent") from error
        selected = evidence.checkout_sha
        print(f"Selected CI checkout: {selected} (base: {evidence.base_sha}, head: {evidence.head_sha}, ref: {evidence.base_ref}, merge-ref: {evidence.merge_ref}, event-base: {pr['base']['sha']})")
    else:
        # Merge groups, pushes and dispatches already carry their exact CI tree.
        selected = sha(os.environ["GITHUB_SHA"])
        print(f"Selected CI checkout: {selected} (event: {os.environ['GITHUB_EVENT_NAME']})")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
        output.write(f"checkout_sha={selected}\n")


if __name__ == "__main__":
    main()
