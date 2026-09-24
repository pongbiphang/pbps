/* Immutable candidate form, mounted by app.js over the fixed compose actions
 * (#494). Tests execute this shipped module with a publisher stub. */
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
    const remoteContract = make("p", "HTTP/SSH destinations must use ordinary branches. Symbolic branches and server-side branch remapping are unsupported; Git advertisements cannot reliably reveal them.");
    form.append(refresh, confirm);
    const results = make("section");
    const saved = make("button", "Find saved results");
    saved.type = "button";
    saved.dataset.action = "list";
    root.replaceChildren(disclosure, remoteContract, form, activity, details, diff, saved, results);
    let generation = 0;
    let candidate = null;
    let confirming = false;
    let reconfirmable = false;
    let operation = null;
    const receipts = new Map();
    // Refusals no reconfirmation can cure; the server releases the handle.
    const stale = ["remote_base_changed", "destination_changed", "signing_changed", "repository_changed"];
    const destinationText = destination => {
      const {transport, host, port, principal, repository} = destination;
      return JSON.stringify({transport, host, port, principal, repository});
    };
    const quote = value => "'" + value.replaceAll("'", "'\"'\"'") + "'";
    // A merge-request link only where its URL shape is known. Any other host
    // gets the branch name; a guessed URL could open someone else's page.
    const mergeRequest = (destination, branch) => {
      const {transport, host, port, repository} = destination || {};
      // scp is `git@host:owner/repo.git`; an explicit port is some other service.
      if (!["https", "ssh", "scp"].includes(transport) || port != null || typeof repository !== "string") return null;
      const path = repository.replace(/^\//, "").replace(/\.git$/, "");
      if (host === "github.com" && /^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/.test(path)) {
        // The compare path keeps the branch's own slashes.
        const name = branch.split("/").map(encodeURIComponent).join("/");
        return `https://github.com/${path}/compare/${name}?expand=1`;
      }
      if (host === "gitlab.com" && /^[A-Za-z0-9._-]+(\/[A-Za-z0-9._-]+)+$/.test(path)) {
        return `https://gitlab.com/${path}/-/merge_requests/new?merge_request%5Bsource_branch%5D=${encodeURIComponent(branch)}`;
      }
      return null;
    };
    const statusText = {
      refused: "Publication refused before an attempt.",
      preparation_unknown: "Commit preparation is unresolved. Preserve the operation for recovery.",
      prepared: "The exact commit is prepared; local publication has not been attempted.",
      publication_unknown: "Local publication outcome is unknown. Reconcile before continuing.",
      published: "The recorded commit is published locally.",
      delivered: "The recorded commit is present locally and at the reviewed destination.",
      recovery_required: "Recovery is required. Preserve the operation and its evidence.",
    };
    const remoteText = {
      not_attempted: "Remote publication has not been attempted.",
      unknown: "Remote publication is uncertain. An absent branch may have been independently deleted.",
      delivered: "The remote branch contains the recorded commit.",
      changed: "The remote branch has changed or was deleted.",
      unavailable: "The remote branch cannot currently be verified.",
    };
    const render = result => {
      if (!Object.hasOwn(statusText, result.status) || typeof result.operation_id !== "string") {
        throw new Error("Unsupported publication result");
      }
      const old = receipts.get(result.operation_id);
      // A failed receipt read must not erase commit details already displayed.
      const evidence = result.details || old?.details;
      receipts.set(result.operation_id, {...result, details: evidence});
      results.replaceChildren();
      for (const receipt of receipts.values()) {
        const card = make("article");
        card.append(make("p", statusText[receipt.status]), make("p", `Operation: ${receipt.operation_id}`));
        card.append(make("p", remoteText[receipt.remote] || "Remote state is unavailable."));
        if (receipt.problem) card.append(make("p", receipt.status !== "refused"
          ? "An operation check failed. Reconcile the saved result before proceeding."
          : stale.includes(receipt.problem)
            ? "No publication was attempted. The reviewed base, destination, signing policy or repository changed; preview a new candidate."
            : "No publication was attempted. Restore the unavailable prerequisite, then confirm this same frozen candidate again."));
        if (receipt.cleanup_pending) card.append(make("p", "Private cleanup remains pending; keep this receipt."));
        const d = receipt.details;
        if (d) {
          card.append(make("pre", `Commit: ${d.commit || "not yet recorded"}\nOutput branch: ${d.output_ref}\nParent: ${d.base}\nTree: ${d.tree}\nDestination: ${destinationText(d.destination)}\nSource project: ${d.source_project}`));
          if (receipt.local === "present" && /^[a-f0-9]{40}([a-f0-9]{24})?$/.test(d.commit) && /^refs\/heads\/pbps-compose\/[a-f0-9]{64}$/.test(d.output_ref)) {
            card.append(make("p", "Continue from this result in a separate checkout. Run from the source repository, replacing NEW_DIRECTORY with an unused path; then open its project directory shown below."));
            card.append(make("pre", `git worktree add NEW_DIRECTORY ${quote(d.output_ref.slice("refs/heads/".length))}\npbps --project ${quote("NEW_DIRECTORY" + (d.project_suffix ? "/" + d.project_suffix : ""))} ui`));
          }
          if (receipt.remote === "delivered" && /^refs\/heads\/pbps-compose\/[a-f0-9]{64}$/.test(d.output_ref)) {
            const branch = d.output_ref.slice("refs/heads/".length);
            const url = mergeRequest(d.destination, branch);
            if (url) {
              const link = make("a", "Open a merge request for this branch");
              link.href = url;
              link.rel = "noreferrer noopener";
              link.target = "_blank";
              card.append(link);
            } else {
              card.append(make("p", `Open a merge request for branch ${branch} on your hosting service; this viewer only links hosts whose request URL it knows.`));
            }
          }
        }
        const action = (name, label, body) => {
          const button = make("button", label);
          button.type = "button";
          button.dataset.action = name;
          let busy = false;
          button.addEventListener("click", async () => {
            if (busy || button.disabled) return;
            busy = true;
            button.disabled = true;
            try {
              const next = await send(name, body);
              if (name === "alternative") {
                // The server must admit the old base and this workflow first.
                confirming = false;
                reconfirmable = false;
                operation = null;
                candidate = null;
                refresh.disabled = false;
                for (const field of Object.values(fields)) field.disabled = false;
                invalidate();
                activity.textContent = "Start an alternative from the original base. Preview and review a new candidate; this does not extend the prior result.";
              } else render(next);
            } catch (_) {
              activity.textContent = "The request outcome is unknown. Reconcile the saved operation before continuing.";
              // A failed explicit action can be retried only through its old
              // generation; a new click never manufactures fresh authority.
              busy = false;
              button.disabled = false;
            }
          });
          card.append(button);
        };
        if (receipt.status !== "refused") {
          action("recover", "Reconcile saved result", {operation_id: receipt.operation_id});
        }
        if (receipt.status === "prepared" || (receipt.local === "present" && receipt.remote === "not_attempted")) {
          action("retry", "Continue unattempted publication", {operation_id: receipt.operation_id});
        }
        if (receipt.local === "present" && d?.delivery_generation && ["unknown", "changed", "unavailable"].includes(receipt.remote)) {
          card.append(make("p", "Republish only after diagnosing the remote state: this explicitly authorizes recreating an absent output branch with the same commit at the same destination."));
          action("republish", "Authorize republishing this exact commit", {operation_id: receipt.operation_id, generation: d.delivery_generation});
        }
        if (receipt.local === "present") {
          action("alternative", "Start an alternative from the original base", {operation_id: receipt.operation_id});
        }
        results.append(card);
      }
    };
    saved.addEventListener("click", async () => {
      if (saved.disabled) return;
      saved.disabled = true;
      try {
        const list = await send("list", {});
        for (const result of list) render(result);
        if (!list.length) activity.textContent = "No saved publication results were found.";
      } catch (_) {
        activity.textContent = "Saved results could not be read. Existing evidence has been preserved.";
      } finally { saved.disabled = false; }
    });
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
        operation = preview.operation_id;
        details.textContent = `Output branch: ${preview.output_ref}\nParent: ${preview.base}\nTree: ${preview.tree}\nDestination: ${destinationText(preview.destination)}\nSigning: ${JSON.stringify(preview.signing)}\nMessage: ${request.message}`;
        diff.textContent = preview.diff;
        activity.textContent = "Review this frozen candidate. Later file edits require Refresh.";
        confirm.disabled = false;
      } catch (_) {
        if (mine !== generation || confirming) return;
        activity.textContent = "Preview failed. Check the inputs with the CLI, then refresh.";
      }
    });
    confirm.addEventListener("click", async () => {
      if (!candidate || (confirming && !reconfirmable) || confirm.disabled) return;
      confirming = true;
      reconfirmable = false;
      confirm.disabled = true;
      refresh.disabled = true;
      for (const field of Object.values(fields)) field.disabled = true;
      activity.textContent = "Confirming the reviewed candidate…";
      try {
        // Only the opaque server handle crosses this boundary. The backend
        // owns the frozen tree, intent, message and destination.
        const result = await send("confirm", {candidate_id: candidate});
        render(result);
        activity.textContent = statusText[result.status];
        if (result.status === "refused" && result.operation_id === operation && stale.includes(result.problem)) {
          // The reviewed base, destination, signing policy or repository
          // changed: the server released this candidate, so review a new one.
          confirming = false;
          candidate = null;
          refresh.disabled = false;
          for (const field of Object.values(fields)) field.disabled = false;
          confirm.disabled = true;
          activity.textContent = "What this candidate was reviewed against has changed. Preview again to review a new candidate.";
        } else if (result.status === "refused" && result.operation_id === operation) {
          // A definite refusal has no receipt to recover. Keep the inputs and
          // preview frozen; only reconfirmation of this same handle is enabled.
          reconfirmable = true;
          confirm.disabled = false;
        }
      } catch (_) {
        activity.textContent = "Confirmation outcome is unknown. Inspect the operation result before continuing.";
        render({status: "recovery_required", operation_id: operation, local: "unavailable",
          remote: "unavailable", cleanup_pending: true});
      }
    });
  },
});
