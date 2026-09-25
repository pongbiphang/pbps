// Runs the actual shipped app.js in Node after a prelude that defines
// `RESPONSES`: the exact bytes the viewer's server sends for each read. This
// DOM implements only the element and event plumbing app.js touches; the
// response is parsed as `response.json()` parses it, by JSON.parse on its text.
const assert = require("node:assert/strict");
class Element {
  constructor(tag) {
    this.tagName = tag; this.children = []; this.dataset = {}; this.listeners = {};
    this.textContent = ""; this.value = ""; this.hidden = false; this.className = "";
  }
  append(...elements) { this.children.push(...elements); }
  replaceChildren(...elements) { this.children = elements; }
  setAttribute(name, value) { this[name] = value; }
  removeAttribute(name) { delete this[name]; }
  addEventListener(name, handler) { (this.listeners[name] ||= []).push(handler); }
  fire(name) { for (const handler of this.listeners[name] || []) handler({preventDefault() {}}); }
  *walk() { yield this; for (const child of this.children) yield* child.walk(); }
}
const elements = {};
const nav = ["status", "drift", "plan", "timeline", "docs", "compose", "deploy"].map(view => {
  const button = new Element("button"); button.dataset.view = view; return button;
});
globalThis.location = {hash: "#token"};
globalThis.document = {
  getElementById: id => (elements[id] ||= new Element("div")),
  createElement: tag => new Element(tag),
  querySelectorAll: selector => (selector === "nav button" ? nav : []),
  querySelector: () => new Element("a"),
};
const requested = [];
globalThis.fetch = async url => {
  requested.push(url);
  const view = url.slice("/api/".length).split("?")[0];
  const text = RESPONSES[view];
  assert(text !== undefined, `no response prepared for ${url}`);
  return {ok: true, text: async () => text, json: async () => JSON.parse(text)};
};
