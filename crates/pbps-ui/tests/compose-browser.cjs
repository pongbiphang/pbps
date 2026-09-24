// Runs after the actual shipped compose.js in Node. This DOM implements only
// browser element/event plumbing; candidate scheduling uses deferred promises.
const assert = require("node:assert/strict");
class Element {
  constructor(tag, doc) {
    this.tagName = tag; this.ownerDocument = doc; this.children = [];
    this.dataset = {}; this.listeners = {}; this.disabled = false;
    this.textContent = ""; this.value = "";
  }
  append(...elements) { this.children.push(...elements); }
  replaceChildren(...elements) { this.children = elements; }
  setAttribute(name, value) { this[name] = value; }
  addEventListener(name, handler) { (this.listeners[name] ||= []).push(handler); }
  async fire(name) {
    for (const handler of this.listeners[name] || []) await handler({preventDefault() {}});
  }
  find(predicate) {
    if (predicate(this)) return this;
    for (const child of this.children) { const found = child.find(predicate); if (found) return found; }
  }
}
const preview = id => ({candidate_id: id, operation_id: `operation-${id}`, output_ref: `refs/heads/${id}`,
  base: "base", tree: `tree-${id}`, diff: `diff-${id}`, destination: {host: "host.test", repository_identity: {common_inode: 12345}}, signing: {required: false}});
function setup() {
  const doc = {createElement: tag => new Element(tag, doc)};
  const root = new Element("section", doc);
  const calls = [];
  PbpsCompose.mount(root, (action, body) => new Promise((resolve, reject) => calls.push({action, body, resolve, reject})),
    {message: "Rename", from: "dbo.t.id", to: "ident", remote: "origin", remote_base_ref: "refs/heads/master"});
  return {root, calls, form: root.find(e => e.tagName === "form"),
    confirm: root.find(e => e.dataset.action === "confirm"), field: name => root.find(e => e.name === name)};
}
async function exercise() {
  // An edit while capture is in flight invalidates that response.
  const a = setup();
  const pending = a.form.fire("submit");
  a.field("message").value = "Changed";
  await a.form.fire("input");
  a.calls[0].resolve(preview("old")); await pending;
  assert.equal(a.confirm.disabled, true);
  await a.confirm.fire("click"); assert.equal(a.calls.length, 1);

  // Out-of-order responses cannot replace the most recently requested preview.
  const first = a.form.fire("submit");
  const second = a.form.fire("submit");
  a.calls[2].resolve(preview("new")); await second;
  a.calls[1].resolve(preview("stale")); await first;
  assert.equal(a.confirm.disabled, false);
  assert(!a.root.find(e => e.textContent.includes("common_inode")));
  assert(a.root.find(e => e.textContent === "diff-new"));
  const confirming = a.confirm.fire("click");
  await a.confirm.fire("click");
  await a.form.fire("submit");
  assert.equal(a.calls.length, 4);
  assert.deepEqual(a.calls[3].body, {candidate_id: "new"});
  assert.equal(a.calls[3].action, "confirm");
  assert.equal(a.field("message").disabled, true);
  a.calls[3].resolve({status: "published", operation_id: "operation-new", local: "present", remote: "not_attempted"}); await confirming;
  await a.confirm.fire("click"); assert.equal(a.calls.length, 4);

  // Invalidating a ready preview clears its rendered diff; errors never restore
  // the old handle, and an uncertain confirmation cannot retry publication.
  const b = setup();
  let p = b.form.fire("submit"); b.calls[0].resolve(preview("ready")); await p;
  b.field("kind").value = "drop"; b.field("column").value = "dbo.t.old";
  b.field("reason").value = "retired"; await b.form.fire("change");
  assert.equal(b.confirm.disabled, true);
  assert(!b.root.find(e => e.textContent === "diff-ready"));
  p = b.form.fire("submit");
  assert.deepEqual(b.calls[1].body.intent, {kind: "drop", column: "dbo.t.old", reason: "retired"});
  b.calls[1].reject(new Error("helper diagnostic FAKE_SECRET")); await p;
  assert.equal(b.confirm.disabled, true);
  assert(!b.root.find(e => e.textContent.includes("FAKE_SECRET")));
  p = b.form.fire("submit"); b.calls[2].resolve(preview("retry")); await p;
  p = b.confirm.fire("click"); b.calls[3].reject(new Error("transport unavailable")); await p;
  await b.confirm.fire("click"); await b.form.fire("submit");
  assert.equal(b.calls.length, 4);
  assert(b.root.find(e => e.textContent.includes("outcome is unknown")));
}
async function outcomes() {
  const a = setup();
  const button = name => a.root.find(e => e.dataset.action === name);
  const visible = text => a.root.find(e => e.textContent.includes(text));
  let p = a.form.fire("submit"); a.calls[0].resolve(preview("one")); await p;
  const receipt = {status: "recovery_required", operation_id: "operation-one", local: "present",
    remote: "unknown", cleanup_pending: true, problem: "persistence_uncertain",
    details: {commit: "a".repeat(40), output_ref: "refs/heads/pbps-compose/" + "b".repeat(64), base: "base", tree: "tree",
      destination: {host: "reviewed.test", repository_identity: {common_inode: 12345}}, source_project: "/source/project", project_suffix: "project", delivery_generation: "nonce-one"}};
  p = a.confirm.fire("click"); a.calls[1].resolve(receipt); await p;
  assert(visible("Commit: " + "a".repeat(40)));
  assert(visible("Remote publication is uncertain"));
  assert(!visible("common_inode"));
  assert(visible("Private cleanup remains pending"));
  assert(visible("git worktree add NEW_DIRECTORY"));
  assert(!button("retry"));
  assert(button("recover"));
  assert(button("republish"));
  p = button("republish").fire("click");
  assert.deepEqual(a.calls[2].body, {operation_id: "operation-one", generation: "nonce-one"});
  a.calls[2].reject(new Error("FAKE_HELPER_SECRET")); await p;
  assert(!visible("FAKE_HELPER_SECRET"));
  assert(visible("Commit: " + "a".repeat(40)));
  p = button("republish").fire("click");
  assert.equal(a.calls[3].body.generation, "nonce-one");
  a.calls[3].resolve({...receipt, details: null, local: "unavailable", remote: "unavailable"}); await p;
  assert(visible("Commit: " + "a".repeat(40)));
  assert(!button("republish"));
  p = button("recover").fire("click");
  a.calls[4].resolve({...receipt, status: "delivered", remote: "delivered"}); await p;
  assert(!button("republish"));
  p = button("alternative").fire("click");
  assert.deepEqual(a.calls[5].body, {operation_id: "operation-one"});
  assert(a.field("message").disabled);
  a.calls[5].resolve({}); await p;
  assert(!a.field("message").disabled);
  assert(a.confirm.disabled);
  assert(visible("does not extend the prior result"));
  const b = setup();
  p = b.root.find(e => e.dataset.action === "list").fire("click");
  assert.equal(b.calls[0].action, "list");
  b.calls[0].resolve([receipt]); await p;
  assert(b.root.find(e => e.textContent.includes("Commit: " + "a".repeat(40))));
}
async function mergeRequests() {
  // A link only for a delivered result on a host whose request URL is known.
  const branch = "pbps-compose/" + "c".repeat(64);
  const cases = [
    [{transport: "https", host: "github.com", port: null, repository: "/team/repo.git"}, "delivered",
      `https://github.com/team/repo/compare/${branch}?expand=1`],
    [{transport: "scp", host: "github.com", port: null, principal: "git", repository: "team/repo.git"}, "delivered",
      `https://github.com/team/repo/compare/${branch}?expand=1`],
    [{transport: "ssh", host: "gitlab.com", port: null, principal: "git", repository: "/group/sub/repo.git"}, "delivered",
      `https://gitlab.com/group/sub/repo/-/merge_requests/new?merge_request%5Bsource_branch%5D=${encodeURIComponent(branch)}`],
    [{transport: "https", host: "git.example.test", port: null, repository: "/team/repo.git"}, "delivered", null],
    [{transport: "https", host: "github.com", port: 8443, repository: "/team/repo.git"}, "delivered", null],
    [{transport: "https", host: "github.com", port: null, repository: "/team/../other/repo.git"}, "delivered", null],
    [{transport: "file", host: null, port: null, repository: "/srv/repo.git"}, "delivered", null],
    [{transport: "https", host: "github.com", port: null, repository: "/team/repo.git"}, "unknown", null],
  ];
  for (const [destination, remote, expected] of cases) {
    const a = setup();
    let p = a.form.fire("submit"); a.calls[0].resolve(preview("mr")); await p;
    p = a.confirm.fire("click");
    a.calls[1].resolve({status: "delivered", operation_id: "operation-mr", local: "present", remote, cleanup_pending: false,
      details: {commit: "d".repeat(40), output_ref: `refs/heads/${branch}`, base: "base", tree: "tree", destination,
        source_project: "/source/project", project_suffix: "project", delivery_generation: "nonce"}});
    await p;
    const link = a.root.find(e => e.tagName === "a");
    const label = JSON.stringify([destination, remote]);
    if (expected) {
      assert(link, label);
      assert.equal(link.href, expected, label);
      assert.equal(link.rel, "noreferrer noopener", label);
    } else {
      assert(!link, label);
      const hint = a.root.find(e => e.textContent.includes(`merge request for branch ${branch}`));
      assert.equal(Boolean(hint), remote === "delivered", label);
    }
  }
}
async function staleRefusal() {
  // A refusal no reconfirmation can cure reopens the form for a new preview.
  const a = setup();
  let p = a.form.fire("submit"); a.calls[0].resolve(preview("stale")); await p;
  p = a.confirm.fire("click");
  a.calls[1].resolve({status: "refused", operation_id: "operation-stale", local: "not_attempted",
    remote: "not_attempted", problem: "remote_base_changed", cleanup_pending: false, details: null});
  await p;
  assert(a.confirm.disabled, "a released candidate cannot be reconfirmed");
  assert(!a.root.find(e => e.dataset.action === "preview").disabled, "Refresh is available again");
  assert(!a.field("message").disabled, "inputs are editable again");
  assert(a.root.find(e => e.textContent.includes("preview a new candidate")));
  p = a.form.fire("submit");
  assert.equal(a.calls[2].action, "preview");
  a.calls[2].resolve(preview("fresh")); await p;
  assert.equal(a.confirm.disabled, false);
}
async function retirement() {
  const a = setup();
  const button = name => a.root.find(e => e.dataset.action === name);
  const visible = text => a.root.find(e => e.textContent.includes(text));
  const delivered = {status: "delivered", operation_id: "operation-retire", local: "present", remote: "delivered",
    cleanup_pending: true, details: {commit: "e".repeat(40), output_ref: "refs/heads/pbps-compose/" + "f".repeat(64),
      base: "base", tree: "tree", destination: {transport: "file", repository: "/srv/repo.git"},
      source_project: "/source/project", project_suffix: "project", delivery_generation: "nonce"}};
  let p = a.form.fire("submit"); a.calls[0].resolve(preview("retire")); await p;
  p = a.confirm.fire("click"); a.calls[1].resolve(delivered); await p;
  // Cleanup only while a delivered result still owns private resources.
  p = button("cleanup").fire("click");
  assert.deepEqual(a.calls[2].body, {operation_id: "operation-retire"});
  a.calls[2].resolve({...delivered, cleanup_pending: false}); await p;
  assert(!button("cleanup"), "nothing left to clean up");
  // Forgetting takes two deliberate clicks and says what it retires.
  assert(!button("forget"));
  await button("forget-ask").fire("click");
  assert(visible("retire its commit root"));
  p = button("forget").fire("click");
  assert.equal(a.calls[3].action, "forget");
  assert.deepEqual(a.calls[3].body, {operation_id: "operation-retire", acknowledged: true});
  a.calls[3].resolve({operation_id: "operation-retire", state: "spent", cleanup_pending: false}); await p;
  assert(!visible("Operation: operation-retire"), "a forgotten receipt leaves the list");
  // Forgetting this workflow's own result releases the form, as the server does.
  assert(!button("preview").disabled, "Refresh is available after forgetting");
  assert(!a.field("message").disabled, "inputs are editable after forgetting");
  assert(a.confirm.disabled);
  // Unresolved results offer neither action.
  const b = setup();
  p = b.root.find(e => e.dataset.action === "list").fire("click");
  b.calls[0].resolve([{...delivered, status: "recovery_required", remote: "unknown"}]); await p;
  assert(!b.root.find(e => e.dataset.action === "cleanup"));
  assert(!b.root.find(e => e.dataset.action === "forget-ask"));
  // Private resources are listed with their obligation and can be retired.
  p = b.root.find(e => e.dataset.action === "resources").fire("click");
  assert.equal(b.calls[1].action, "resources");
  b.calls[1].resolve([
    {operation_id: "sealed-one", state: "sealed", cleanup_pending: true, instruction: "Retry only the recorded retirement."},
    {operation_id: "confirmed-one", state: "confirmed", cleanup_pending: true, instruction: "Clean up from its result."},
    {operation_id: "spent-one", state: "spent", cleanup_pending: false, instruction: "none"},
  ]); await p;
  // Only a state recover-resources can advance offers the control.
  const controls = () => {
    const found = [];
    const walk = e => { if (e.dataset.action === "recover-resources") found.push(e); e.children.forEach(walk); };
    walk(b.root);
    return found;
  };
  assert.equal(controls().length, 1, "confirmed and spent reports offer no retirement");
  assert(controls()[0].textContent.includes("if it has expired"));
  // An unexpired preview is kept and the page says why.
  p = controls()[0].fire("click");
  assert.deepEqual(b.calls[2].body, {operation_id: "sealed-one"});
  b.calls[2].resolve({operation_id: "sealed-one", state: "sealed", cleanup_pending: true, instruction: "kept"}); await p;
  assert(b.root.find(e => e.textContent.includes("not reached its 24-hour expiry")));
  // An expired preview retires.
  p = controls()[0].fire("click");
  b.calls[3].resolve({operation_id: "sealed-one", state: "spent", cleanup_pending: false, instruction: "none"}); await p;
  assert.equal(controls().length, 0);
  // Retiring this workflow's own expired preview releases the page too.
  const c = setup();
  p = c.form.fire("submit"); c.calls[0].resolve(preview("expired")); await p;
  assert.equal(c.confirm.disabled, false);
  p = c.root.find(e => e.dataset.action === "resources").fire("click");
  c.calls[1].resolve([{operation_id: "operation-expired", state: "sealed", cleanup_pending: true, instruction: "kept"}]); await p;
  p = c.root.find(e => e.dataset.action === "recover-resources").fire("click");
  c.calls[2].resolve({operation_id: "operation-expired", state: "spent", cleanup_pending: false, instruction: "none"}); await p;
  assert(c.confirm.disabled, "a retired preview cannot be confirmed");
  assert(!c.root.find(e => e.dataset.action === "preview").disabled);
}
async function definiteRefusals() {
  for (const problem of ["remote_unavailable", "signing_unavailable"]) {
    const a = setup();
    let p = a.form.fire("submit"); a.calls[0].resolve(preview("frozen")); await p;
    p = a.confirm.fire("click");
    a.calls[1].resolve({status: "refused", operation_id: "operation-frozen", local: "not_attempted",
      remote: "not_attempted", cleanup_pending: false, problem}); await p;
    assert.equal(a.confirm.disabled, false, "definite refusal permits confirming the retained candidate");
    assert(a.field("message").disabled, "refusal does not reopen bound fields");
    assert(a.root.find(e => e.dataset.action === "preview").disabled);
    assert(a.root.find(e => e.textContent === "diff-frozen"));
    assert(!a.root.find(e => e.dataset.action === "recover"), "a refusal has no receipt to recover");
    await a.form.fire("submit");
    await a.form.fire("change");
    assert.equal(a.calls.length, 2, "a refused confirmed workflow cannot replace its frozen preview");
    p = a.confirm.fire("click");
    await a.confirm.fire("click");
    assert.equal(a.calls.length, 3, "repeat clicks cannot duplicate an in-flight confirmation");
    assert.deepEqual(a.calls[2].body, {candidate_id: "frozen"});
    assert.equal(a.calls[2].action, "confirm");
    if (problem === "remote_unavailable") {
      a.calls[2].resolve({status: "delivered", operation_id: "operation-frozen", local: "present", remote: "delivered"});
    } else {
      a.calls[2].reject(new Error("acknowledgement lost after reconfirmation"));
    }
    await p;
    assert(a.confirm.disabled, "success or uncertainty must not re-enable confirmation");
    await a.confirm.fire("click");
    assert.equal(a.calls.length, 3);
    assert(a.root.find(e => e.dataset.action === "recover"));
  }
}
exercise().then(outcomes).then(definiteRefusals).then(staleRefusal).then(mergeRequests).then(retirement).then(() => process.stdout.write("compose browser behavior passed\n"), error => { console.error(error); process.exitCode = 1; });
