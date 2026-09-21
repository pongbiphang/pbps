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
exercise().then(outcomes).then(() => process.stdout.write("compose browser behavior passed\n"), error => { console.error(error); process.exitCode = 1; });
