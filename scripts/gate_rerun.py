"""Evidence and single-use claims for independent gate failures (DEC-2651.1).

The operator reviews the complete module/fixture scope in `paths`; Git checks
overlap. Pass proofs are produced by `live-shards.py proof`, never inferred
from a successful neighbour or a register edit. See docs/GATE-RERUN.md.
"""

import hashlib
import json
import os
from pathlib import Path, PurePosixPath, PureWindowsPath
import re
import subprocess
import stat


def require(condition, message):
    if not condition:
        raise ValueError(message)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate rerun evidence field: {key}")
        result[key] = value
    return result


def read(path):
    try:
        return json.loads(Path(path).read_text(), object_pairs_hook=unique_object)
    except (OSError, ValueError) as error:
        raise ValueError(f"unreadable rerun evidence {path}: {error}") from error


def git(root, *args):
    result = subprocess.run(["git", *args], cwd=root, text=True, capture_output=True)
    require(result.returncode == 0, f"cannot read rerun Git evidence: {result.stderr.strip()}")
    return result.stdout if args[0] == "diff" or "-z" in args else result.stdout.strip()


def identity(cargo, name):
    return json.dumps([list(cargo), name], separators=(",", ":"))


class ScopePathError(ValueError):
    """A scope must be one canonical, literal entry in the tracked Git tree."""


def path_name(value, tracked):
    # Validate the spelling before normalization can erase a component. Compare
    # against literal Git inventory, never Git pathspecs or filesystem targets.
    if (not isinstance(value, str) or not value or value == "."
            or PurePosixPath(value).is_absolute() or PureWindowsPath(value).drive
            or ".." in PurePosixPath(value).parts or "\\" in value
            or any(c in value for c in "*?[]{}\0")
            or str(PurePosixPath(value)) != value):
        raise ScopePathError(f"invalid evidence scope path: {value!r}; use a literal normalized repo-relative path")
    path = PurePosixPath(value)
    if not any(overlaps(entry, path) for entry in tracked):
        raise ScopePathError(f"untracked evidence scope path: {value!r}")
    return path


def overlaps(changed, scope):
    # Components enforce '/' boundaries. The inferred parent of a root mod.rs
    # has no components and correctly protects the entire repository.
    return changed.parts[:len(scope.parts)] == scope.parts


def python_checks(root):
    """Whole Python steps of CI lint/quick, without a second command list."""
    from ignored_test_inventory import workflow_runs
    workflow = (root / ".github/workflows/ci.yml").read_text()
    checks = []
    for job in ("lint", "quick"):
        for command in workflow_runs(workflow, job):
            match = re.fullmatch(r"python3 (scripts/[A-Za-z0-9_-]+\.py)", command)
            if match:
                require((root / match[1]).is_file(), f"missing Python gate check: {match[1]}")
                if match[1] not in checks:
                    checks.append(match[1])
            else:
                require(not re.search(r"\bpython[0-9.]*\b", command),
                        f"Python gate step needs an explicit adapter: {command}")
    require(checks, "CI lint/quick contains no Python checks")
    return checks


def proof(path, root, commit, tree, inputs, cargo, name):
    data = read(path)
    require(isinstance(data, dict) and data.get("version") == 1
            and data.get("commit") == commit and data.get("tree") == tree
            and data.get("inputs") == inputs and data.get("cargo") == list(cargo)
            and data.get("test") == name and data.get("status") == "ok"
            and type(data.get("returncode")) is int and data["returncode"] == 0
            and data.get("valid") is True,
            f"pass proof does not prove this exact isolated check: {path}")
    require(git(root, "rev-parse", commit + "^{tree}") == tree,
            f"pass proof tree differs from its commit: {path}")
    return data


def load(path, root, tree, inputs):
    """Validate explicit evidence before any output, fixture or claim is made."""
    if path is None:
        return {}
    data = read(path)
    require(isinstance(data, dict) and set(data) == {"version", "pr", "names"}
            and data["version"] == 1 and isinstance(data["names"], list) and data["names"],
            "rerun evidence needs a nonempty names list")
    require(isinstance(data["pr"], str) and re.fullmatch(
        r"https://github\.com/[^/]+/[^/]+/pull/[1-9][0-9]*(?:#issuecomment-[0-9]+)?", data["pr"]),
        "rerun evidence must cite the PR recording the independence evidence")
    require(tree is not None and inputs is not None, "rerun needs a clean head and readable inputs")
    master = git(root, "rev-parse", "--verify", "origin/master^{commit}")
    head = git(root, "rev-parse", "--verify", "HEAD^{commit}")
    base = git(root, "merge-base", master, head)
    changed = [PurePosixPath(p) for p in git(root, "diff", "--no-renames", "--name-only", "-z", base, head).split("\0") if p]
    tracked = {PurePosixPath(p) for p in git(root, "ls-tree", "-r", "--full-tree", "--name-only", "-z", "HEAD").split("\0") if p}
    entries = {}
    for entry in data["names"]:
        require(isinstance(entry, dict) and set(entry) == {
            "cargo", "test", "source", "paths", "scope_review", "issue", "master_pass", "head_pass"},
            "each rerun name needs its selector, source, complete scope, issue and two pass proofs")
        cargo, name = entry["cargo"], entry["test"]
        require(isinstance(cargo, list) and all(isinstance(a, str) for a in cargo)
                and isinstance(name, str) and name, "invalid rerun selector")
        require(type(entry["issue"]) is int and entry["issue"] > 0,
                "each rerun name needs a tracking issue")
        require(isinstance(entry["scope_review"], str) and entry["scope_review"].strip(),
                "record the review of the complete module and fixture scope")
        source = path_name(entry["source"], tracked)
        require(isinstance(entry["paths"], list) and entry["paths"], "missing module/fixture scope")
        paths = [path_name(p, tracked) for p in entry["paths"]]
        require(source in paths, "scope must include the test source file")
        require(source in tracked, f"test source is not a tracked file: {source}")
        text = git(root, "show", "HEAD:" + str(source))
        if cargo[:1] == ["python"]:
            require(cargo == ["python", name] and name == str(source) and source.suffix == ".py",
                    "Python reruns are whole checks identified by their script path")
        else:
            require(cargo[:1] == ["test"] and cargo[-1:] == ["--"] and source.suffix == ".rs"
                    and re.search(r"\bfn\s+" + re.escape(name.split("::")[-1]) + r"\s*\(", text),
                    "cannot verify the Rust test source; provide its defining file")
        # A Rust file is a module together with its same-named directory. A
        # mod.rs owns its directory; the operator adds every external fixture.
        module = source.parent if source.name == "mod.rs" else source.with_suffix("")
        scope = [*paths, module, *(PurePosixPath(p) for p in (
                 "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo",
                 ".github/workflows/ci.yml", "scripts/live-shards.py", "scripts/gate_rerun.py"))]
        hits = [str(p) for p in changed if any(overlaps(p, s) for s in scope)]
        require(not hits, f"PR diff touches the test file, module, fixture or gate inputs: {hits}")
        evidence_dir = Path(path).resolve().parent
        require(all(isinstance(entry[p], str) and entry[p] for p in ("master_pass", "head_pass")),
                "missing isolated pass proof paths")
        passes = {"master": proof(evidence_dir / entry["master_pass"], root, master,
                  git(root, "rev-parse", master + "^{tree}"), inputs, cargo, name),
                  "head": proof(evidence_dir / entry["head_pass"], root, head, tree, inputs, cargo, name)}
        key = (tuple(cargo), name)
        require(key not in entries, "duplicate rerun name")
        entries[key] = dict(entry, pr=data["pr"], master=master, head=head, proofs=passes)
    return entries


def claim_path(root, tree, cargo, name):
    directory = Path(git(root, "rev-parse", "--path-format=absolute", "--git-common-dir")).resolve(strict=True)
    ledger = directory / "pbps-targeted-reruns"
    for parent in (ledger, ledger / tree):
        try:
            mode = parent.lstat().st_mode
        except FileNotFoundError:
            continue
        require(stat.S_ISDIR(mode), f"unreadable targeted-rerun ledger directory: {parent}")
    key = hashlib.sha256(identity(cargo, name).encode()).hexdigest()
    return ledger / tree / (key + ".json")


def claimed(root, tree, cargo, name):
    # Creation consumes the allowance even if killed before writing evidence.
    # lstat also treats an empty claim or a dangling symlink as consumed; only
    # absence permits a retry, and other I/O errors must refuse the resume.
    try:
        claim_path(root, tree, cargo, name).lstat()
    except FileNotFoundError:
        return False
    return True


def claim(root, tree, entries):
    """Reserve before starting; resuming an older output cannot mint a retry.

    Keep the ledger in the common Git directory so other worktrees and sibling
    resume chains share the allowance. A killed attempt consumes it as well.
    """
    for cargo, name in sorted(entries):
        path = claim_path(root, tree, cargo, name)
        path.parent.mkdir(parents=True, exist_ok=True)
        try:
            with path.open("x") as output:
                json.dump(entries[(cargo, name)], output, indent=2)
                output.write("\n")
                output.flush()
                os.fsync(output.fileno())
        except FileExistsError as error:
            raise ValueError(f"targeted rerun already attempted on this head: {name}") from error
