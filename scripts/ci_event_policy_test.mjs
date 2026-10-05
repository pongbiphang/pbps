// Evaluate the expressions read from the workflow with GitHub's own parser.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
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
  approve: property(job('approve'), 'if'),
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
function requiresFullCi(github, pr = true) {
  const actual = outcome(github);
  assert.equal(actual.approve, true);
  assert.equal(actual.gate, true);
  assert.equal(actual.check, 'ci-gate');
  assert.equal(actual.environment, pr ? 'ci-approval' : '');
  assert.equal(actual.cancel, pr);
  return actual;
}

test('concurrency cancellation remains boolean for absent and populated base changes', () => {
  for (const changes of [undefined, {}, { body: { from: 'Old body' } }, { base: { ref: { from: '' } } }, { base: { ref: { from: 'master' } } }]) {
    const result = evaluate(policy.cancel, context('pull_request', 'edited', changes));
    assert.ok(result instanceof data.BooleanData, 'GitHub rejects null/string cancellation before creating jobs');
  }
});

test('opened, synchronize and reopened still require approved full CI', () => {
  for (const action of ['opened', 'synchronize', 'reopened']) {
    assert.equal(requiresFullCi(context('pull_request', action)).group, 'CI-1457');
  }
  const trigger = workflow.match(/^  pull_request:\n    types: \[([^\]]+)\]/m);
  assert.ok(trigger);
  assert.deepEqual(trigger[1].split(',').map(item => item.trim()).sort(), ['edited', 'opened', 'reopened', 'synchronize']);
});

test('the base-edited payload creates a fresh associated gate without changing head', () => {
  // The webhook schema uses changes.base.ref.from and changes.base.sha.from.
  const edited = context('pull_request', 'edited', { base: { ref: { from: 'fix/issue-1212-parent' }, sha: { from: 'old-base-sha' } } });
  assert.equal(edited.event.pull_request.head.sha, 'unchanged-head');
  assert.equal(requiresFullCi(edited).group, 'CI-1457');
  // Existing success and missing evidence both leave the new run behind approval.
  for (const existing of ['success', null]) {
    edited.event.pull_request.checks = existing;
    requiresFullCi(edited);
  }
});

test('title and body edits neither cancel CI nor create a passing required context', () => {
  for (const changes of [{ base: { ref: { from: '' } } }, { title: { from: 'Old title' } }, { body: { from: '' } }, { title: { from: 'a' }, body: { from: 'b' } }, undefined, {}, { base: { sha: { from: 'old-sha' } } }]) {
    const one = outcome(context('pull_request', 'edited', changes, 42));
    const two = outcome(context('pull_request', 'edited', changes, 43));
    assert.equal(one.approve, false, JSON.stringify({ changes, one }));
    assert.equal(one.gate, false, JSON.stringify({ changes, one }));
    assert.equal(one.check, 'ci-metadata');
    assert.equal(one.cancel, false);
    assert.notEqual(one.group, 'CI-1457');
    assert.notEqual(one.group, two.group);
    assert.notEqual(outcome(context('pull_request', 'edited', changes, 1457)).group, 'CI-1457');
  }
});

test('simultaneous base and metadata edits still require full CI', () => {
  requiresFullCi(context('pull_request', 'edited', { base: { ref: { from: 'parent' } }, title: { from: 'Old' } }));
});

test('master, merge groups and dispatch keep the full gate without approval or cancellation', () => {
  for (const event of ['push', 'merge_group', 'workflow_dispatch']) {
    const github = { workflow: 'CI', event_name: event, event: {}, ref: 'refs/heads/master', run_id: 42 };
    assert.equal(requiresFullCi(github, false).group, 'CI-refs/heads/master');
  }
});

test('failure and cancellation cannot skip the required qualifying gate', () => {
  // always() remains true after failure/cancellation; no !cancelled() filter.
  assert.match(policy.gate, /\balways\(\)/);
  assert.doesNotMatch(policy.gate, /cancelled\(/);
  for (const action of ['opened', 'edited']) {
    const github = context('pull_request', action, { base: { ref: { from: 'parent' } } });
    assert.equal(outcome(github).gate, true);
  }
  assert.match(job('gate'), /all\(\.\[\]; \.result == "success"\)/);
  assert.match(job('gate'), /python3 scripts\/check_gate_needs\.py/);
  for (const [name, block] of [...workflow.split('jobs:\n')[1].matchAll(/^  ([\w-]+):\n([\s\S]*?)(?=^  [\w-]+:|(?![\s\S]))/gm)].map(match => [match[1], match[2]])) {
    if (name === 'approve' || name === 'gate') continue;
    assert.match(block, /^    needs: (approve|\[approve, lint\])$/m, `${name} must remain behind approval`);
  }
});


test('every job uses one validated intended-base checkout rather than the stale edited-event SHA', () => {
  const head = 'a'.repeat(40);
  const staleMerge = 'b'.repeat(40);
  const currentMerge = 'c'.repeat(40);
  const edited = context('pull_request', 'edited', { base: { ref: { from: 'parent' } } });
  edited.sha = staleMerge;
  edited.event.pull_request.head.sha = head;
  const approve = job('approve');
  assert.match(approve, /checkout_sha: \$\{\{ steps\.checkout\.outputs\.checkout_sha \}\}/);
  assert.match(approve, /python3 scripts\/ci_pr_checkout\.py/);
  const bootstrap = approve.match(/uses: actions\/checkout@v7\n        with:\n          ref: (.+)/);
  assert.ok(bootstrap, 'approval bootstraps the reviewed source before selecting its merge ref');
  assert.equal(evaluate(bootstrap[1], edited).coerceString(), head);
  for (const [name, block] of [...workflow.split('jobs:\n')[1].matchAll(/^  ([\w-]+):\n([\s\S]*?)(?=^  [\w-]+:|(?![\s\S]))/gm)].map(match => [match[1], match[2]])) {
    if (name === 'approve') continue;
    assert.match(block, /^    needs: (approve|\[[^\]\n]*\bapprove\b[^\]\n]*\])$/m, `${name} directly receives approval outputs`);
    const checkouts = [...block.matchAll(/uses: actions\/checkout@v7\n        with:\n          ref: (.+)/g)];
    assert.equal(checkouts.length, 1, `${name} has one explicit checkout`);
    const selected = evaluate(checkouts[0][1], edited, { approve: { outputs: { checkout_sha: currentMerge } } }).coerceString();
    assert.equal(selected, currentMerge, `${name} shares the validated merge SHA`);
    assert.notEqual(selected, staleMerge, 'edited-event GITHUB_SHA cannot select the former base');
  }
});
