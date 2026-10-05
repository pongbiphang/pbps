#!/usr/bin/env bash
# Keep the pinned expression evaluator and npm cache out of the source tree.
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
policy_root=$(mktemp -d "${TMPDIR:-/tmp}/pbps-ci-event-policy.XXXXXX")
trap 'rm -rf -- "$policy_root"' EXIT
cp "$repo_root/scripts/ci-event-policy/"{package.json,package-lock.json} "$policy_root/"
cp "$repo_root/scripts/ci_event_policy_test.mjs" "$policy_root/"
npm ci --prefix "$policy_root" --cache "${TMPDIR:-/tmp}/pbps-ci-policy-npm-cache" \
  --ignore-scripts --no-audit --no-fund
node "$policy_root/ci_event_policy_test.mjs" "$repo_root/.github/workflows/ci.yml"
