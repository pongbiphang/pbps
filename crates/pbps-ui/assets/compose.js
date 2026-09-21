/* Immutable candidate form. Mounted only after publication/recovery are
 * qualified (#748); tests execute this shipped module with a publisher stub. */
"use strict";
globalThis.PbpsCompose = Object.freeze({
  mount(root, send, defaults = {}) {
    const doc = root.ownerDocument;
    const make = (tag, text) => {
      const element = doc.createElement(tag);
      if (text !== undefined) element.textContent = text;
      return element;
    };
    const fields = {};
    const form = make("form");
    const kinds = {
      "rename": ["from", "to"], "rename-table": ["from", "to"],
      "rename-role": ["from", "to"], "drop": ["column", "reason"],
      "drop-table": ["table", "reason"], "drop-role": ["role", "reason"],
      "declarations": [],
    };
    const labels = {
      kind: "Intent", from: "Current name", to: "New name", column: "Column",
      table: "Table", role: "Role", reason: "Drop reason", message: "Commit message",
      remote: "Configured remote name", remote_base_ref: "Remote base branch (refs/heads/…)",
    };
    const wrappers = {};
    for (const [name, label] of Object.entries(labels)) {
      const wrapper = make("label", label);
      const field = make(name === "kind" ? "select" : "input");
      field.name = name;
      if (name === "kind") {
        for (const kind of Object.keys(kinds)) {
          const option = make("option", kind);
          option.value = kind;
          field.append(option);
        }
      } else {
        field.type = "text";
        field.autocomplete = "off";
        field.maxLength = name === "message" ? 16384 : 4096;
      }
      field.value = defaults[name] || (name === "kind" ? "rename" : "");
      wrapper.append(field);
      form.append(wrapper);
      fields[name] = field;
      wrappers[name] = wrapper;
    }
    const refresh = make("button", "Preview / Refresh");
    refresh.type = "submit";
    refresh.dataset.action = "preview";
    const confirm = make("button", "Confirm this candidate");
    confirm.type = "button";
    confirm.dataset.action = "confirm";
    confirm.disabled = true;
    const activity = make("p");
    activity.setAttribute("role", "status");
    const details = make("pre");
    const diff = make("pre");
    const disclosure = make("p", "This creates a new output branch. Your original declarations, old identity file, staged work and current branch stay as they are. Edits after Preview are excluded until Refresh. Client hooks will not run.");
    form.append(refresh, confirm);
    root.replaceChildren(disclosure, form, activity, details, diff);
    let generation = 0;
    let candidate = null;
    let confirming = false;
    const intentFields = new Set(Object.values(kinds).flat());
    const updateFields = () => {
      for (const name of intentFields) {
        wrappers[name].hidden = !(kinds[fields.kind.value] || []).includes(name);
      }
    };
    updateFields();
    const invalidate = () => {
      if (confirming) return;
      generation += 1;
      candidate = null;
      confirm.disabled = true;
      activity.textContent = "Inputs changed. Preview again before confirming.";
      details.textContent = "";
      diff.textContent = "";
      updateFields();
    };
    form.addEventListener("input", invalidate);
    form.addEventListener("change", invalidate);
    form.addEventListener("submit", async event => {
      event.preventDefault();
      if (confirming) return;
      invalidate();
      const mine = generation;
      const kind = fields.kind.value;
      if (!Object.hasOwn(kinds, kind)) return;
      const intent = {kind};
      for (const name of kinds[kind]) intent[name] = fields[name].value;
      const request = {intent, message: fields.message.value,
        remote: fields.remote.value, remote_base_ref: fields.remote_base_ref.value};
      activity.textContent = "Capturing and validating the candidate…";
      try {
        const preview = await send("preview", request);
        if (mine !== generation || confirming) return;
        candidate = preview.candidate_id;
        details.textContent = `Output branch: ${preview.output_ref}\nParent: ${preview.base}\nTree: ${preview.tree}\nDestination: ${JSON.stringify(preview.destination)}\nSigning: ${JSON.stringify(preview.signing)}\nMessage: ${request.message}`;
        diff.textContent = preview.diff;
        activity.textContent = "Review this frozen candidate. Later file edits require Refresh.";
        confirm.disabled = false;
      } catch (_) {
        if (mine !== generation || confirming) return;
        activity.textContent = "Preview failed. Check the inputs with the CLI, then refresh.";
      }
    });
    confirm.addEventListener("click", async () => {
      if (!candidate || confirming || confirm.disabled) return;
      confirming = true;
      confirm.disabled = true;
      refresh.disabled = true;
      for (const field of Object.values(fields)) field.disabled = true;
      activity.textContent = "Confirming the reviewed candidate…";
      try {
        // Only the opaque server handle crosses this boundary. The backend
        // owns the frozen tree, intent, message and destination.
        const result = await send("confirm", {candidate_id: candidate});
        activity.textContent = result.message;
      } catch (_) {
        // Publication may already have happened. #746 supplies the receipt
        // and reconcile action; this form must not send a second operation.
        activity.textContent = "Confirmation outcome is unknown. Inspect the operation result before continuing.";
      }
    });
  },
});
