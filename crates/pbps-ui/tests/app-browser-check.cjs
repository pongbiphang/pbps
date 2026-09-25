// Runs after app.js: drives the status page, then each environment's timeline
// and drift reads, and asserts the identifiers as rendered text.
const settle = () => new Promise(resolve => setImmediate(resolve));
const texts = () => [...elements.content.walk()].map(e => String(e.textContent));
(async () => {
  await settle();
  // Absent stays absent: rendered as not provided, never as a zero.
  const shown = texts();
  for (const id of EXPECTED.status) assert(shown.includes(id), `status last_entry ${id} in ${shown}`);
  const absent = shown.findIndex((text, i) => text === "last entry" && shown[i + 1] === "Not provided");
  assert(absent >= 0, "an absent last_entry is shown as not provided");

  const button = text => [...elements.content.walk()].find(e => e.tagName === "button" && e.textContent === text);
  button("Read timeline").fire("click");
  await settle();
  const timeline = texts();
  for (const id of EXPECTED.timeline) {
    assert(timeline.includes(`#${id} · apply`), `timeline title #${id} in ${timeline}`);
    assert(timeline.includes(id), `timeline id field ${id}`);
  }

  // Back to status, then drift: the baseline id is nested, so it is displayed
  // inside the baseline object's JSON text.
  nav[0].fire("click");
  await settle();
  button("Check drift").fire("click");
  await settle();
  const drift = texts().join("\n");
  assert(drift.includes(`"entry_id": "${EXPECTED.drift}"`), drift);

  assert.deepEqual(requested.map(url => url.split("?")[0]), ["/api/status", "/api/timeline", "/api/status", "/api/drift"]);
  console.log("app browser behavior passed");
})().catch(error => { console.error(error); process.exit(1); });
