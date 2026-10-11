// Evaluate the expressions read from the workflow with GitHub's own parser.
import assert from 'node:assert/strict';
import { readFileSync, mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { test } from 'node:test';
import { Lexer, Parser, Evaluator, data } from '@actions/expressions';
import { falsy } from '@actions/expressions/result';

const workflow = readFileSync(process.argv[2], 'utf8');
function property(block, key) {
  const matches = [...block.matchAll(new RegExp(`^ +${key}: (.+)$`, 'gm'))];
  assert.equal(matches.length, 1, `one ${key} declaration`);
  return matches[0][1];
}
function job(key) {
  const match = workflow.match(new RegExp(`^  ${key}:\\n([\\s\\S]*?)(?=^  [\\w-]+:|(?![\\s\\S]))`, 'm'));
  assert.ok(match, `job ${key} exists`);
  return match[1];
}
const concurrency = workflow.match(/^concurrency:\n([\s\S]*?)(?=^\S)/m)[1];
const policy = {
  group: property(concurrency, 'group'),
  cancel: property(concurrency, 'cancel-in-progress'),
  approve: '${{ true }}',
  environment: property(job('approve'), 'environment'),
  gate: property(job('gate'), 'if'),
  check: property(job('gate'), 'name'),
};
function value(input) {
  if (input === null || input === undefined) return new data.Null();
  if (typeof input === 'string') return new data.StringData(input);
  if (typeof input === 'number') return new data.NumberData(input);
  if (typeof input === 'boolean') return new data.BooleanData(input);
  return new data.Dictionary(...Object.entries(input).map(([key, item]) => ({ key, value: value(item) })));
}
const always = { name: 'always', minArgs: 0, maxArgs: 0, call: () => new data.BooleanData(true) };
function evaluate(template, github, needs = {}) {
  const parts = template.split(/(\$\{\{[\s\S]*?\}\})/).filter(Boolean);
  const results = parts.map(part => {
    if (!part.startsWith('${{')) return part;
    const tokens = new Lexer(part.slice(3, -2)).lex().tokens;
    const expression = new Parser(tokens, ['github', 'needs'], [always]).parse();
    return new Evaluator(expression, value({ github, needs }), new Map([['always', always]])).evaluate();
  });
  if (results.length === 1 && typeof results[0] !== 'string') return results[0];
  return new data.StringData(results.map(item => typeof item === 'string' ? item : item.coerceString()).join(''));
}
function context(event_name, action, changes, run_id = 42) {
  return { workflow: 'CI', event_name, event: { action, changes, pull_request: { number: 1457, base: { ref: 'master' }, head: { sha: 'unchanged-head' } } }, ref: 'refs/pull/1457/merge', run_id };
}
function outcome(github) {
  return Object.fromEntries(Object.entries(policy).map(([key, template]) => {
    const result = evaluate(template, github);
    // Unlike job conditions, cancellation rejects a null or string result.
    if (key === 'cancel') assert.ok(result instanceof data.BooleanData, 'concurrency cancellation must evaluate to a boolean');
    return [key, ['approve', 'gate', 'cancel'].includes(key) ? !falsy(result) : result.coerceString()];
  }));
}
function requiresApprovedCi(github, pr = true) {
  const actual = outcome(github);
  assert.equal(actual.approve, true);
  assert.equal(actual.gate, true);
  assert.equal(actual.check, 'ci-gate');
  assert.equal(actual.environment, pr ? 'ci-approval' : '');
  assert.equal(actual.cancel, pr);
  return actual;
}

const pullRequestTypes = workflow.match(/^  pull_request:\n    types: \[([^\]]+)\]/m);
assert.ok(pullRequestTypes, 'PR activity types are explicit');
const admittedTypes = pullRequestTypes[1].split(',').map(item => item.trim());
function admitted(github) {
  return github.event_name !== 'pull_request' || admittedTypes.includes(github.event.action);
}

test('opened, synchronize and reopened still require approved CI', () => {
  for (const action of ['opened', 'synchronize', 'reopened']) {
    const event = context('pull_request', action);
    assert.equal(admitted(event), true);
    assert.equal(requiresApprovedCi(event).group, 'CI-1457');
  }
  assert.deepEqual([...admittedTypes].sort(), ['labeled', 'opened', 'reopened', 'synchronize']);
});

test('an explicit label requests associated CI on the unchanged head and intended base', () => {
  const labeled = context('pull_request', 'labeled');
  labeled.event.pull_request.base.ref = 'retargeted-base';
  labeled.event.label = { name: 'ci-retest' };
  assert.equal(admitted(labeled), true);
  assert.equal(labeled.event.pull_request.head.sha, 'unchanged-head');
  for (const existing of ['success', null]) {
    labeled.event.pull_request.checks = existing;
    assert.equal(requiresApprovedCi(labeled).group, 'CI-1457');
  }
});

test('title and body edits cannot create a suite that supersedes qualified checks', () => {
  for (const changes of [{ title: { from: 'Old title' } }, { body: { from: '' } }, { title: { from: 'a' }, body: { from: 'b' } }, undefined, {}, { base: { sha: { from: 'old-sha' } } }]) {
    assert.equal(admitted(context('pull_request', 'edited', changes)), false);
  }
  // Skipped/renamed jobs preserved the run but made real queue admission fail.
  assert.equal(policy.check, 'ci-gate');
  assert.doesNotMatch(workflow, /ci-metadata|metadata-\{|changes\.base/);
});

test('a base edit requires a fresh explicit retry rather than silently rerunning CI', () => {
  const changed = context('pull_request', 'edited', { base: { ref: { from: 'parent' }, sha: { from: 'old-base-sha' } }, title: { from: 'Old' } });
  assert.equal(admitted(changed), false);
  changed.event.action = 'labeled';
  changed.event.label = { name: 'ci-retest' };
  assert.equal(admitted(changed), true);
  requiresApprovedCi(changed);
  assert.equal(admitted(context('pull_request', 'unlabeled')), false, 'removing a retry label cannot cancel CI');
});

test('no admitted label can skip approval or manufacture a passing required context', () => {
  assert.doesNotMatch(job('approve'), /^    if:/m, 'all admitted labels enter approval');
  for (const name of ['ci-retest', 'ordinary-triage', '']) {
    const event = context('pull_request', 'labeled');
    event.event.label = { name };
    assert.equal(admitted(event), true);
    requiresApprovedCi(event);
  }
  assert.equal(policy.check, 'ci-gate');
});

test('concurrency cancellation is boolean and never applies to non-PR events', () => {
  for (const changes of [undefined, {}, { body: { from: 'Old body' } }, { base: { ref: { from: 'master' } } }]) {
    assert.ok(evaluate(policy.cancel, context('pull_request', 'labeled', changes)) instanceof data.BooleanData);
  }
  for (const event of ['schedule', 'merge_group', 'workflow_dispatch']) {
    const github = { workflow: 'CI', event_name: event, event: {}, ref: 'refs/heads/master', run_id: 42 };
    assert.equal(requiresApprovedCi(github, false).group, 'CI-refs/heads/master');
  }
});

test('failure and cancellation cannot skip the required qualifying gate', () => {
  assert.match(policy.gate, /\balways\(\)/);
  assert.doesNotMatch(policy.gate, /cancelled\(/);
  for (const action of ['opened', 'labeled']) assert.equal(outcome(context('pull_request', action)).gate, true);
  assert.match(job('gate'), /^        run: python3 scripts\/ci_gate_verdict\.py$/m);
  assert.match(job('gate'), /python3 scripts\/check_gate_needs\.py/);
  for (const [name, block] of [...workflow.split('jobs:\n')[1].matchAll(/^  ([\w-]+):\n([\s\S]*?)(?=^  [\w-]+:|(?![\s\S]))/gm)].map(match => [match[1], match[2]])) {
    if (name === 'approve' || name === 'gate') continue;
    assert.match(block, /^    needs: (approve|\[approve, lint\])$/m, `${name} must remain behind approval`);
  }
});

test('a pull request omits exactly the declared jobs, and every other event runs every job', () => {
  const declared = workflow.match(/^          PR_OMITTED: (.+)$/m);
  assert.ok(declared, 'the gate declares the jobs a pull request omits');
  const omitted = declared[1].trim().split(/\s+/);
  assert.deepEqual([...omitted].sort(), ['live', 'live-pg', 'release-windows', 'resolver']);
  const condition = "${{ github.event_name != 'pull_request' }}";
  const blocks = [...workflow.split('jobs:\n')[1].matchAll(/^  ([\w-]+):\n([\s\S]*?)(?=^  [\w-]+:|(?![\s\S]))/gm)];
  const names = blocks.map(match => match[1]);
  for (const name of omitted) assert.ok(names.includes(name), `${name} is a job`);
  assert.ok(!omitted.includes('approve') && !omitted.includes('lint'), 'approval and lint always run');
  for (const [, name, block] of blocks) {
    if (name === 'gate') continue;
    const conditions = [...block.matchAll(/^    if: (.+)$/gm)].map(match => match[1]);
    if (!omitted.includes(name)) {
      assert.deepEqual(conditions, [], `${name} has no job-level condition, so it runs on every event`);
      continue;
    }
    assert.deepEqual(conditions, [condition], `${name} is skipped on pull requests and nowhere else`);
    for (const event of ['merge_group', 'schedule', 'workflow_dispatch']) {
      const github = { workflow: 'CI', event_name: event, event: {}, ref: 'refs/heads/master', run_id: 42 };
      assert.equal(falsy(evaluate(condition, github)), false, `${name} runs on ${event}`);
    }
    assert.equal(falsy(evaluate(condition, context('pull_request', 'synchronize'))), true, `${name} is skipped on a pull request`);
  }
  const verdict = job('gate').match(/- name: every job succeeded\n        env:\n([\s\S]*?)\n        run: (.+)/);
  assert.ok(verdict, 'the verdict step reads its inputs from the environment');
  assert.match(verdict[1], /^          EVENT: \$\{\{ github\.event_name \}\}$/m);
  assert.match(verdict[1], /^          NEEDS: \$\{\{ toJSON\(needs\) \}\}$/m);
});

test('master runs nightly rather than on push, and only master saves caches', () => {
  const triggers = workflow.match(/^on:\n([\s\S]*?)(?=^\S)/m);
  assert.ok(triggers, 'the workflow declares its triggers');
  const events = [...triggers[1].matchAll(/^  ([\w-]+):/gm)].map(match => match[1]).sort();
  assert.deepEqual(events, ['merge_group', 'pull_request', 'schedule', 'workflow_dispatch'],
    'a push to master is the merge group\'s own commit, so it is not a trigger');
  assert.equal([...triggers[1].matchAll(/^    - cron: "[^"]+"$/gm)].length, 1, 'one nightly schedule');
  const caches = [...workflow.matchAll(/^      - uses: Swatinem\/rust-cache@v2\n((?:        .*\n)*)/gm)];
  assert.ok(caches.length > 0, 'the workflow restores Rust caches');
  const saveIf = "${{ github.ref == 'refs/heads/master' }}";
  for (const [, inputs] of caches) {
    assert.match(inputs, /^        with:$/m, 'every cache step has inputs');
    const declared = [...inputs.matchAll(/^          save-if: (.+)$/gm)].map(match => match[1]);
    assert.deepEqual(declared, [saveIf], 'a pull request or merge group never saves a cache nobody can restore');
  }
  for (const [ref, saves] of [['refs/heads/master', true], ['refs/pull/1457/merge', false], ['refs/heads/gh-readonly-queue/master/pr-1457-abc', false]]) {
    assert.equal(!falsy(evaluate(saveIf, { ref })), saves, `${ref} saves caches: ${saves}`);
  }
});

test('release-windows starts on every non-PR event and conditions everything after its scope decision', () => {
  const steps = job('release-windows').split(/^      - /m).slice(1);
  const scope = steps.findIndex(step => /^        id: scope$/m.test(step));
  assert.ok(scope > 0, 'the job decides its scope in a step with id `scope`');
  const decide = steps[scope];
  assert.match(decide, /^          python scripts\/ci_release_scope\.py$/m);
  assert.match(decide, /^          EVENT: \$\{\{ github\.event_name \}\}$/m);
  assert.match(decide, /^          BASE_SHA: \$\{\{ github\.event\.merge_group\.base_sha \}\}$/m);
  assert.match(decide, /^          HEAD_SHA: \$\{\{ needs\.approve\.outputs\.checkout_sha \}\}$/m);
  for (const step of steps.slice(0, scope)) {
    assert.doesNotMatch(step, /^        if:/m, 'the steps the decision needs always run');
  }
  const later = steps.slice(scope + 1);
  assert.ok(later.length > 0);
  for (const step of later) {
    const conditions = [...step.matchAll(/^        if: (.+)$/gm)].map(match => match[1]);
    assert.equal(conditions.length, 1, `one condition on: ${step.split('\n')[0]}`);
    assert.ok(conditions[0].includes("steps.scope.outputs.run == 'true'"),
      `a step after the decision runs only when it says so: ${step.split('\n')[0]}`);
  }
});

test('every job uses one validated intended-base checkout rather than an unverified event SHA', () => {
  const head = 'a'.repeat(40);
  const staleMerge = 'b'.repeat(40);
  const currentMerge = 'c'.repeat(40);
  const event = context('pull_request', 'labeled');
  event.sha = staleMerge;
  event.workflow_sha = "d".repeat(40);
  event.event.pull_request.head.sha = head;
  const approve = job('approve');
  assert.match(approve, /checkout_sha: \$\{\{ steps\.checkout\.outputs\.checkout_sha \}\}/);
  assert.match(approve, /python3 scripts\/ci_pr_checkout\.py/);
  const bootstrap = approve.match(/uses: actions\/checkout@v7\n        with:\n          ref: (.+)/);
  assert.ok(bootstrap, 'approval bootstraps the defining workflow before selecting its merge ref');
  assert.equal(evaluate(bootstrap[1], event).coerceString(), event.workflow_sha);
  for (const [name, block] of [...workflow.split('jobs:\n')[1].matchAll(/^  ([\w-]+):\n([\s\S]*?)(?=^  [\w-]+:|(?![\s\S]))/gm)].map(match => [match[1], match[2]])) {
    if (name === 'approve') continue;
    assert.match(block, /^    needs: (approve|\[[^\]\n]*\bapprove\b[^\]\n]*\])$/m, `${name} directly receives approval outputs`);
    const checkouts = [...block.matchAll(/uses: actions\/checkout@v7\n        with:\n          ref: (.+)/g)];
    assert.equal(checkouts.length, 1, `${name} has one explicit checkout`);
    const selected = evaluate(checkouts[0][1], event, { approve: { outputs: { checkout_sha: currentMerge } } }).coerceString();
    assert.equal(selected, currentMerge, `${name} shares the validated merge SHA`);
    assert.notEqual(selected, staleMerge, 'an event SHA cannot select an unverified former-base tree');
  }
});


test('existing PR heads can bootstrap newly introduced helpers without rebasing', () => {
  const fixture = mkdtempSync(join(tmpdir(), 'pbps-ci-workflow-source-'));
  const git = (...args) => execFileSync('git', ['-c', 'user.name=CI fixture', '-c', 'user.email=fixture@example.invalid', '-c', 'commit.gpgsign=false', '-c', 'core.hooksPath=/dev/null', ...args], { cwd: fixture, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
  try {
    git('init', '--quiet');
    git('commit', '--quiet', '--allow-empty', '-m', 'Existing PR head before the selector');
    const oldHead = git('rev-parse', 'HEAD');
    const approve = job('approve');
    const selectorPath = property(approve, 'run').replace(/^python3 /, '');
    const selectorSource = '# CI helper introduced with its defining workflow\n';
    mkdirSync(dirname(join(fixture, selectorPath)), { recursive: true });
    writeFileSync(join(fixture, selectorPath), selectorSource);
    mkdirSync(join(fixture, '.github/workflows'), { recursive: true });
    writeFileSync(join(fixture, '.github/workflows/ci.yml'), workflow);
    git('add', '.');
    git('commit', '--quiet', '-m', 'Defining workflow introduces its selector');
    const workflowSha = git('rev-parse', 'HEAD');
    assert.throws(() => git('show', `${oldHead}:${selectorPath}`), 'the unchanged pre-existing head has no new helper');
    const event = context('pull_request', 'labeled');
    event.sha = oldHead;
    event.event.pull_request.head.sha = oldHead;
    event.workflow_sha = workflowSha;
    const selected = evaluate(property(approve, 'ref'), event).coerceString();
    assert.equal(git('show', `${selected}:${selectorPath}`), selectorSource.trim(), 'bootstrap must find the helper in the immutable defining workflow');
    const marker = approve.match(/- name: Pin the validated CI tree \((.+)\)/);
    assert.ok(marker, 'CI step metadata names the actual workflow source');
    assert.equal(evaluate(marker[1], event).coerceString(), selected);
    assert.equal(event.event.pull_request.head.sha, oldHead, 'the reviewed PR head is unchanged');
  } finally {
    rmSync(fixture, { recursive: true, force: true });
  }
});
