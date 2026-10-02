"""Compiled-test discovery and bounded scheduling witnesses (DEC-1130.1)."""

import ast
import copy
import io
import json
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tokenize
import tomllib


class InventoryError(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise InventoryError(message)


def listed_tests(output):
    names = []
    for line in output.splitlines():
        require(line.endswith(": test"), f"unexpected libtest listing: {line!r}")
        names.append(line[:-6])
    require(len(names) == len(set(names)), "duplicate libtest case")
    return set(names)


def dep_paths(text):
    """Read rustc's first Make rule, preserving Windows path separators."""
    first = text.splitlines()[0] if text.splitlines() else ""
    _, separator, dependencies = first.partition(": ")
    require(separator, "missing dep-info dependency rule")
    paths, current, escaped = [], [], False
    for char in dependencies + " ":
        if escaped:
            current.extend([char] if char == " " else ["\\", char])
            escaped = False
        elif char == "\\":
            escaped = True
        elif char == " ":
            if current:
                paths.append("".join(current))
                current = []
        else:
            current.append(char)
    require(paths and not current and not escaped, "empty or malformed dep-info dependencies")
    return paths


def discover(root):
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version=1"], cwd=root, text=True, encoding="utf-8"))
    packages = {p["id"]: p for p in metadata["packages"]}
    built = subprocess.check_output(
        ["cargo", "test", "--workspace", "--all-targets", "--no-run", "--message-format=json"],
        cwd=root, text=True, encoding="utf-8")
    targets = {}
    for artifact in map(json.loads, built.splitlines()):
        if (artifact.get("reason") != "compiler-artifact" or not artifact.get("executable")
                or not artifact.get("profile", {}).get("test")):
            continue
        package = packages[artifact["package_id"]]
        target = artifact["target"]
        manifest = tomllib.loads(Path(package["manifest_path"]).read_text(encoding="utf-8"))
        declaration = manifest.get(target["kind"][0], [])
        declarations = [declaration] if isinstance(declaration, dict) else declaration
        require(not any(d.get("harness") is False and
                        (target["kind"][0] == "lib" or d.get("name") == target["name"])
                        for d in declarations),
                f"custom test harness needs a safe listing adapter: {package['name']}::{target['name']}")
        key = (package["name"], target["kind"][0], target["name"])
        require(key not in targets, f"duplicate test artifact: {key}")
        command = [artifact["executable"], "--list", "--format", "terse"]
        cwd = Path(package["manifest_path"]).parent
        all_cases = listed_tests(subprocess.check_output(command, cwd=cwd, text=True, encoding="utf-8", timeout=60))
        ignored = listed_tests(subprocess.check_output(
            [*command, "--ignored"], cwd=cwd, text=True, encoding="utf-8", timeout=60))
        require(ignored <= all_cases, f"inconsistent ignored listing: {key}")
        dep_info = Path(artifact["executable"]).with_suffix(".d").read_text(encoding="utf-8")
        sources = {(root / path).resolve(strict=True) for path in dep_paths(dep_info)}
        targets[key] = {"all": all_cases, "ignored": ignored, "sources": sources}
    require(targets, "Cargo produced no test artifacts")
    return targets


def static_value(node, values):
    """Only literal selector data; never import or execute a fixture."""
    def read(child, bindings=values):
        return static_value(child, bindings)

    if isinstance(node, ast.Constant):
        return node.value
    if isinstance(node, ast.Name) and node.id in values:
        return values[node.id]
    if isinstance(node, (ast.List, ast.Tuple)):
        items = [read(x) for x in node.elts]
        return tuple(items) if isinstance(node, ast.Tuple) else items
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        try:
            return read(node.left) + read(node.right)
        except TypeError as error:
            raise InventoryError("incompatible literal selector operands") from error
    if isinstance(node, ast.ListComp) and len(node.generators) == 1:
        loop = node.generators[0]
        items = read(loop.iter)
        require(type(items) in (list, tuple, str), "selector comprehension needs literal iterable data")
        return [read(node.elt, dict(values, **{loop.target.id: value}))
                for value in items]
    raise InventoryError("selector is not supported literal data: " + ast.dump(node))


def syntax_failure(filename, node, construct):
    raise InventoryError(f"{filename}:{getattr(node, 'lineno', 1)}: outside closed fixture form: {construct}")


def reference_names(node, parent):
    """Identifier references also live in import and pattern string fields."""
    if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Load):
        return [node.id]
    if isinstance(node, ast.Attribute) and isinstance(node.ctx, ast.Load):
        return [node.attr]
    if isinstance(node, ast.alias) and isinstance(parent, ast.ImportFrom):
        return [node.name]
    if isinstance(node, ast.MatchClass):
        return node.kwd_attrs
    return []


def binding_names(node):
    """Binding fields are not all Name(Store) nodes: imports and patterns matter."""
    if isinstance(node, ast.Name) and isinstance(node.ctx, (ast.Store, ast.Del)):
        return [node.id]
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
        return [node.name]
    if isinstance(node, ast.arg):
        return [node.arg]
    if isinstance(node, ast.alias):
        return [node.asname or node.name.split('.')[0]]
    if isinstance(node, (ast.Global, ast.Nonlocal)):
        return node.names
    if isinstance(node, ast.ExceptHandler):
        return [node.name] if node.name else []
    if isinstance(node, (ast.MatchAs, ast.MatchStar)):
        return [node.name] if node.name else []
    if isinstance(node, ast.MatchMapping):
        return [node.rest] if node.rest else []
    if type(node).__name__ in {'TypeVar', 'ParamSpec', 'TypeVarTuple'}:
        return [node.name]
    return []


class PythonFixture:
    """One literal grammar and syntactic policy for every Python owner (DEC-1413.1)."""

    def __init__(self, source, filename, selection=None, scopes=()):
        self.source, self.filename, self.selection = source, filename, selection
        try:
            self.tree = ast.parse(source)
        except SyntaxError as error:
            raise InventoryError(f"{filename}:{error.lineno}: invalid Python fixture: {error.msg}") from error
        self.parents = {child: node for node in ast.walk(self.tree)
                        for child in ast.iter_child_nodes(node)}
        self.protected, self.permitted = {'__name__'}, set()
        self.functions, self.values, self.declarations = {}, {}, {}
        self.data_nodes = []
        self.scopes = set(scopes) - {'<module>'}
        for node in self.tree.body:
            if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
                self.declarations.setdefault(node.targets[0].id, []).append(node)
        for name in self.scopes | ({'main'} if selection else set()):
            functions = [node for node in self.tree.body
                         if isinstance(node, ast.FunctionDef) and node.name == name]
            self.functions[name] = functions
            self.protected.add(name)
            self.permitted.update(functions)
        # Check source-wide spelling restrictions before inspecting selector data,
        # so a reflective expression is diagnosed at its actual source construct.
        for name in ('reflection_names', 'reflection_imports', 'wildcards', 'sys_access'):
            if name in RULES:
                RULES[name](self)
        if selection:
            if selection['kind'] == 'data':
                expressions = [ast.parse(selection['expression'], mode='eval').body]
            elif selection['kind'] == 'calls':
                scope = python_scope_tree(source, selection['scope'], tree=self.tree)
                expressions = [node.args[selection['argument']] for node in ast.walk(scope)
                               if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
                               and node.func.id == selection['callee']
                               and len(node.args) > selection['argument']
                               and (not selection.get('ignored_keyword') or any(
                                   key.arg == 'ignored' and isinstance(key.value, ast.Constant)
                                   and key.value.value is True for key in node.keywords))]
            else:
                syntax_failure(filename, self.tree, 'unknown selector kind')
            self.data_nodes.extend(expressions)
            pending = set().union(*(self.free_names(node) for node in expressions))
            while pending:
                name = pending.pop()
                if name in self.protected:
                    continue
                self.protected.add(name)
                assignments = self.declarations.get(name, [])
                if not assignments:
                    syntax_failure(filename, self.tree, 'missing literal declaration ' + name)
                self.permitted.update(node.targets[0] for node in assignments)
                self.data_nodes.extend(node.value for node in assignments)
                pending.update(self.free_names(assignments[0].value) - self.protected)
            if 'declarations' in RULES:
                RULES['declarations'](self)
            if 'comprehensions' in RULES:
                RULES['comprehensions'](self)
            visiting = set()
            def read(name):
                if name in self.values:
                    return self.values[name]
                if name in visiting:
                    syntax_failure(filename, self.declarations[name][0], 'cyclic selector data ' + name)
                visiting.add(name)
                node = self.declarations[name][0].value
                bindings = {dependency: read(dependency) for dependency in self.free_names(node)}
                try:
                    value = static_value(node, bindings)
                except InventoryError as error:
                    syntax_failure(filename, node, str(error))
                self.values[name] = value
                visiting.remove(name)
                return value
            for name in self.protected - {'__name__'} - set(self.functions):
                read(name)
        for name, check in RULES.items():
            if name not in {'reflection_names', 'reflection_imports', 'wildcards', 'sys_access',
                            'declarations', 'comprehensions'}:
                check(self)

    @staticmethod
    def free_names(node, bound=frozenset()):
        if isinstance(node, ast.Name):
            return {node.id} - bound if isinstance(node.ctx, ast.Load) else set()
        if isinstance(node, ast.ListComp):
            names, local = set(), set(bound)
            for loop in node.generators:
                names.update(PythonFixture.free_names(loop.iter, local))
                local.update(n.id for n in ast.walk(loop.target) if isinstance(n, ast.Name))
                for condition in loop.ifs:
                    names.update(PythonFixture.free_names(condition, local))
            return names | PythonFixture.free_names(node.elt, local)
        return set().union(*(PythonFixture.free_names(child, bound)
                             for child in ast.iter_child_nodes(node)))

    def declarations_rule(self):
        for name in self.protected - {'__name__'} - set(self.functions):
            assignments = self.declarations.get(name, [])
            if len(assignments) != 1:
                syntax_failure(self.filename, assignments[-1] if assignments else self.tree,
                               'unique module literal declaration ' + name)

    def comprehensions_rule(self):
        for expression in self.data_nodes:
            for node in ast.walk(expression):
                if isinstance(node, ast.ListComp):
                    if (len(node.generators) != 1 or not isinstance(node.generators[0].target, ast.Name)
                            or node.generators[0].ifs or node.generators[0].is_async):
                        syntax_failure(self.filename, node, 'single synchronous unfiltered selector comprehension')

    def bindings_rule(self):
        for node in ast.walk(self.tree):
            for name in binding_names(node):
                if name in self.protected and node not in self.permitted:
                    syntax_failure(self.filename, node, 'protected binding ' + name)
            if isinstance(node, ast.Attribute) and node.attr in self.protected and isinstance(node.ctx, (ast.Store, ast.Del)):
                syntax_failure(self.filename, node, 'protected attribute binding ' + node.attr)

    def list_reads_rule(self):
        names = {name for name, value in self.values.items() if type(value) is list}
        for node in ast.walk(self.tree):
            parent = self.parents.get(node)
            for name in reference_names(node, parent):
                if name in names:
                    if not isinstance(parent, (ast.For, ast.comprehension)) or parent.iter is not node:
                        syntax_failure(self.filename, node, 'list selector outside iteration ' + name)

    def reflection_names_rule(self):
        spellings = {'globals', 'locals', 'vars', 'eval', 'exec', 'compile',
                     '__import__', '__builtins__', '__dict__', '__globals__', 'f_globals', 'f_locals'}
        for node in ast.walk(self.tree):
            names = ([node.id] if isinstance(node, ast.Name)
                     else [node.attr] if isinstance(node, ast.Attribute)
                     else reference_names(node, self.parents.get(node)))
            for name in names:
                if name in spellings:
                    syntax_failure(self.filename, node, 'reflective spelling ' + name)

    def reflection_imports_rule(self):
        for node in ast.walk(self.tree):
            modules = ([item.name for item in node.names] if isinstance(node, ast.Import)
                       else [node.module or ''] if isinstance(node, ast.ImportFrom) else [])
            for module in modules:
                if module.split('.')[0] in {'builtins', 'importlib', 'inspect', 'gc'}:
                    syntax_failure(self.filename, node, 'reflection import ' + module)

    def wildcards_rule(self):
        for node in ast.walk(self.tree):
            if isinstance(node, ast.ImportFrom) and any(item.name == '*' for item in node.names):
                syntax_failure(self.filename, node, 'wildcard import')

    def sys_access_rule(self):
        aliases = {'sys'}
        for node in ast.walk(self.tree):
            if isinstance(node, ast.Import):
                aliases.update(item.asname or 'sys' for item in node.names if item.name == 'sys')
            if isinstance(node, ast.ImportFrom) and node.module == 'sys' and any(item.name in {'modules', '_getframe'} for item in node.names):
                syntax_failure(self.filename, node, 'reflective sys import')
        for node in ast.walk(self.tree):
            if (isinstance(node, ast.Attribute) and isinstance(node.value, ast.Name)
                    and node.value.id in aliases and node.attr in {'modules', '_getframe'}):
                syntax_failure(self.filename, node, 'reflective sys attribute ' + node.attr)

    def functions_rule(self):
        for name, functions in self.functions.items():
            if len(functions) != 1 or functions[0].decorator_list:
                syntax_failure(self.filename, functions[0] if functions else self.tree,
                               'unique undecorated module function ' + name)

    def entry_rule(self):
        if not self.selection:
            return
        entries = [ast.parse('if __name__ == "__main__":\n    ' + call).body[0]
                   for call in ('main()', 'raise SystemExit(main())')]
        if not self.tree.body or not any(ast.dump(self.tree.body[-1]) == ast.dump(entry) for entry in entries):
            syntax_failure(self.filename, self.tree.body[-1] if self.tree.body else self.tree,
                           'final canonical main entry')


# Operational restrictions have independent counterfactual tests. Literal AST
# node decoding is a grammar, never an execution or alias-analysis fallback.
RULES = {
    'declarations': PythonFixture.declarations_rule,
    'comprehensions': PythonFixture.comprehensions_rule,
    'bindings': PythonFixture.bindings_rule,
    'list_reads': PythonFixture.list_reads_rule,
    'reflection_names': PythonFixture.reflection_names_rule,
    'reflection_imports': PythonFixture.reflection_imports_rule,
    'wildcards': PythonFixture.wildcards_rule,
    'sys_access': PythonFixture.sys_access_rule,
    'functions': PythonFixture.functions_rule,
    'entry': PythonFixture.entry_rule,
}


def python_fixtures(root, owners):
    specifications, scopes = {}, {}
    for owner in owners.values():
        selection = owner.get('selection')
        if selection:
            filename = selection['file']
            require(filename not in specifications or specifications[filename] == selection,
                    'conflicting Python owner selectors: ' + filename)
            specifications[filename] = selection
        for witness in owner.get('witnesses', []):
            if witness['language'] == 'python':
                scopes.setdefault(witness['file'], set()).add(witness['scope'])
    return {filename: PythonFixture((root / filename).read_text(encoding='utf-8'), filename,
                                   specifications.get(filename), scopes.get(filename, ()))
            for filename in set(specifications) | set(scopes)}


class PruneInactive(ast.NodeTransformer):
    def visit_If(self, node):
        node = self.generic_visit(node)
        if isinstance(node.test, ast.Constant):
            return node.body if node.test.value else node.orelse
        return node

    def visit_While(self, node):
        node = self.generic_visit(node)
        if isinstance(node.test, ast.Constant) and not node.test.value:
            return node.orelse
        return node

    def visit_FunctionDef(self, node):
        return None

    visit_AsyncFunctionDef = visit_FunctionDef
    visit_ClassDef = visit_FunctionDef

    def visit_Lambda(self, node):
        return ast.Constant(value="unexecuted lambda")


class PruneModuleEntry(PruneInactive):
    def visit_If(self, node):
        entries = [ast.parse('if __name__ == "__main__":\n    ' + call).body[0]
                   for call in ('main()', 'raise SystemExit(main())')]
        if any(ast.dump(node) == ast.dump(entry) for entry in entries):
            return node.body
        if isinstance(node.test, ast.Constant):
            return super().visit_If(node)
        return None

    def visit_unsupported_control(self, node):
        # A module witness does not interpret an opaque control-flow body.
        return None

    visit_While = visit_unsupported_control
    visit_For = visit_unsupported_control
    visit_AsyncFor = visit_unsupported_control
    visit_Try = visit_unsupported_control
    visit_TryStar = visit_unsupported_control
    visit_With = visit_unsupported_control
    visit_AsyncWith = visit_unsupported_control
    visit_Match = visit_unsupported_control


def python_scope_tree(source, scope, *, tree=None):
    validated = tree is not None
    tree = copy.deepcopy(tree) if validated else ast.parse(source)
    if scope == "<module>":
        if not validated:
            PythonFixture(source, "<fixture>")
        body = tree.body
    else:
        functions = [n for n in tree.body if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)) and n.name == scope]
        require(len(functions) == 1, f"missing or ambiguous Python function: {scope}")
        body = [n for n in functions[0].body if not isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))]
    pruner = PruneModuleEntry() if scope == "<module>" else PruneInactive()
    active = pruner.visit(ast.Module(body=body, type_ignores=[]))
    return ast.fix_missing_locations(active)


def python_scope(source, scope, *, tree=None):
    return ast.unparse(python_scope_tree(source, scope, tree=tree))


def python_tokens(source):
    result = []
    try:
        for token in tokenize.generate_tokens(io.StringIO(source).readline):
            if token.type == tokenize.NEWLINE:
                result.append(("statement",))
            if token.type in (tokenize.NAME, tokenize.OP, tokenize.NUMBER):
                result.append(token.string)
            elif token.type == tokenize.STRING:
                try:
                    result.append(("string", ast.literal_eval(token.string)))
                except (ValueError, SyntaxError):
                    result.append(("formatted", ast.dump(ast.parse(token.string, mode="eval"))))
    except tokenize.TokenError as error:
        # A witness may deliberately be a call prefix, e.g. run(command,.
        require("EOF in multi-line statement" in str(error), str(error))
    while result and result[-1] == ("statement",):
        result.pop()
    return result


def rust_tokens(source):
    """Lex witnesses, not Rust items/cfgs; Cargo supplies compiled case names."""
    result, i = [], 0
    while i < len(source):
        if source[i].isspace():
            i += 1
            continue
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = len(source) if end < 0 else end + 1
            continue
        if source.startswith("/*", i):
            depth, i = 1, i + 2
            while depth and i < len(source):
                if source.startswith("/*", i): depth, i = depth + 1, i + 2
                elif source.startswith("*/", i): depth, i = depth - 1, i + 2
                else: i += 1
            require(depth == 0, "unterminated Rust comment")
            continue
        raw = re.match(r'(?:b|c)?r(#+)?"', source[i:])
        if raw:
            start = i + raw.end()
            end = source.find('"' + (raw[1] or ""), start)
            require(end >= 0, "unterminated Rust raw string")
            result.append(("string", source[start:end]))
            i = end + 1 + len(raw[1] or "")
            continue
        if source[i] == '"':
            end = i + 1
            while end < len(source):
                if source[end] == "\\": end += 2
                elif source[end] == '"': break
                else: end += 1
            require(end < len(source), "unterminated Rust string")
            spelling = source[i:end + 1]
            try: value = json.loads(spelling)
            except ValueError: value = spelling
            result.append(("string", value))
            i = end + 1
            continue
        char = re.match(r"'(?:\\.|[^'\\\n])'", source[i:])
        if char:
            result.append(("char", char[0])); i += len(char[0]); continue
        word = re.match(r"[A-Za-z_][A-Za-z_0-9]*|[0-9]+", source[i:])
        if word:
            result.append(word[0]); i += len(word[0]); continue
        result.append(source[i]); i += 1
    return result


def rust_scope(source, function):
    tokens = rust_tokens(source)
    starts = [i for i in range(len(tokens) - 2) if tokens[i:i + 3] == ["fn", function, "("]]
    require(len(starts) == 1, f"missing or ambiguous Rust function: {function}")
    i = starts[0]
    while i < len(tokens) and tokens[i] != "{": i += 1
    require(i < len(tokens), f"function has no body: {function}")
    start, depth = i + 1, 1
    while depth and i + 1 < len(tokens):
        i += 1
        if tokens[i] == "{": depth += 1
        elif tokens[i] == "}": depth -= 1
    require(depth == 0, f"unterminated function: {function}")
    return tokens[start:i]


def contains(tokens, witness):
    return bool(witness) and any(tokens[i:i + len(witness)] == witness for i in range(len(tokens) - len(witness) + 1))


def check_witness(root, witness, fixtures=None):
    fixture = (fixtures or {}).get(witness["file"])
    source = fixture.source if fixture else (root / witness["file"]).read_text(encoding="utf-8")
    language = witness["language"]
    if language == "python":
        tokens = python_tokens(python_scope(source, witness["scope"], tree=fixture.tree if fixture else None))
        lexer = python_tokens
    elif language == "rust":
        tokens = rust_scope(source, witness["scope"]) if witness["scope"] != "<module>" else rust_tokens(source)
        lexer = rust_tokens
    else:
        raise InventoryError(f"unsupported witness language: {language}")
    for fragment in witness.get("requires", []):
        require(contains(tokens, lexer(fragment)), f"removed invocation in {witness['file']}::{witness['scope']}: {fragment}")
    for fragment in witness.get("literal_contains", []):
        require(any(isinstance(t, tuple) and t[0] == "string" and fragment in t[1] for t in tokens),
                f"missing command literal in {witness['file']}::{witness['scope']}: {fragment}")


# Plain scalars YAML 1.1 or 1.2 may resolve to a boolean or null rather than a
# string; GitHub's parser is not pinned to either, so both sets are refused.
YAML_NON_STRINGS = {"true", "false", "yes", "no", "on", "off", "y", "n", "null"}


def workflow_runs(source, job, platform=None):
    """Read the repository's run scalar/block subset; ambiguous owners fail."""
    match = re.search(r"^  " + re.escape(job) + r":\s*$", source, re.M)
    require(match is not None, f"missing CI job: {job}")
    tail = source[match.end():]
    following = re.search(r"^  [\w-]+:\s*$", tail, re.M)
    body = tail[:following.start()] if following else tail
    if platform is not None:
        runner = re.search(r"^    runs-on: ([\w-]+)\s*$", body, re.M)
        prefix = {"linux": "ubuntu-", "win32": "windows-", "darwin": "macos-"}[platform]
        require(runner is not None and runner[1].startswith(prefix), f"owner platform differs from CI runner: {job}")
    job_conditions = re.findall(r"^    if: (.+)$", body, re.M)
    require(not job_conditions, f"conditional owner job needs an explicit audit: {job}")
    steps = re.split(r"^      - ", body, flags=re.M)[1:]
    require(not re.search(r"^        (include|exclude):", body, re.M),
            f"matrix include/exclude needs an explicit execution audit: {job}")
    # Without include/exclude every combination of the axes runs, so a
    # conjunction of equalities executes exactly when each value is on its axis.
    axes = {}
    # Any strategy must be the block form read below. A flow or expression
    # matrix on one line would otherwise skip this reading altogether, and
    # its unconditional steps would count although it may yield no variant.
    strategy = re.search(r"^    strategy:(.*)$", body, re.M)
    if strategy:
        block = re.search(r"^      matrix:[ \t]*\n((?:        .*\n?)*)", body, re.M)
        require(not strategy[1].strip() and block is not None,
                f"unsupported matrix definition needs an explicit audit: {job}")
        for line in block[1].splitlines():
            axis = re.fullmatch(r"        ([\w-]+): \[([^\]]*)\]\s*", line)
            require(axis is not None, f"unsupported matrix line needs an explicit audit: {job}: {line.strip()}")
            require(axis[2].strip(), f"owner matrix has no known executed variant: {job}")
            # Only tokens YAML can read as nothing but a string. A quoted value
            # may hold a comma, and splitting it would invent variants; a
            # boolean, null or number keeps its type in the matrix, and
            # `matrix.x == 'true'` then compares it as a number and never holds.
            values = [x.strip() for x in axis[2].split(",")]
            require(all(re.fullmatch(r"[A-Za-z][\w-]*", v) and v.lower() not in YAML_NON_STRINGS for v in values),
                    f"unsupported matrix value needs an explicit audit: {job}: {line.strip()}")
            axes[axis[1]] = set(values)
        require(axes, f"owner matrix has no known executed variant: {job}")
    for step in steps:
        condition = re.search(r"^        if: (.+)$", step, re.M)
        active = True
        if condition:
            value = condition[1].strip()
            clauses = [re.fullmatch(r"matrix\.([\w-]+) == '([^']+)'", c.strip()) for c in value.split("&&")]
            if value in ("false", "${{ false }}"): active = False
            elif value in ("true", "${{ true }}"): pass
            elif all(clauses):
                # One combination holds one value per axis, so two different
                # values asked of the same axis select nothing.
                wanted = {}
                for clause in clauses: wanted.setdefault(clause[1], set()).add(clause[2])
                active = all(len(values) == 1 and values <= axes.get(axis, set()) for axis, values in wanted.items())
            else: active = False
        if not active: continue
        run = re.search(r"(?:^|\n)(?:        )?run: (.*)", step)
        if not run: continue
        scalar = run[1]
        if scalar in ("|", "|-", "|+"):
            lines = []
            for line in step[run.end():].splitlines():
                if not line.strip(): continue
                if not line.startswith("          "): break
                lines.append(line[10:])
            yield "\n".join(lines)
        elif scalar.startswith(('"', "'")):
            raise InventoryError(f"quoted run scalar needs an explicit audit: {job}")
        else:
            require(not scalar.startswith((">", "&", "*")), f"unsupported run scalar: {scalar}")
            yield scalar


def shell_commands(script):
    # Owned invocations are straight-line commands or the existing out=$(...)
    # control. A command in a comment, echo, function or branch is not an owner.
    script = script.replace("\\\n", " ")
    if not re.search(r"^\s*(?:out=\$\()?((?:sudo\s+)?(?:cargo|python3)\s+)", script, re.M):
        return
    require(not re.search(r"^\s*\w+\s*\(\)\s*\{", script, re.M), "shell functions need an explicit execution witness")
    guarded = 0
    for line in script.splitlines():
        stripped = line.strip()
        if re.match(r"(if|for|while|case)\b", stripped): guarded += 1
        if re.match(r"(fi|done|esac)\b", stripped): guarded = max(0, guarded - 1); continue
        if guarded: continue
        match = re.match(r"(?:out=\$\()?((?:sudo\s+)?(?:cargo|python3)\s+.*)", stripped)
        if not match: continue
        command = shlex.split(match[1], comments=True)
        if command and command[0] == "sudo": command = command[1:]
        # Redirections/pipes are not libtest filters.
        command = command[:next((i for i, x in enumerate(command) if x in ("|", ">", ">>") or x.startswith("2>")), len(command))]
        yield [x.rstrip(")") for x in command]


def cargo_selection(command):
    if command[:2] != ["cargo", "test"] or "--" not in command: return None
    split = command.index("--"); build, args = command[2:split], command[split + 1:]
    if "--no-run" in build: return None
    # Cargo forwards its optional TESTNAME to libtest alongside the arguments
    # after `--`. Consume known option values first so they cannot become names.
    options, filters, targets = {}, [], []
    value_options = {"-p": "package", "--package": "package", "--profile": "profile",
                     "--test": "test", "--bin": "bin"}
    flags = {"--offline", "--locked", "--frozen"}
    seen_flags = set()
    i = 0
    while i < len(build):
        arg = build[i]
        name, equals, value = arg.partition("=") if arg.startswith("--") else (arg, "", "")
        if name in value_options:
            key = value_options[name]
            require(key not in options, f"repeated Cargo selector option: {name}")
            if not equals:
                i += 1
                require(i < len(build), f"missing Cargo option value: {name}")
                value = build[i]
            require(value and not value.startswith("-"), f"missing Cargo option value: {name}")
            options[key] = value
            if key in ("test", "bin"): targets.append((key, value))
        elif arg == "--lib": targets.append(("lib", None))
        elif arg in flags:
            require(arg not in seen_flags, f"repeated Cargo selector option: {arg}")
            seen_flags.add(arg)
        elif arg.startswith("-"): raise InventoryError(f"unrecognized Cargo selector option: {arg}")
        else:
            require(not filters, "multiple Cargo TESTNAME arguments")
            filters.append(arg)
        i += 1
    require(len(targets) <= 1, "multiple Cargo target selectors")
    package = options.get("package")
    if not package or not targets: return None
    kind, target = targets[0]
    if kind == "lib": target = package.replace("-", "_")
    exact, skips, ignored = False, [], False
    i = 0
    while i < len(args):
        arg = args[i]
        if arg == "--ignored": ignored = True
        elif arg == "--exact": exact = True
        elif arg == "--skip":
            i += 1; require(i < len(args), "missing skip value"); skips.append(args[i])
        elif arg in ("--test-threads", "--format", "--color"):
            i += 1; require(i < len(args), "missing libtest option value")
        elif arg.startswith(("--test-threads=", "--format=", "--color=")) or arg == "--nocapture": pass
        elif arg.startswith("-"): raise InventoryError(f"unrecognized libtest selector option: {arg}")
        else: filters.append(arg)
        i += 1
    return (package, kind, target), ignored, exact, filters, skips


def selects(selection, key, name):
    if not selection: return False
    target, ignored, exact, filters, skips = selection
    return (target == key and ignored and not any(name == s if exact else s in name for s in skips)
            and (not filters or any(name == f if exact else f in name for f in filters)))


def owner_selectors(root, owner, fixtures=None):
    selection = owner.get("selection")
    if not selection: return None
    fixture = (fixtures or {}).get(selection["file"])
    if fixture is None:
        source = (root / selection["file"]).read_text(encoding="utf-8")
        fixture = PythonFixture(source, selection["file"], selection, [w["scope"] for w in owner.get("witnesses", []) if w["file"] == selection["file"]])
    source, tree, values = fixture.source, fixture.tree, fixture.values
    kind = selection["kind"]
    if kind == "data":
        names = static_value(ast.parse(selection["expression"], mode="eval").body, values)
        if isinstance(names, str): names = [names]
        require(isinstance(names, (list, tuple)) and all(isinstance(n, str) for n in names), "selector must contain names")
        return {selection.get("prefix", "") + n for n in names}
    if kind == "calls":
        scoped = python_scope_tree(source, selection["scope"], tree=tree)
        names = set()
        for call in ast.walk(scoped):
            if not isinstance(call, ast.Call) or not isinstance(call.func, ast.Name) or call.func.id != selection["callee"]: continue
            if selection.get("ignored_keyword") and not any(k.arg == "ignored" and isinstance(k.value, ast.Constant) and k.value.value is True for k in call.keywords): continue
            if len(call.args) <= selection["argument"]:
                syntax_failure(fixture.filename, call, "missing selected case argument")
            argument = call.args[selection["argument"]]
            try:
                name = static_value(argument, values)
            except InventoryError as error:
                syntax_failure(fixture.filename, argument, str(error))
            if not isinstance(name, str):
                syntax_failure(fixture.filename, argument, "selected case argument must be a string")
            names.add(name)
        require(names, "no literal case calls")
        return names
    raise InventoryError(f"unknown selector kind: {kind}")


def validate(root, inventory, targets, platform):
    require(inventory.get("version") == 1, "unsupported inventory version")
    require(platform in ("linux", "win32", "darwin"), f"unsupported discovery platform: {platform}")
    owners = inventory["owners"]
    entries, used = {}, set()
    for group in inventory["groups"]:
        key = tuple(group["target"])
        require(group["owner"] in owners, f"unknown owner: {group['owner']}")
        require(group["cases"], "empty case group")
        for name in group["cases"]:
            case = (*key, name)
            require(case not in entries, f"duplicate case mapping: {case}")
            entries[case] = group; used.add(group["owner"])
            if platform == group["validate_on"]:
                require(key in targets and name in targets[key]["all"], f"stale case/target mapping: {case}")
            if key in targets and name in targets[key]["all"]:
                require((name in targets[key]["ignored"]) == (platform in group["ignored_on"]), f"changed ignore condition: {case}")
    for key, target in targets.items():
        for name in target["ignored"]:
            require((*key, name) in entries, f"ignored case has no execution owner: {(*key, name)}")
    pending = list(used)
    while pending:
        parent = owners[pending.pop()].get("parent")
        if parent:
            require(parent in owners, f"unknown parent owner: {parent}")
            if parent not in used:
                used.add(parent)
                pending.append(parent)
    require(used == set(owners), "unused execution owner")
    workflow = (root / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    fixtures = python_fixtures(root, owners)
    cache, visiting = {}, set()
    def check_owner(owner_id):
        if owner_id in cache: return cache[owner_id]
        require(owner_id not in visiting, f"execution-owner cycle: {owner_id}"); visiting.add(owner_id)
        owner = owners[owner_id]
        require(owner.get("fixture") and owner.get("platform") in ("linux", "win32", "darwin"), f"missing fixture/platform: {owner_id}")
        if "parent" in owner:
            check_owner(owner["parent"])
            require(owner.get("witnesses"), f"nested owner has no invocation witnesses: {owner_id}")
            if not owner.get("parent_tests"):
                parent = owners[owner["parent"]]
                parent_file = parent.get("script") or parent.get("selection", {}).get("file")
                require(parent_file and any(w["file"] == parent_file for w in owner["witnesses"]),
                        f"nested fixture is not witnessed in its parent: {owner_id}")
        if "job" in owner:
            commands = [c for run in workflow_runs(workflow, owner["job"], owner["platform"]) for c in shell_commands(run)]
            if "script" in owner:
                require(any(w["file"] == owner["script"] for w in owner.get("witnesses", [])),
                        f"script owner has no invocation witness in its runner: {owner_id}")
                if owner.get("selection"):
                    require(owner["selection"]["file"] == owner["script"], "selector data is not in the scheduled runner")
                require(any(c[:2] == ["python3", owner["script"]] and all(x in c for x in owner.get("arguments", [])) for c in commands), f"removed CI runner: {owner_id}")
            elif owner.get("ordinary"):
                require(any(c == ["cargo", "test", "--workspace", "--all-targets"] for c in commands), f"removed ordinary test runner: {owner_id}")
            else:
                cache[owner_id] = [cargo_selection(c) for c in commands]
        else:
            require("parent" in owner, f"owner is not rooted in CI: {owner_id}")
        for witness in owner.get("witnesses", []): check_witness(root, witness, fixtures)
        cache.setdefault(owner_id, None)
        visiting.remove(owner_id)
        return cache[owner_id]
    for owner_id in owners: check_owner(owner_id)
    for group in inventory["groups"]:
        key = tuple(group["target"]); owner = owners[group["owner"]]
        require(group["validate_on"] == owner["platform"], f"owner platform cannot discover its cases: {group['owner']}")
        require(not owner.get("ordinary") or owner["platform"] not in group["ignored_on"],
                f"ordinary runner skips ignored cases: {group['owner']} ({key})")
        if "case" in owner:
            require(group["cases"] == [owner["case"]], f"nested helper mapping differs from its witnessed case: {group['owner']}")
            require(any(owner["case"] in json.dumps(w) for w in owner.get("witnesses", [])), "nested helper witness must name its case")
        if "prefix" in owner:
            require(all(n.startswith(owner["prefix"]) for n in group["cases"]), "case is outside the runner prefix")
            require(any(owner["prefix"] in json.dumps(w) for w in owner.get("witnesses", [])), "prefix is not witnessed")
        if platform == owner["platform"]:
            for witness in owner.get("witnesses", []):
                if witness["language"] == "rust":
                    require((root / witness["file"]).resolve(strict=True) in targets[key].get("sources", set()),
                            f"Rust invocation witness is not compiled into its target: {witness['file']}")
        selected = owner_selectors(root, owner, fixtures)
        if selected is not None:
            require(set(group["cases"]) <= selected, f"runner no longer selects cases: {group['owner']}")
            if platform == group["validate_on"]:
                require(selected <= targets[key]["all"], f"stale runner selector: {group['owner']}: {sorted(selected - targets[key]['all'])}")
        for name in group["cases"]:
            if cache[group["owner"]] is not None:
                require(any(selects(c, key, name) for c in cache[group["owner"]]), f"CI no longer executes {name}")
        for parent in owner.get("parent_tests", []):
            parent_key = tuple(parent["target"]); parent_case = (*parent_key, parent["case"])
            require(any(w["language"] == "rust" and w["scope"] == parent["case"].split("::")[-1]
                        for w in owner.get("witnesses", [])), f"missing invocation witness in parent test: {parent_case}")
            if owner["platform"] == platform:
                require(parent_key in targets and parent["case"] in targets[parent_key]["all"], f"missing parent test: {parent_case}")
                if parent["case"] in targets[parent_key]["ignored"]:
                    require(parent_case in entries and entries[parent_case]["owner"] == owner["parent"], f"unowned nested parent: {parent_case}")
                else:
                    require(owners[owner["parent"]].get("ordinary"), f"parent is not scheduled as an ordinary test: {parent_case}")
    return len(entries)


def main():
    root = Path(__file__).resolve().parents[1]
    try:
        inventory = json.loads((root / "scripts/ignored-test-owners.json").read_text(encoding="utf-8"))
        count = validate(root, inventory, discover(root), sys.platform)
    except (InventoryError, OSError, subprocess.SubprocessError, json.JSONDecodeError) as error:
        raise SystemExit(f"ignored-test inventory failed: {error}") from error
    print(f"Ignored-test execution ownership verified ({count} registered cases, {sys.platform}).")


if __name__ == "__main__":
    main()
