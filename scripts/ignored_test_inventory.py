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
        targets[key] = {"all": all_cases, "ignored": ignored, "sources": sources,
                        "root": Path(target["src_path"])}
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

    def visit_AnnAssign(self, node):
        # Annotation evaluation depends on scope, future flags and Python
        # version. Only an assigned value supplies direct scheduling evidence;
        # admission rules still inspect annotation text in the original AST.
        if node.value is None:
            return None
        assignment = ast.Assign(targets=[node.target], value=node.value)
        return self.generic_visit(ast.copy_location(assignment, node))

    def visit_TypeAlias(self, node):
        # Alias values and type parameters are lazy; their text cannot witness
        # a direct invocation. Source-form rules still inspect the original AST.
        return None


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
    # Match at an offset: copying the remaining tail for each token made
    # large source fixtures quadratic without changing any token rule.
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
        raw = re.compile(r'(?:b|c)?r(#+)?"').match(source, i)
        if raw:
            start = raw.end()
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
        char = re.compile(r"'(?:\\.|[^'\\\n])'").match(source, i)
        if char:
            result.append(("char", char[0])); i += len(char[0]); continue
        word = re.compile(r"[A-Za-z_][A-Za-z_0-9]*|[0-9]+").match(source, i)
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

# The one job-level condition an owner may carry. A job with it is skipped on a
# pull request only; the merge group, `master` and dispatched runs execute it,
# so its cases still run on the exact tree before every merge (DEC-1835.1).
# Any other condition, or a spelling of this one, still needs an audit.
PULL_REQUEST_OMISSION = "${{ github.event_name != 'pull_request' }}"

# `continue-on-error` runs the cases but keeps their failure out of the job's
# result, which is what `ci-gate` reads, so an owner under it could never block
# a merge (#1380). Only a literal false leaves the result alone. The key is
# found whatever follows its colon, so a value on the next line, which YAML
# reads as `true` as readily, is read as empty and refused.
KEEPS_FAILURES = ("false", "${{ false }}")

# Every key the readings below look for, each by one spelling: unquoted, with
# its colon right after it. YAML takes the same key quoted or with whitespace
# before the colon, and those readings would not see it. Restrict this guard
# to the mappings they read; an action input or environment variable with the
# same name cannot change scheduling (DEC-2183.1).
AUDITED_KEYS = ("if", "run", "continue-on-error", "strategy", "matrix", "include", "exclude", "runs-on", "steps")
YAML_QUOTED = r""""(?:\\[\s\S]|[^"\\])*"|'(?:''|[^'])*'"""
YAML_QUOTED_SCALAR = re.compile(YAML_QUOTED)
YAML_KEY = re.compile(r"(" + YAML_QUOTED + r"|[^\s{}\[\],:'\"#]+)([ \t]*):")
# Python's Unicode whitespace includes plain YAML text such as NBSP. Treating
# it as separation could comment out a closing delimiter and a later key.
YAML_SEPARATION = " \t\r\n\x85\u2028\u2029"
YAML_SPACE_OR_COMMENT = re.compile(r"(?:[ \t\r\n\x85\u2028\u2029]+|#[^\r\n\x85\u2028\u2029]*)*")


def check_workflow_key_spellings(body, job):
    """Track block/flow mapping paths without interpreting scalar values."""
    def entry(match, path):
        spelling, space = match.groups()
        quoted = spelling[0] in "\"'"
        name = spelling[1:-1] if quoted else spelling
        if spelling[0] == "'": name = name.replace("''", "'")
        audited = path in ((), ("strategy",), ("strategy", "matrix"), ("steps", None))
        # Refuse escapes rather than maintaining a second YAML string decoder.
        # Single quotes only unescape doubled quotes; i''f is not the key if.
        require(not (audited and ((quoted and "\\" in name)
                                 or (name in AUDITED_KEYS and (quoted or space)))),
                f"an alternate spelling of a key needs an explicit audit: {job}: {match[0]}")
        return (*path, name)

    def whitespace(index):
        return YAML_SPACE_OR_COMMENT.match(body, index).end()

    def flow_separation(index):
        return index == len(body) or body[index] in YAML_SEPARATION or body[index] in "{}[],"

    def flow_scalar(index):
        # A quote opens a quoted token only at the scalar's start. In a plain
        # scalar even a quote after whitespace is text (DEC-2183.1).
        if index < len(body) and body[index] in "\"'":
            scalar = YAML_QUOTED_SCALAR.match(body, index)
            require(scalar is not None, f"unsupported flow mapping needs an explicit audit: {job}")
            return scalar.end()
        # Node properties can introduce a quoted scalar after another token.
        # Refuse these rather than mistake its quoted delimiters for structure.
        require(index >= len(body) or body[index] not in "!&*",
                f"unsupported flow mapping needs an explicit audit: {job}")
        separated = False
        while index < len(body):
            char = body[index]
            if char in "{}[]," or (char == "#" and separated): break
            if char == ":" and flow_separation(index + 1): break
            separated = char in YAML_SEPARATION
            index += 1
        return index

    def flow(index, path):
        # Opaque values need only a balanced end, not our mapping grammar.
        # In particular, action inputs may use YAML's explicit/complex keys.
        if path not in ((), ("strategy",), ("strategy", "matrix"), ("steps",), ("steps", None)):
            depth = 0
            while index < len(body):
                index = whitespace(index)
                if index == len(body): break
                char = body[index]
                if char in "{[": depth += 1
                elif char in "}]":
                    depth -= 1
                    if depth == 0: return index + 1
                elif char != "," and not (char in ":?" and flow_separation(index + 1)):
                    end = flow_scalar(index)
                    require(end > index, f"unsupported flow mapping needs an explicit audit: {job}")
                    # A completed quoted key makes its colon an indicator even
                    # without separation; a colon inside plain text does not.
                    if char in "\"'" and body[end:end + 1] == ":": end += 1
                    index = end
                    continue
                index += 1
            raise InventoryError(f"unterminated flow mapping: {job}")
        mapping = body[index] == "{"
        closing = "}" if mapping else "]"
        index = whitespace(index + 1)
        while index < len(body) and body[index] != closing:
            child = (*path, None)
            if mapping:
                key = YAML_KEY.match(body, index)
                # A plain key's colon needs separation; a:#b is still plain
                # text. Never hand its hash to the between-token comment scan.
                require(key is not None and (key[1][0] in "\"'" or flow_separation(key.end())),
                        f"unsupported flow mapping needs an explicit audit: {job}")
                child, index = entry(key, path), whitespace(key.end())
            if index < len(body) and body[index] in "{[":
                index = flow(index, child)
            else:
                index = flow_scalar(index)
            index = whitespace(index)
            require(index < len(body), f"unterminated flow mapping: {job}")
            if body[index] == closing: break
            require(body[index] == ",", f"unsupported flow mapping needs an explicit audit: {job}")
            index = whitespace(index + 1)
        require(index < len(body) and body[index] == closing, f"unterminated flow mapping: {job}")
        return index + 1

    stack, block, consumed, offset = [], None, 0, 0
    for line in body.splitlines(keepends=True):
        start, offset = offset, offset + len(line)
        if start < consumed or not line.strip() or line.lstrip().startswith("#"): continue
        indent = len(line) - len(line.lstrip(" "))
        if block is not None:
            if indent > block: continue
            block = None
        while stack and indent <= stack[-1][0]: stack.pop()
        path = stack[-1][1] if stack else ()
        sequence = re.match(r"-(?:[ \t]+|(?=\n|$))", line[indent:])
        if sequence:
            path = (*path, None)
            stack.append((indent, path))
            indent += sequence.end()
        index = start + indent
        if index >= len(body): continue
        if body[index] in "{[":
            consumed = flow(index, path)
            continue
        key = YAML_KEY.match(body, index)
        if not key: continue
        child = entry(key, path)
        stack.append((indent, child))
        index = re.compile(r"[ \t]*").match(body, key.end()).end()
        if index < len(body) and body[index] in "{[":
            consumed = flow(index, child)
        else:
            scalar = YAML_QUOTED_SCALAR.match(body, index)
            consumed = scalar.end() if scalar else key.end()
            if re.match(r"[|>][-+0-9]*\s*(?:#.*)?$", body[index:offset].rstrip("\n")):
                block = indent


def workflow_runs(source, job, platform=None):
    """Read the repository's run scalar/block subset; ambiguous owners fail."""
    match = re.search(r"^  " + re.escape(job) + r":(?=\s|$)", source, re.M)
    require(match is not None, f"missing CI job: {job}")
    tail = source[match.end():]
    following = re.search(r"^  [\w-]+:(?=\s|$)", tail, re.M)
    body = tail[:following.start()] if following else tail
    check_workflow_key_spellings(body, job)
    if platform is not None:
        runner = re.search(r"^    runs-on: ([\w-]+)\s*$", body, re.M)
        prefix = {"linux": "ubuntu-", "win32": "windows-", "darwin": "macos-"}[platform]
        require(runner is not None and runner[1].startswith(prefix), f"owner platform differs from CI runner: {job}")
    job_conditions = [c.strip() for c in re.findall(r"^    if: (.+)$", body, re.M)]
    require(job_conditions in ([], [PULL_REQUEST_OMISSION]),
            f"conditional owner job needs an explicit audit: {job}")
    require(all(v.strip() in KEEPS_FAILURES for v in re.findall(r"^    continue-on-error:(.*)$", body, re.M)),
            f"continue-on-error needs an explicit execution audit: {job}")
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
            # Accept plain string tokens and quoted digit strings for shards.
            # General quoted values may contain commas; splitting those would
            # invent variants. An unquoted boolean, null or number keeps its type,
            # `matrix.x == 'true'` then compares it as a number and never holds.
            values = [x.strip() for x in axis[2].split(",")]
            require(all(re.fullmatch(r"(?:[A-Za-z][\w-]*|'[0-9]+')", v) and v.lower() not in YAML_NON_STRINGS for v in values),
                    f"unsupported matrix value needs an explicit audit: {job}: {line.strip()}")
            axes[axis[1]] = {v.strip("'") for v in values}
        require(axes, f"owner matrix has no known executed variant: {job}")
    for step in steps:
        condition = re.search(r"^        if: (.+)$", step, re.M)
        active = True
        wanted = {}
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
            script = "\n".join(lines)
        elif scalar.startswith(('"', "'")):
            raise InventoryError(f"quoted run scalar needs an explicit audit: {job}")
        else:
            require(not scalar.startswith((">", "&", "*")), f"unsupported run scalar: {scalar}")
            script = scalar
        # On the step, only where it carries a command an owner could be: a
        # step that runs no case cannot hide a case's failure.
        tolerated = re.search(r"(?:^|\n)(?:        )?continue-on-error:(.*)", step)
        require(not tolerated or tolerated[1].strip() in KEEPS_FAILURES or not any(True for _ in shell_commands(script)),
                f"continue-on-error needs an explicit execution audit: {job}")
        # Expand only exact references to the known, nonempty literal axes.
        # Shell variables and arbitrary expressions remain outside this subset.
        scripts = [script]
        for axis in set(re.findall(r"\$\{\{ matrix\.([\w-]+) \}\}", script)):
            require(axis in axes, f"unknown matrix axis: {job}: {axis}")
            scripts = [text.replace("${{ matrix." + axis + " }}", value)
                       for text in scripts for value in sorted(wanted.get(axis, axes[axis]))]
        yield from scripts


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
    # CI's wrapper partitions the complete list and refuses incomplete results.
    if command[:3] == ["python3", "scripts/live-shards.py", "ci"]:
        tail = command[3:]
        while tail and tail[0] in ("--output", "--expected-tests", "--allow-empty"):
            option = tail[0]
            if option == "--allow-empty":
                tail = tail[1:]
                continue
            require(len(tail) > 1 and tail[1] and not tail[1].startswith("-"),
                    f"missing CI shard option value: {option}")
            if option == "--expected-tests":
                require(re.fullmatch(r"[1-9][0-9]*", tail[1]), "invalid expected test count")
            tail = tail[2:]
        require(len(tail) > 1 and re.fullmatch(r"[0-9]+/[1-9][0-9]*", tail[0]),
                "invalid CI shard selector")
        index, count = map(int, tail[0].split("/"))
        require(index < count, "CI shard index must be less than its count")
        command = ["cargo", *tail[1:]]
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


def rule_assignments(owners, rules, cases, platform):
    """Match namespace boundaries, with no longest-prefix or file-order priority."""
    require(isinstance(rules, dict) and rules.get("version") == 1,
            "unsupported ownership rules version")
    require(isinstance(rules.get("rules"), list), "ownership rules must be a list")
    indexed = {}
    matched = set()
    for index, rule in enumerate(rules["rules"]):
        require(isinstance(rule, dict) and set(rule) == {"target", "prefix", "owner"},
                f"invalid ownership rule {index}: expected target, prefix and owner")
        target, prefix, owner = rule["target"], rule["prefix"], rule["owner"]
        require(isinstance(target, list) and len(target) == 3 and
                all(isinstance(value, str) and value for value in target),
                f"invalid rule target: {target!r}")
        require(isinstance(prefix, str) and (not prefix or
                prefix.endswith("::") and all(part.removeprefix("r#").isidentifier()
                    for part in prefix[:-2].split("::"))),
                f"rule prefix must end at a module boundary: {prefix!r}")
        require(isinstance(owner, str) and owner in owners, f"unknown rule owner: {owner!r}")
        require(not any(key in owners[owner] for key in ("script", "selection", "parent")),
                f"Python-driven and child owners require legacy exact entries: {owner}")
        indexed.setdefault(tuple(target), []).append((index, prefix, owner))
    assignments = {}
    for case in sorted(cases):
        matches = [(index, owner) for index, prefix, owner in indexed.get(case[:3], [])
                   if case[3].startswith(prefix)]
        require(len(matches) <= 1, f"ambiguous ownership rules for {case}: {matches}")
        if matches:
            index, owner = matches[0]
            matched.add(index)
            assignments[case] = owner
    for index, rule in enumerate(rules["rules"]):
        # Other-platform cfg cases may not exist here. Their owner's platform
        # remains the authoritative stale-mapping check, as for legacy entries.
        if owners[rule["owner"]]["platform"] == platform:
            require(index in matched, f"ownership rule matches no cases: {rule}")
    return assignments


def validate(root, inventory, targets, platform, rules=None):
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
    groups = list(inventory["groups"])
    cases = set(entries) | {(*key, name) for key, target in targets.items() for name in target["ignored"]}
    assignments = rule_assignments(owners, rules if rules is not None else {"version": 1, "rules": []},
                                   cases, platform)
    if rules is not None:
        used.update(rule["owner"] for rule in rules["rules"])
    discovered = {(*key, name) for key, target in targets.items() for name in target["ignored"]}
    for case, owner in assignments.items():
        if case in entries:
            exact = entries[case]["owner"]
            # The one disagreement that is the documented rule, not a conflict
            # (AGENTS.md, "Cross-platform discovery needs an exact legacy entry
            # or an owner that runs on the discovering platform"): a case
            # compiled on one platform only, which a broad rule gives to an
            # owner of another platform, has an exact entry whose owner is
            # authoritative for it (#2491: the Windows-only PostgreSQL cases
            # under flow_pg's blanket rule). Where the case is compiled, that
            # owner must run on the discovering platform; where it is not
            # (the registry names it, discovery here cannot see it), the
            # owner must run on another platform, exactly as a legacy entry's
            # stale-mapping check is left to its own platform. Two owners on
            # the same platform, or an exact owner that does not fit either
            # way, still disagree.
            seen = case in discovered
            owner_here = owners[exact]["platform"] == platform
            require(exact == owner
                    or (owners[owner]["platform"] != platform and owner_here and seen)
                    or (not seen and not owner_here),
                    f"rule and legacy owners disagree for {case}: {owner} != {exact}")
        else:
            # Exact entries are checked on validate_on. A namespace alone cannot
            # establish that another platform compiles this particular case.
            require(platform == owners[owner]["platform"],
                    f"rule-only case {case} discovered on {platform} cannot be verified on "
                    f"owner {owner}'s platform {owners[owner]['platform']}; "
                    "add an exact legacy entry with the right owner, or a rule for an owner "
                    f"that runs on {platform}")
            group = {"target": list(case[:3]), "owner": owner, "validate_on": owners[owner]["platform"],
                     "ignored_on": ["linux", "win32", "darwin"], "cases": [case[3]]}
            entries[case] = group
            groups.append(group)
            used.add(owner)
    # Both sources use the same per-platform existence and ignore-condition
    # checks. Cross-platform exact entries keep their validate_on obligation.
    for case, group in entries.items():
        key, name = case[:3], case[3]
        if platform == group["validate_on"]:
            require(key in targets and name in targets[key]["all"], f"stale case/target mapping: {case}")
        if key in targets and name in targets[key]["all"]:
            require((name in targets[key]["ignored"]) == (platform in group["ignored_on"]), f"changed ignore condition: {case}")
    for key, target in targets.items():
        for name in target["ignored"]:
            prefix = name.rpartition("::")[0] + "::" if "::" in name else ""
            require((*key, name) in entries,
                    f"ignored case has no execution owner: {(*key, name)}; add a rule in "
                    f"scripts/ignored-test-rules.json: target={list(key)!r}, prefix={prefix!r}, "
                    "owner=<existing owner> (mixed root cases need an owned module)")
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
    for group in groups:
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
                # One runner may run the cases of several targets (the LocalDB
                # job runs library and CLI flow cases, #2491). A selected case
                # is stale only when none of the owner's targets has it.
                reachable = set().union(*(targets.get(tuple(g["target"]), {}).get("all", set())
                                          for g in groups if g["owner"] == group["owner"]))
                require(selected <= reachable, f"stale runner selector: {group['owner']}: {sorted(selected - reachable)}")
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


# --- Predicting the Windows build's inventory from Linux (#2833) ---------------
#
# The Windows `quick` job lists the ignored tests of the Windows build and refuses
# a rule-only case whose owner does not run on win32. A Linux run only lists the
# Linux binaries, so it cannot see that. Whether the Windows build compiles a test, still has it
# as a test and lists it as ignored is answered here from source, from the attributes of the test
# function and of every enclosing module (inline or file), each read once as one list (see
# "Attributes" below): outer attributes plus leading inner attributes, `cfg_attr` expanded.
# The module files are found by rustc's own path rule (see `RustModules`). Whatever the
# reader cannot decide counts as compiled and ignored on Windows, so the prediction fails
# closed; the win32 run stays the authority.

# The Windows runner of the `quick` job: windows-latest, MSVC, x86_64.
WINDOWS_CFG = {"target_os": "windows", "target_family": "windows", "target_env": "msvc",
               "target_arch": "x86_64", "target_pointer_width": "64", "target_endian": "little",
               "target_vendor": "pc"}
WINDOWS_FLAGS = {"windows": True, "unix": False, "test": True, "true": True, "false": False}


def tri_all(values):
    if any(value is False for value in values): return False
    return None if any(value is None for value in values) else True


def tri_any(values):
    if any(value is True for value in values): return True
    return None if any(value is None for value in values) else False


def parse_cfg(tokens, i):
    """Evaluate one cfg predicate at tokens[i:]; return (True/False/None, next index).

    None is a predicate the evaluator cannot decide (a feature, an unknown name)."""
    require(i < len(tokens) and isinstance(tokens[i], str) and (tokens[i][0].isalpha() or tokens[i][0] == "_"),
            "unreadable cfg predicate")
    name, i = tokens[i], i + 1
    if i < len(tokens) and tokens[i] == "(":
        if name == "cfg":
            raise InventoryError("nested cfg is not a predicate")
        values, i = [], i + 1
        while i < len(tokens) and tokens[i] != ")":
            value, i = parse_cfg(tokens, i)
            values.append(value)
            if i < len(tokens) and tokens[i] == ",": i += 1
        require(i < len(tokens), "unterminated cfg predicate")
        i += 1
        if name == "all": return tri_all(values), i
        if name == "any": return tri_any(values), i
        if name == "not" and len(values) == 1:
            return (None if values[0] is None else not values[0]), i
        return None, i
    if i + 1 < len(tokens) and tokens[i] == "=" and isinstance(tokens[i + 1], tuple) and tokens[i + 1][0] == "string":
        value = tokens[i + 1][1]
        if value.startswith('"'):
            return None, i + 2   # an escape the lexer could not decode: not compared
        if name in WINDOWS_CFG: return WINDOWS_CFG[name] == value, i + 2
        return None, i + 2
    return WINDOWS_FLAGS.get(name), i


def tri_not(value):
    return None if value is None else not value


# Attributes (#2833). Every item the prediction reads (a function, a module, a module
# file, the crate root) has its outer attributes and its leading inner attributes
# gathered into one list, `cfg_attr` is expanded once, completely and recursively, into
# the attributes it stands for with the value of the condition they apply under on
# Windows, and the three questions (is it compiled, is it ignored, which file) are
# answered from that one list. An attribute the expansion cannot read is UNCLASSIFIED
# and answers every question "undecided", so the prediction fails closed.
UNCLASSIFIED = ["<unclassified>"]
UNDECIDED = object()


def split_arguments(tokens):
    """The top-level comma-separated parts of tokens (a cfg_attr's attribute list)."""
    parts, current, depth = [], [], 0
    for token in tokens:
        if isinstance(token, str) and token in ("(", "[", "{"): depth += 1
        elif isinstance(token, str) and token in (")", "]", "}"): depth -= 1
        if token == "," and depth == 0:
            parts.append(current); current = []
        else:
            current.append(token)
    if current: parts.append(current)
    return parts


def effective_attributes(attributes, applies=True):
    """Expand `cfg_attr` in attributes into [(attribute tokens, applies on Windows)].

    `applies` is True, False (its guard fails on Windows: the attribute is not there, but the
    test-ness question needs to know it was spelled) or None (the guard is undecidable)."""
    result = []
    for tokens in attributes:
        if tokens[:1] != ["cfg_attr"]:
            result.append((tokens, applies)); continue
        unreadable = (UNCLASSIFIED, tri_all([applies, None]))
        if tokens[1:2] != ["("] or tokens[-1:] != [")"]:
            result.append(unreadable); continue
        try:
            predicate, i = parse_cfg(tokens, 2)
        except InventoryError:
            result.append(unreadable); continue
        if i == len(tokens) - 1:
            continue   # cfg_attr(predicate): no attribute at all
        if tokens[i] != ",":
            result.append(unreadable); continue
        result += effective_attributes(split_arguments(tokens[i + 1:-1]), tri_all([applies, predicate]))
    return result


def cfg_of(effective):
    """Windows value of the `cfg` attributes among effective: True/False, or None if undecidable."""
    values = []
    for tokens, applies in effective:
        if tokens == UNCLASSIFIED:
            values.append(None)
        elif tokens[:1] == ["cfg"]:
            try:
                value, end = parse_cfg(tokens, 2) if tokens[1:2] == ["("] else (None, 0)
            except InventoryError:
                value, end = None, 0
            if end != len(tokens) - 1 or tokens[end] != ")":
                value = None
            values.append(tri_any([tri_not(applies), value]))
    return tri_all(values)


def ignored_of(effective):
    """Is the item `#[ignore]`d on Windows: True/False, or None if undecidable?

    `#[ignore]` and `#[ignore = "why"]` ignore it; an `ignore` of any other spelling is not read."""
    values = []
    for tokens, applies in effective:
        if tokens == UNCLASSIFIED:
            values.append(None)
        elif tokens[:1] == ["ignore"]:
            exact = tokens == ["ignore"] or (len(tokens) == 3 and tokens[1] == "=" and isinstance(tokens[2], tuple))
            values.append(applies if exact else None)
    return tri_any(values)


def path_of(effective):
    """The string of the `#[path = "..."]` in effect on Windows, None if there is none, UNDECIDED if unreadable."""
    found = None
    for tokens, applies in effective:
        if tokens == UNCLASSIFIED:
            return UNDECIDED
        if tokens[:1] == ["path"]:
            if applies is None or len(tokens) != 3 or tokens[1] != "=" or not isinstance(tokens[2], tuple) \
                    or tokens[2][1].startswith('"'):   # a conditional path, or an escape the lexer could not decode
                return UNDECIDED
            found = tokens[2][1]
    return found


# Attributes that do not make a function a test. Any other attribute may be a test macro.
INERT_ATTRIBUTES = {"ignore", "should_panic", "cfg", "doc", "allow", "warn", "deny", "forbid", "expect",
                    "inline", "must_use", "deprecated", "track_caller", "cold", "coverage"}


def attribute_head(tokens):
    """The tokens of an attribute's name, up to its arguments: `tokio::test(flavor = "x")` is tokio : : test."""
    for i, token in enumerate(tokens):
        if token in ("(", "=", "[", "{"): return tokens[:i]
    return tokens


def attribute_path(tokens):
    """The path segments of an attribute's name: `tokio::test(flavor = "x")` is ["tokio", "test"]."""
    return [token for token in attribute_head(tokens) if token != ":"]


def test_of(everything):
    """Is the function still a test on Windows: True/False, or None if undecidable?

    A `test` attribute (any path ending in `test`) that applies makes it one. It is certainly not
    one only if every such attribute is guarded off and nothing else could be a test macro."""
    named, other = [], False
    for tokens, applies in everything:
        if tokens == UNCLASSIFIED:
            if applies is not False: other = True
        elif attribute_path(tokens)[-1:] == ["test"]:
            named.append(applies)
        elif applies is not False and (len(attribute_head(tokens)) != 1 or tokens[0] not in INERT_ATTRIBUTES):
            other = True   # only an exact, unqualified built-in is known to be inert
    if any(applies is True for applies in named): return True
    if other or any(applies is None for applies in named) or not named: return None
    return False


def attribute_facts(outer, inner=()):
    """(compiled, ignored, path, test) of one item on Windows, from its outer and leading inner attributes together."""
    everything = effective_attributes([*outer, *inner])
    live = [pair for pair in everything if pair[1] is not False]
    return (cfg_of(live), ignored_of(live), path_of([pair for pair in effective_attributes(outer) if pair[1] is not False]),
            test_of(everything))


def leading_inner_attributes(tokens, i, end):
    """The inner attributes `#![...]` at the start of a body, tokens[i:end]."""
    inner = []
    while i + 2 < end and tokens[i] == "#" and tokens[i + 1] == "!" and tokens[i + 2] == "[":
        close = skip_balanced(tokens, i + 2)
        inner.append(tokens[i + 3:close - 1])
        i = close
    return inner


def skip_balanced(tokens, i):
    """tokens[i] opens a group; return the index after its matching close."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = [pairs[tokens[i]]]
    i += 1
    while stack and i < len(tokens):
        token = tokens[i]
        if isinstance(token, str):
            if token in pairs: stack.append(pairs[token])
            elif token == stack[-1]: stack.pop()
        i += 1
    require(not stack, "unbalanced Rust group")
    return i


ITEM_MODIFIERS = {"pub", "async", "unsafe", "const", "extern", "default", "safe"}


def rust_items(tokens, start=0, end=None):
    """List the modules and functions directly inside tokens[start:end].

    Returns (inner_attributes, items); an item is (kind, name, attributes, body)
    where body is (start, end) of an inline module or None for `mod x;`."""
    end = len(tokens) if end is None else end
    inner, items, i = [], [], start
    while i < end:
        attributes = []
        while i < end and tokens[i] == "#":
            bang = i + 1 < end and tokens[i + 1] == "!"
            j = i + 2 if bang else i + 1
            if j >= end or tokens[j] != "[":
                i = end; break
            close = skip_balanced(tokens, j)
            (inner if bang else attributes).append(tokens[j + 1:close - 1])
            i = close
        if i >= end:
            break
        j = i
        while j < end and tokens[j] in ITEM_MODIFIERS:
            j += 1
            if j < end and tokens[j] == "(" and tokens[j - 1] == "pub": j = skip_balanced(tokens, j)
            elif j < end and isinstance(tokens[j], tuple): j += 1
        kind = tokens[j] if j < end and isinstance(tokens[j], str) else None
        if kind == "mod" and j + 1 < end:
            name = tokens[j + 1]
            if j + 2 < end and tokens[j + 2] == "{":
                close = skip_balanced(tokens, j + 2)
                items.append(("mod", name, attributes, (j + 3, close - 1)))
                i = close
            else:
                items.append(("mod", name, attributes, None))
                i = j + 3
        elif kind == "fn" and j + 1 < end:
            name, k = tokens[j + 1], j + 2
            while k < end and tokens[k] not in ("{", ";"):
                k = skip_balanced(tokens, k) if tokens[k] in ("(", "[") else k + 1
            if k < end and tokens[k] == "{":
                close = skip_balanced(tokens, k)
                items.append(("fn", name, [*attributes, *leading_inner_attributes(tokens, k + 1, close - 1)], None))
                i = close
            else:
                items.append(("fn", name, attributes, None))
                i = k + 1
        else:
            # Any other item: to the `;` or the closing brace of its body, at depth 0.
            braced = kind in ("struct", "enum", "union", "impl", "trait", "macro_rules", "extern", "{") \
                or (j + 1 < end and tokens[j + 1] == "!")
            k = j
            while k < end:
                token = tokens[k]
                if token == ";": k += 1; break
                if token in ("(", "["): k = skip_balanced(tokens, k); continue
                if token == "{":
                    k = skip_balanced(tokens, k)
                    if braced: break
                    continue
                k += 1
            i = max(k, i + 1)
    return inner, items


# rustc's module-path rule (rustc_expand::module), implemented once. The state of a module
# is (directory, relative): the directory of its source file (or the one its inline modules
# have pushed) and, for a file `foo.rs` that is not `mod.rs`, the name `foo` its children
# live under.
#   - lib.rs, main.rs and the crate root start at (their directory, None);
#   - `mod x;` without #[path] is `<directory>/<relative>/x.rs`, state (that file's
#     directory, x), or `<directory>/<relative>/x/mod.rs`, state (that directory, None);
#   - `#[path = "p"] mod x;` is `<directory>/p` (the relative name is not used), and the
#     file owns its directory: state (its directory, None);
#   - `mod x { }` pushes `<relative>/x` onto the directory: state (<directory>/<relative>/x, None);
#     with its own #[path = "p"] it pushes p instead: state (<directory>/p, None);
#   - nesting composes, because every child starts from its parent's state.


def children_base(state):
    directory, relative = state
    return directory / relative if relative else directory


def inline_state(state, name, path_attribute):
    """State of the children of the inline module `name`."""
    if path_attribute is not None:
        return state[0] / path_attribute, None
    return children_base(state) / name, None


class RustModules:
    """The module tree of one crate, read lazily from source."""

    def __init__(self):
        self.tokens, self.parsed = {}, {}

    def read(self, path):
        if path not in self.tokens:
            self.tokens[path] = rust_tokens(path.read_text(encoding="utf-8"))
        return self.tokens[path]

    def file_module(self, path):
        """(inner attributes, items, tokens) of a module file."""
        tokens = self.read(path)
        if path not in self.parsed:
            self.parsed[path] = rust_items(tokens)
        inner, items = self.parsed[path]
        return inner, items, tokens

    def child_file(self, state, path_attribute, name):
        """(source file, state) of `mod name;` declared in `state`; None if it cannot be found."""
        directory = state[0]
        if path_attribute is not None:
            candidate = directory / path_attribute
            return (candidate, (candidate.parent, None)) if candidate.is_file() else None
        base = children_base(state)
        for candidate, relative in ((base / f"{name}.rs", name), (base / name / "mod.rs", None)):
            if candidate.is_file():
                return candidate, (candidate.parent, relative)
        return None

    def still_ignored_on_windows(self, root, name):
        """False if the Windows build certainly does not list test `name` of the crate at `root`
        as ignored: it does not compile it, or it is not `#[ignore]`d there."""
        inner, items, tokens = self.file_module(root)
        return self.walk((root.parent, None), tokens, inner, items, name.split("::"), True) is not False

    def walk(self, state, tokens, inner, items, parts, inherited):
        value = tri_all([inherited, attribute_facts([], inner)[0]])
        if value is False:
            return False
        kind = "fn" if len(parts) == 1 else "mod"
        found = [item for item in items if item[0] == kind and item[1] == parts[0]]
        if not found:
            return None   # generated by a macro, or outside what this reads: undecided
        results = []
        for _, _, attributes, body in found:
            compiled, ignored, path_attribute, is_test = attribute_facts(attributes)
            here = tri_all([value, compiled])
            if kind == "fn":
                results.append(tri_all([here, ignored, is_test])); continue
            if here is False:
                results.append(here); continue
            if path_attribute is UNDECIDED:
                results.append(None); continue   # a conditional or unreadable #[path]: not decided here
            if body is not None:   # inline module: it pushes a directory for its children
                if (id(tokens), body) not in self.parsed:
                    self.parsed[id(tokens), body] = rust_items(tokens, *body)
                sub_inner, sub_items = self.parsed[id(tokens), body]
                results.append(self.walk(inline_state(state, parts[0], path_attribute), tokens, sub_inner,
                                         sub_items, parts[1:], here))
                continue
            located = self.child_file(state, path_attribute, parts[0])
            if located is None:
                results.append(None); continue
            child, child_state = located
            sub_inner, sub_items, sub_tokens = self.file_module(child)
            results.append(self.walk(child_state, sub_tokens, sub_inner, sub_items, parts[1:], here))
        return tri_any(results)


def predict_win32(inventory, rules, targets, platform):
    """Refuse, on Linux, a rule-only case that the win32 run would refuse (see above).

    Raises on the first such case, in sorted order, like the win32 run; returns 0."""
    if platform != "linux":
        return 0
    owners = inventory["owners"]
    legacy = {(*group["target"], name) for group in inventory["groups"] for name in group["cases"]}
    discovered = {(*key, name) for key, target in targets.items() for name in target["ignored"]}
    assigned = rule_assignments(owners, rules, legacy | discovered, platform)
    modules = RustModules()
    for case in sorted(discovered - legacy):
        owner = assigned.get(case)
        if owner is None or owners[owner]["platform"] == "win32":
            continue
        root = targets[case[:3]].get("root")
        if root is not None and not modules.still_ignored_on_windows(Path(root), case[3]):
            continue
        raise InventoryError(
            f"predicted on win32: rule-only case {case} discovered on win32 cannot be verified on "
            f"owner {owner}'s platform {owners[owner]['platform']}; "
            "add an exact legacy entry with the right owner, or a rule for an owner "
            "that runs on win32 (or keep the test out of the Windows build with a cfg such as "
            "#[cfg(unix)] on it or on its module)")
    return 0


def main():
    root = Path(__file__).resolve().parents[1]
    try:
        inventory = json.loads((root / "scripts/ignored-test-owners.json").read_text(encoding="utf-8"))
        rules = json.loads((root / "scripts/ignored-test-rules.json").read_text(encoding="utf-8"))
        require(isinstance(rules, dict), "ownership rules must be an object")
        targets = discover(root)
        count = validate(root, inventory, targets, sys.platform, rules)
        predict_win32(inventory, rules, targets, sys.platform)
        legacy = {(*group["target"], name): group["owner"]
                  for group in inventory["groups"] for name in group["cases"]}
        cases = set(legacy) | {(*key, name) for key, target in targets.items() for name in target["ignored"]}
        assigned = rule_assignments(inventory["owners"], rules, cases, sys.platform)
        covered = len(legacy.keys() & assigned.keys())
    except (InventoryError, OSError, subprocess.SubprocessError, json.JSONDecodeError) as error:
        raise SystemExit(f"ignored-test inventory failed: {error}") from error
    print(f"Ignored-test execution ownership verified ({count} owned cases, {sys.platform}).")
    print(f"Additive migration: {len(rules['rules'])} rules; {len(legacy)} legacy owners unchanged; "
          f"{covered} rule-covered legacy cases; {len(legacy) - covered} legacy-only cases.")


if __name__ == "__main__":
    main()
