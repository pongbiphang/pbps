// Runs after the shipped trigger.js. The DOM implements only what trigger.js
// touches. The server is a stand-in that records every POST body.
const assert = require("node:assert/strict");
class Element {
  constructor(tag) {
    this.tagName = tag; this.children = []; this.listeners = {};
    this.textContent = ""; this.value = ""; this.checked = false; this.className = "";
  }
  append(...elements) { this.children.push(...elements); }
  replaceChildren(...elements) { this.children = elements; }
  addEventListener(name, handler) { (this.listeners[name] ||= []).push(handler); }
  fire(name) { for (const handler of this.listeners[name] || []) handler({preventDefault() {}}); }
  *walk() { yield this; for (const child of this.children) yield* child.walk(); }
}
globalThis.document = {createElement: tag => new Element(tag)};
globalThis.setTimeout = () => 0; // no polling in this test; each step asks for runs itself
const settle = () => new Promise(resolve => setImmediate(resolve));

const posted = [];
let runs = [];
const post = async (action, body) => {
  posted.push([action, JSON.parse(JSON.stringify(body))]);
  if (action === "plan") runs = [{environment: body.environment, action: "plan",
    arguments: ["plan", `--env=${body.environment}`, `--out=${body.out}`], ended: true, code: 0,
    stdout: "Plan written; checksum 1234abcd\n", stderr: "", truncated: false}];
  return runs;
};
const reads = [];
const root = new Element("section");
globalThis.PbpsTrigger.mount(root, post, (view, value) => reads.push([view, value]));
const [planForm, applyForm] = root.children;
const input = (form, name) => [...form.walk()].find(e => e.tagName === "input" && e.name === name);
const checksum = input(applyForm, "checksum");

(async () => {
  await settle();
  assert.equal(checksum.value, "", "the checksum field starts empty");
  assert.equal(checksum.autocomplete, "off");

  input(planForm, "environment").value = "production";
  input(planForm, "out").value = "plans/release.json";
  planForm.fire("submit");
  await settle();
  assert.deepEqual(posted[posted.length - 2], ["plan", {environment: "production", out: "plans/release.json"}]);
  // A finished plan, whose output even names a checksum, fills nothing.
  assert.equal(checksum.value, "", "a plan run never fills the checksum");
  assert.equal(input(applyForm, "plan").value, "");
  assert.equal(input(applyForm, "allow").value, "");
  assert.equal(input(applyForm, "staged").checked, false);

  const button = [...root.walk()].find(e => e.tagName === "button" && e.textContent === "Read the plan");
  button.fire("click");
  assert.deepEqual(reads, [["plan", "plans/release.json"]]);

  input(applyForm, "environment").value = "production";
  input(applyForm, "plan").value = "plans/release.json";
  checksum.value = "typed-by-a-person";
  input(applyForm, "allow").value = " rename , destructive ,";
  applyForm.fire("submit");
  await settle();
  const apply = posted.find(([action]) => action === "apply");
  assert.deepEqual(apply, ["apply", {environment: "production", plan: "plans/release.json",
    checksum: "typed-by-a-person", allow: ["rename", "destructive"], staged: false, resume: false}]);
  console.log("trigger browser behavior passed");
})().catch(error => { console.error(error); process.exit(1); });
