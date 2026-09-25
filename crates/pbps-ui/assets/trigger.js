"use strict";
// The deployment trigger (#1025): `plan --out` and `apply --plan`, each one
// fixed POST action. The page holds no approval: the checksum field starts
// empty, and nothing but the person typing into it ever sets it (DEC-1025.1).
globalThis.PbpsTrigger = (() => {
  function node(tag, text, className) {
    const el = document.createElement(tag);
    if (text !== undefined) el.textContent = text;
    if (className) el.className = className;
    return el;
  }
  function field(form, label, name, options = {}) {
    const row = node("label", label);
    const input = node("input");
    input.name = name;
    input.type = options.type || "text";
    input.autocomplete = "off";
    input.spellcheck = false;
    if (options.placeholder) input.placeholder = options.placeholder;
    if (options.required) input.required = true;
    row.append(input);
    form.append(row);
    return input;
  }
  function mount(root, post, read) {
    const status = node("p", "", "muted");
    const runs = node("div", undefined, "runs");
    let polling = false;

    const planForm = node("form", undefined, "panel");
    planForm.append(node("h2", "Plan against an environment"), node("p", "Runs pbps plan --env --out and writes a new saved plan file. Read it before anyone approves it.", "muted"));
    const planEnv = field(planForm, "Environment name", "environment", {placeholder: "production", required: true});
    const planOut = field(planForm, "New plan file", "out", {placeholder: "plans/release.json", required: true});
    const planButton = node("button", "Write plan"); planButton.type = "submit"; planForm.append(planButton);

    const applyForm = node("form", undefined, "panel");
    applyForm.append(node("h2", "Apply a saved plan"), node("p", "Runs pbps apply --plan with the checksum your deployment gate approved. The CLI refuses a plan whose checksum does not match.", "muted"));
    const applyEnv = field(applyForm, "Environment name", "environment", {placeholder: "production", required: true});
    const applyPlan = field(applyForm, "Saved plan file", "plan", {placeholder: "plans/release.json", required: true});
    const checksum = field(applyForm, "Approved checksum", "checksum", {placeholder: "SHA-256 approved at the deployment gate", required: true});
    const allow = field(applyForm, "Allowed risk classes", "allow", {placeholder: "rename,destructive (comma-separated, empty for none)"});
    const staged = field(applyForm, "Staged apply", "staged", {type: "checkbox"});
    const resume = field(applyForm, "Resume a stopped staged apply", "resume", {type: "checkbox"});
    const applyButton = node("button", "Apply plan"); applyButton.type = "submit"; applyForm.append(applyButton);

    function render(list) {
      runs.replaceChildren();
      for (const run of list) {
        const box = node("article", undefined, "panel");
        const state = !run.ended ? "running" : run.code === null ? "ended without an exit code" : `exited ${run.code}`;
        box.append(node("h2", `${run.environment} · ${run.action} · ${state}`));
        box.append(node("pre", `pbps ${run.arguments.join(" ")}`));
        if (run.stdout) box.append(node("pre", run.stdout));
        if (run.stderr) box.append(node("pre", run.stderr, "finding warning"));
        if (run.truncated) box.append(node("p", "Output was cut short; run the command in a terminal for all of it.", "muted"));
        if (run.ended) {
          const actions = node("div", undefined, "card-actions");
          const out = run.arguments.find(argument => argument.startsWith("--out="));
          const target = run.action === "plan" && run.code === 0 && out
            ? ["plan", out.slice("--out=".length), "Read the plan"]
            : ["timeline", run.environment, "Read timeline"];
          const button = node("button", target[2]); button.type = "button";
          button.addEventListener("click", () => read(target[0], target[1]));
          actions.append(button); box.append(actions);
        }
        runs.append(box);
      }
      return list.some(run => !run.ended);
    }
    async function refresh() {
      try {
        const running = render(await post("runs", {}));
        if (running && !polling) {
          polling = true;
          setTimeout(() => { polling = false; refresh(); }, 2000);
        }
      } catch (error) {
        status.className = "error"; status.textContent = error.message;
      }
    }
    async function submit(action, body) {
      status.className = "muted"; status.textContent = `Starting ${action}…`;
      try {
        render(await post(action, body));
        status.textContent = `${action} started against ${body.environment}`;
        refresh();
      } catch (error) {
        status.className = "error"; status.textContent = error.message;
      }
    }
    planForm.addEventListener("submit", event => {
      event.preventDefault();
      submit("plan", {environment: planEnv.value, out: planOut.value});
    });
    applyForm.addEventListener("submit", event => {
      event.preventDefault();
      const classes = allow.value.split(",").map(word => word.trim()).filter(word => word);
      submit("apply", {environment: applyEnv.value, plan: applyPlan.value, checksum: checksum.value,
        allow: classes, staged: staged.checked, resume: resume.checked});
    });
    root.append(planForm, applyForm, status, runs);
    refresh();
  }
  return {mount};
})();
