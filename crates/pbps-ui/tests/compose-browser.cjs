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
  base: "base", tree: `tree-${id}`, diff: `diff-${id}`, destination: {host: "host.test"}, signing: {required: false}});
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
  assert(a.root.find(e => e.textContent === "diff-new"));
  const confirming = a.confirm.fire("click");
  await a.confirm.fire("click");
  await a.form.fire("submit");
  assert.equal(a.calls.length, 4);
  assert.deepEqual(a.calls[3].body, {candidate_id: "new"});
  assert.equal(a.calls[3].action, "confirm");
  assert.equal(a.field("message").disabled, true);
  a.calls[3].resolve({message: "Created refs/heads/new"}); await confirming;
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
exercise().then(() => process.stdout.write("compose browser behavior passed\n"), error => { console.error(error); process.exitCode = 1; });
