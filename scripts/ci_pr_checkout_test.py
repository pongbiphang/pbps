#!/usr/bin/env python3
"""Replay the real edited-event stale-base shape without network or host privileges."""
import copy
import contextlib
import io
import json
import os
from pathlib import Path
import tempfile
from unittest import mock
import ci_pr_checkout
import unittest
from ci_pr_checkout import CheckoutError, select_checkout

HEAD = "a" * 40
BASE = "b" * 40
OLD_BASE = "c" * 40
MERGE = "d" * 40
OLD_MERGE = "e" * 40
NEW_BASE = "f" * 40
NEW_MERGE = "1" * 40
FOREIGN_BASE = "3" * 40


class Api:
    def __init__(self, stale=0, move_base=False):
        self.stale = stale
        self.move_base = move_base
        self.commits_read = 0
        self.pulls_read = 0
        self.requests = []
        self.head = HEAD
        self.base_ref = "master"
        self.state = "open"
        self.incomplete = False
        self.absent = False
        self.stale_pr_base = False
        self.actual_base = None
        self.merge_base = None

    def __call__(self, path):
        self.requests.append(path)
        stale = self.commits_read < self.stale
        moving = self.move_base and self.pulls_read >= 2
        merge = OLD_MERGE if stale else NEW_MERGE if moving else MERGE
        base = NEW_BASE if moving else BASE
        if "/git/ref/heads/" in path:
            return {"ref": "refs/heads/master", "object": {"type": "commit", "sha": self.actual_base or base}}
        if "/compare/" in path:
            ancestor, descendant = path.rsplit("/", 1)[1].split("...")
            order = {OLD_BASE: 0, BASE: 1, NEW_BASE: 2}
            if descendant == FOREIGN_BASE and ancestor in (OLD_BASE, BASE):
                return {"base_commit": {"sha": ancestor}, "merge_base_commit": {"sha": ancestor}, "status": "ahead"}
            if ancestor not in order or descendant not in order:
                return {"base_commit": {"sha": ancestor}, "merge_base_commit": {"sha": OLD_BASE}, "status": "diverged"}
            status = "ahead" if order[ancestor] < order[descendant] else "behind"
            return {"base_commit": {"sha": ancestor}, "merge_base_commit": {"sha": ancestor if status == "ahead" else descendant}, "status": status}
        if "/pulls/" in path:
            self.pulls_read += 1
            moving = self.move_base and self.pulls_read >= 2
            return {"number": 1535, "state": self.state, "head": {"sha": self.head},
                    "base": {"sha": OLD_BASE if self.stale_pr_base else NEW_BASE if moving else BASE, "ref": self.base_ref},
                    "merge_commit_sha": None if self.absent else OLD_MERGE if stale else NEW_MERGE if moving else MERGE}
        if "/git/ref/" in path:
            return {"ref": "refs/pull/1535/merge", "object": {"type": "commit", "sha": merge}}
        if "/git/commits/" in path:
            self.commits_read += 1
            return {"sha": path.rsplit("/", 1)[1], "parents": [{"sha": self.merge_base or (OLD_BASE if stale else base)}] + ([] if self.incomplete else [{"sha": HEAD}])}
        raise AssertionError(path)


class CheckoutTests(unittest.TestCase):
    def select(self, api, attempts=3):
        return select_checkout(api, "pongbiphang/pbps", 1535, HEAD, "master", BASE, attempts=attempts, sleep=lambda _: None)

    def test_former_base_merge_is_refused_even_when_its_head_and_association_match(self):
        # Actual #1535: GITHUB_SHA's parents were [former base, current head]
        # while the associated run already named master/current head.
        with self.assertRaisesRegex(CheckoutError, "No stable merge ref"):
            self.select(Api(stale=10), attempts=2)

    def test_stale_pr_base_cache_cannot_masquerade_as_the_intended_base_branch(self):
        api = Api(stale=10)
        api.stale_pr_base = True
        with self.assertRaisesRegex(CheckoutError, "No stable merge ref"):
            self.select(api, attempts=2)

    def test_asynchronous_ref_recomputation_selects_only_the_new_base_tree(self):
        api = Api(stale=1)
        result = self.select(api)
        self.assertEqual((result.checkout_sha, result.base_sha, result.head_sha), (MERGE, BASE, HEAD))
        self.assertEqual(result.merge_ref, "refs/pull/1535/merge")
        self.assertEqual(api.commits_read, 2)

    def test_current_base_movement_retries_and_pins_one_stable_snapshot(self):
        result = self.select(Api(move_base=True))
        self.assertEqual((result.checkout_sha, result.base_sha), (NEW_MERGE, NEW_BASE))

    def test_same_base_branch_can_advance_without_rebasing_or_advancing_the_merge_ref(self):
        # Actual #1535: master advanced, while PR.base.sha and the merge ref
        # still named the earlier master tree. The queue owns later integration.
        api = Api()
        api.actual_base = NEW_BASE
        result = self.select(api)
        self.assertEqual((result.checkout_sha, result.base_sha), (MERGE, BASE))

    def test_pr_base_metadata_can_lag_a_merge_ref_advanced_on_the_same_branch(self):
        # Actual #1535: the PR and associated run cached an earlier master SHA,
        # while the merge commit already had current master as its first parent.
        api = Api()
        api.actual_base = NEW_BASE
        api.merge_base = NEW_BASE
        result = self.select(api)
        self.assertEqual((result.checkout_sha, result.base_sha, result.head_sha),
                         (MERGE, NEW_BASE, HEAD))

    def test_a_merge_parent_outside_the_event_floor_or_intended_branch_is_refused(self):
        for base in (OLD_BASE, FOREIGN_BASE):
            api = Api()
            api.actual_base = NEW_BASE
            api.merge_base = base
            with self.subTest(base=base), self.assertRaisesRegex(CheckoutError, "No stable merge ref"):
                self.select(api, attempts=2)

    def test_a_base_branch_that_no_longer_contains_the_selected_base_is_refused(self):
        api = Api()
        api.actual_base = OLD_BASE
        with self.assertRaisesRegex(CheckoutError, "No stable merge ref"):
            self.select(api, attempts=2)
        api = Api()
        api.actual_base = NEW_BASE
        def unreadable_ancestry(path):
            if "/compare/" in path:
                return {"status": "ahead"}
            return api(path)
        with self.assertRaisesRegex(CheckoutError, "Incomplete base-ancestry"):
            self.select(unreadable_ancestry)

    def test_changed_head_or_intended_base_ref_cannot_be_silently_admitted(self):
        for attribute, value in [("head", "2" * 40), ("base_ref", "parent")]:
            api = Api()
            setattr(api, attribute, value)
            with self.subTest(attribute=attribute), self.assertRaisesRegex(CheckoutError, "changed since this event"):
                self.select(api)

    def test_closed_pr_and_absent_merge_evidence_do_not_mean_success(self):
        api = Api()
        api.state = "closed"
        with self.assertRaisesRegex(CheckoutError, "no longer open"):
            self.select(api)
        api = Api()
        api.absent = True
        with self.assertRaisesRegex(CheckoutError, "No stable merge ref"):
            self.select(api, attempts=2)

    def test_incomplete_parent_and_unreadable_api_evidence_are_refused(self):
        api = Api()
        api.incomplete = True
        with self.assertRaisesRegex(CheckoutError, "exact two-parent"):
            self.select(api)
        def unreadable(_):
            raise CheckoutError("unreadable")
        with self.assertRaisesRegex(CheckoutError, "unreadable"):
            self.select(unreadable)

    def test_ref_divergence_and_confirmation_head_change_cannot_supply_an_output(self):
        api = Api()
        def divergent(path):
            row = copy.deepcopy(api(path))
            if "/git/ref/pull/" in path:
                row["object"]["sha"] = OLD_MERGE
            return row
        with self.assertRaisesRegex(CheckoutError, "No stable merge ref"):
            self.select(divergent, attempts=2)
        api = Api()
        def changed_on_confirmation(path):
            row = api(path)
            if "/pulls/" in path and api.pulls_read > 1:
                row["head"]["sha"] = "2" * 40
            return row
        with self.assertRaisesRegex(CheckoutError, "changed since this event"):
            self.select(changed_on_confirmation)

    def test_incomplete_ref_and_time_boundary_cannot_supply_an_output(self):
        api = Api()
        def incomplete(path):
            row = api(path)
            if "/git/ref/" in path:
                row.pop("object")
            return row
        with self.assertRaisesRegex(CheckoutError, "Incomplete merge-ref"):
            self.select(incomplete)
        ticks = iter([0, 60])
        with self.assertRaisesRegex(CheckoutError, "No stable merge ref"):
            select_checkout(api, "pongbiphang/pbps", 1535, HEAD, "master", BASE, clock=lambda: next(ticks))


class OutputTests(unittest.TestCase):
    def test_non_pr_events_preserve_the_exact_event_tree_without_api_reads(self):
        with tempfile.TemporaryDirectory(prefix="pbps-ci-checkout-test-") as directory:
            output = Path(directory) / "output"
            for event in ["push", "merge_group", "workflow_dispatch"]:
                output.write_text("")
                with self.subTest(event=event), mock.patch.dict(os.environ, {
                    "GITHUB_EVENT_NAME": event, "GITHUB_SHA": OLD_MERGE,
                    "GITHUB_OUTPUT": str(output),
                }), mock.patch("ci_pr_checkout.github_api", side_effect=AssertionError("unexpected API read")), contextlib.redirect_stdout(io.StringIO()):
                    ci_pr_checkout.main()
                self.assertEqual(output.read_text(), f"checkout_sha={OLD_MERGE}\n")

    def test_pr_output_ignores_the_old_event_sha_and_remains_absent_on_refusal(self):
        with tempfile.TemporaryDirectory(prefix="pbps-ci-checkout-test-") as directory:
            output = Path(directory) / "output"
            event = Path(directory) / "event.json"
            event.write_text(json.dumps({"number": 1535, "pull_request": {
                "head": {"sha": HEAD}, "base": {"ref": "master", "sha": BASE},
            }}))
            output.write_text("")
            with mock.patch.dict(os.environ, {
                "GITHUB_EVENT_NAME": "pull_request", "GITHUB_SHA": OLD_MERGE,
                "GITHUB_EVENT_PATH": str(event), "GITHUB_OUTPUT": str(output),
                "GITHUB_TOKEN": "test-placeholder", "GITHUB_API_URL": "https://api.github.com",
                "GITHUB_REPOSITORY": "pongbiphang/pbps",
            }), mock.patch("ci_pr_checkout.github_api", return_value=Api()), contextlib.redirect_stdout(io.StringIO()):
                ci_pr_checkout.main()
                self.assertEqual(output.read_text(), f"checkout_sha={MERGE}\n")
                output.write_text("")
                with mock.patch("ci_pr_checkout.select_checkout", side_effect=CheckoutError("refused")), self.assertRaisesRegex(CheckoutError, "refused"):
                    ci_pr_checkout.main()
                self.assertEqual(output.read_text(), "")


if __name__ == "__main__":
    unittest.main()
