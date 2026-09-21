"use strict";
(() => {
  const token = location.hash.slice(1);
  const byId = id => document.getElementById(id);
  const content = byId("content"), findings = byId("findings"), activity = byId("activity");
  const views = {
    status: ["Environments", "See the recorded state and readiness of your environments."],
    drift: ["Drift", "Compare a live environment with its recorded schema."],
    plan: ["Saved plan", "Read the changes and risks in an existing plan file."],
    timeline: ["Timeline", "Browse the deployment history recorded in an environment."],
    docs: ["Schema & ERD", "Explore documentation and relationships from your declarations."],
    compose: ["Compose intent", "Record a rename or a drop as a commit on this branch."]
  };
  // The shape of each intent: what the two inputs mean, and how the commit
  // message is spelled. The second field of a drop is its reason, which the
  // CLI requires for the audit trail.
  const intents = {
    "rename": ["Old fully qualified column name", "New column name", (a, b) => `rename ${a} ${b}`],
    "rename-table": ["Old fully qualified table name", "New table name", (a, b) => `rename table ${a} ${b}`],
    "drop": ["Fully qualified column name", "Why it is being dropped", a => `drop ${a}`],
    "drop-table": ["Fully qualified table name", "Why it is being dropped", a => `drop table ${a}`],
    "rename-role": ["Old role name", "New role name", (a, b) => `rename role ${a} ${b}`],
    "drop-role": ["Role name", "Why it is being dropped", a => `drop role ${a}`]
  };
  let context = null, previewed = null;
  let current = "status", generation = 0;
  function node(tag, text, className) {
    const el = document.createElement(tag);
    if (text !== undefined) el.textContent = text;
    if (className) el.className = className;
    return el;
  }
  const label = key => key.replaceAll("_", " ");
  function fields(value) {
    const dl = node("dl");
    for (const [key, item] of Object.entries(value)) {
      dl.append(node("dt", label(key)), node("dd", item === null ? "Not provided" : typeof item === "object" ? JSON.stringify(item, null, 2) : String(item)));
    }
    return dl;
  }
  function badge(state) {
    const kind = ["ok", "ready"].includes(state) ? "good" : ["failed", "unreachable", "unanswerable", "drift"].includes(state) ? "bad" : "attention";
    return node("span", state, `badge ${kind}`);
  }
  function panel(title) {
    const box = node("article", undefined, "panel");
    if (title) box.append(node("h2", title));
    return box;
  }
  function empty(title, message) {
    const box = node("div", undefined, "empty");
    box.append(node("h2", title), node("p", message));
    content.append(box);
  }
  function renderFindings(report) {
    for (const finding of report.findings) {
      const box = node("article", undefined, `finding ${finding.severity}`);
      box.append(node("strong", `${finding.severity} · ${finding.id}`), node("p", finding.message));
      if (finding.location) box.append(node("p", `${finding.location.file}${finding.location.line ? `:${finding.location.line}` : ""}`, "muted"));
      if (finding.remedy) box.append(node("pre", finding.remedy));
      findings.append(box);
    }
  }
  function render(report) {
    renderFindings(report);
    activity.textContent = `${report.command} · ${report.result} · pbps ${report.tool_version} · envelope ${report.schema_version}`;
    const data = report.data;
    if (data === undefined || data === null) {
      empty("No report available", "The command's findings above explain what prevented this read.");
      return;
    }
    if (current === "status") {
      if (!data.length) return empty("No environments", "Configure environments in pbps.yml to see their state here.");
      const grid = node("div", undefined, "grid");
      for (const env of data) {
        const box = node("article", undefined, "card"), top = node("div", undefined, "card-top");
        top.append(node("h2", env.environment), badge(env.state));
        box.append(top);
        if (env.description) box.append(node("p", env.description, "muted"));
        if (env.detail) box.append(node("p", env.detail));
        if (env.lock_unknown) box.append(node("p", `Lock not determined: ${env.lock_unknown}`, "finding warning"));
        if (env.locked_by) box.append(node("p", `Lock held by ${env.locked_by}`, "finding warning"));
        const details = node("details");
        details.append(node("summary", "All environment details"), fields(env));
        box.append(details);
        const actions = node("div", undefined, "card-actions");
        for (const [view, text] of [["drift", "Check drift"], ["timeline", "Read timeline"]]) {
          const button = node("button", text);
          button.type = "button";
          button.addEventListener("click", () => select(view, env.environment));
          actions.append(button);
        }
        box.append(actions); grid.append(box);
      }
      content.append(grid);
    } else if (current === "plan") {
      const summary = panel(data.applyable ? "Deployment plan" : "Preview plan"), stats = node("div", undefined, "stats");
      for (const [key, text] of [["change_count", "changes"], ["table_count", "tables"], ["module_count", "modules"], ["role_count", "roles"]]) {
        const stat = node("div", undefined, "stat"); stat.append(node("strong", data[key]), node("span", text)); stats.append(stat);
      }
      summary.append(stats); content.append(summary);
      for (const risk of data.risks) {
        const box = panel(risk.class); box.append(node("p", risk.why));
        const list = node("ul"); for (const change of risk.changes) list.append(node("li", change));
        box.append(list); content.append(box);
      }
      const all = panel("Plan details"); all.append(fields(data)); content.append(all);
    } else if (current === "timeline") {
      const summary = panel(data.environment);
      summary.append(fields({initialized:data.initialized, limit:data.limit})); content.append(summary);
      if (!data.entries.length) empty(data.initialized ? "No recorded entries" : "Ledger not initialized", "This is the state reported by the command.");
      for (const entry of data.entries) {
        const box = panel(`#${entry.id} · ${entry.kind}`); box.append(fields(entry)); content.append(box);
      }
    } else {
      const box = panel(data.environment); box.append(fields(data)); content.append(box);
    }
  }
  function intentBody() {
    const kind = byId("compose-kind").value;
    const first = byId("compose-first").value, second = byId("compose-second").value;
    const intent = kind.startsWith("drop")
      ? {kind, [kind === "drop" ? "column" : kind === "drop-table" ? "table" : "role"]: first, reason: second}
      : {kind, from: first, to: second};
    return {
      intent,
      message: byId("compose-message").value,
      remote: byId("compose-remote").value,
      // What the preview read, sent back so the confirmation is checked
      // against the files as they are *now*: an editor that saved while you
      // were reading the diff is refused rather than overwritten.
      shown: previewed ? previewed.shown : {}
    };
  }
  function describeIntent() {
    const kind = byId("compose-kind").value, shape = intents[kind];
    byId("compose-first-label").textContent = shape[0];
    byId("compose-second-label").textContent = shape[1];
    byId("compose-message").value = shape[2](byId("compose-first").value, byId("compose-second").value);
    byId("compose-record").hidden = true;
    previewed = null;
  }
  function renderDiff(text) {
    const box = node("pre", undefined, "diff");
    for (const line of text.split("\n")) {
      const kind = line.startsWith("+") && !line.startsWith("+++") ? "add"
        : line.startsWith("-") && !line.startsWith("---") ? "remove" : "";
      box.append(node("span", line + "\n", kind));
    }
    return box;
  }
  async function composeRequest(route) {
    activity.className = ""; activity.textContent = "Working…";
    content.replaceChildren(); findings.replaceChildren();
    const response = await fetch(route, {
      method: "POST",
      headers: {"X-Pbps-Token": token, "Content-Type": "application/json"},
      cache: "no-store",
      credentials: "omit",
      body: JSON.stringify(intentBody())
    });
    const answer = await response.json();
    if (!answer.ok) {
      activity.className = "error"; activity.textContent = answer.refusal;
      empty("Nothing was changed", "The refusal above is the whole of what happened; your checkout is as it was.");
      return null;
    }
    return answer.data;
  }
  async function composeView() {
    content.replaceChildren(); findings.replaceChildren();
    activity.className = ""; activity.textContent = "Reading this checkout…";
    const response = await fetch("/api/compose", {headers: {"X-Pbps-Token": token}, cache: "no-store", credentials: "omit"});
    context = await response.json();
    const form = byId("compose");
    for (const recovered of context.recovered || []) {
      const box = node("article", undefined, "finding warning");
      box.append(node("strong", "A previous compose was interrupted"), node("p", JSON.stringify(recovered)));
      findings.append(box);
    }
    if (!context.available) {
      form.hidden = true;
      activity.className = "error"; activity.textContent = context.why || "Composing is not available here.";
      const box = node("div", undefined, "empty");
      box.append(node("h2", "Record this in a shell instead"));
      for (const line of context.commands || []) box.append(node("pre", line));
      content.append(box);
      return;
    }
    form.hidden = false;
    const remotes = byId("compose-remote");
    remotes.replaceChildren();
    for (const name of context.remotes) remotes.append(new Option(name, name));
    remotes.append(new Option("Do not push", ""));
    const warn = node("div", undefined, "warn");
    warn.textContent = `On ${context.branch}. Your git hooks do not run for a commit or a push made here — this tool keeps policy in files and in CI.`;
    content.append(warn);
    activity.textContent = "";
    describeIntent();
  }
  async function load() {
    const own = ++generation, view = current;
    content.replaceChildren(); findings.replaceChildren(); activity.className = "";
    if (!token) { activity.textContent = "Open the complete URL printed by pbps ui, including its fragment."; return; }
    if (view === "compose") { await composeView(); return; }
    let url = `/api/${view}`;
    if (["drift", "timeline", "plan"].includes(view)) {
      const value = byId("selection-value").value;
      if (!value) { activity.textContent = "Enter a value above to read this view."; return; }
      url += `?${view === "plan" ? "path" : "env"}=${encodeURIComponent(value)}`;
    }
    activity.textContent = "Reading…";
    try {
      const response = await fetch(url, {headers:{"X-Pbps-Token":token}, cache:"no-store", credentials:"omit"});
      if (!response.ok) throw new Error(await response.text());
      const result = view === "docs" ? await response.text() : await response.json();
      if (own !== generation) return;
      if (view === "docs") {
        const frame = node("iframe"); frame.title = "Schema documentation and ERD";
        frame.setAttribute("sandbox", ""); frame.srcdoc = result;
        content.append(frame); activity.textContent = "Documentation from the current declarations";
      } else render(result);
    } catch (error) {
      if (own !== generation) return;
      activity.className = "error"; activity.textContent = error.message;
      empty("This read could not complete", "Check the reported problem, then refresh to try again.");
    }
  }
  function select(view, value = "") {
    current = view; ++generation;
    byId("title").textContent = views[view][0]; byId("description").textContent = views[view][1];
    for (const button of document.querySelectorAll("nav button")) {
      if (button.dataset.view === view) button.setAttribute("aria-current", "page"); else button.removeAttribute("aria-current");
    }
    byId("compose").hidden = view !== "compose";
    byId("selection").hidden = !["drift", "timeline", "plan"].includes(view);
    byId("input-label").textContent = view === "plan" ? "Saved plan path" : "Environment name";
    byId("selection-value").value = value;
    byId("selection-value").placeholder = view === "plan" ? "plans/release.json" : "production";
    byId("read").textContent = view === "plan" ? "Read plan" : "Read environment";
    byId("input-help").textContent = view === "plan" ? "A file path on this machine, relative to the project directory or absolute." : "Use a configured environment name from pbps.yml.";
    load();
  }
  for (const button of document.querySelectorAll("nav button")) button.addEventListener("click", () => select(button.dataset.view));
  document.querySelector(".brand").addEventListener("click", event => { event.preventDefault(); select("status"); });
  byId("selection").addEventListener("submit", event => { event.preventDefault(); load(); });
  byId("compose-kind").addEventListener("change", describeIntent);
  for (const id of ["compose-first", "compose-second"]) byId(id).addEventListener("input", describeIntent);
  byId("compose").addEventListener("submit", async event => {
    event.preventDefault();
    const preview = await composeRequest("/api/compose/preview");
    if (!preview) return;
    previewed = preview;
    const box = panel(`What ${preview.branch} would gain`);
    box.append(renderDiff(preview.diff));
    const list = node("ul");
    for (const path of preview.paths) list.append(node("li", path));
    box.append(node("h3", "Files"), list);
    content.append(box);
    byId("compose-record").hidden = false;
    activity.textContent = "Nothing has been committed. Read the diff, then commit.";
  });
  byId("compose-record").addEventListener("click", async () => {
    if (!previewed) return;
    const composed = await composeRequest("/api/compose/record");
    if (!composed) return;
    byId("compose-record").hidden = true; previewed = null;
    const box = panel("Recorded");
    box.append(fields({
      commit: composed.commit,
      branch: composed.branch,
      signature: composed.signature || "not signed",
      pushed_to: composed.destination || (composed.push_refused ? "push failed" : "not pushed"),
      hooks_did_not_run: composed.hooks_did_not_run
    }));
    const list = node("ul");
    for (const path of composed.paths) list.append(node("li", path));
    box.append(node("h3", "Files in the commit"), list);
    if (composed.push_refused) {
      const warn = node("article", undefined, "finding error");
      warn.append(node("strong", "The commit is on your branch; the push did not happen"),
                  node("p", composed.push_refused),
                  node("p", "Push it yourself when the remote is reachable.", "muted"));
      findings.append(warn);
    }
    for (const kept of composed.retained) box.append(node("p", kept, "muted"));
    content.append(box);
    activity.textContent = `${composed.commit} on ${composed.branch}`;
  });
  byId("refresh").addEventListener("click", load);
  load();
})();
