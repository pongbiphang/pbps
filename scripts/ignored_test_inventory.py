"""Compiled-test discovery and bounded scheduling witnesses (DEC-1130.1)."""

import ast
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


def static_value(node, values, *, subscripts=False):
    """Only literal selector data; never import or execute a fixture."""
    def read(child, bindings=values):
        return static_value(child, bindings, subscripts=subscripts)

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
        require(isinstance(loop.target, ast.Name) and not loop.ifs and not loop.is_async,
                "unsupported selector comprehension")
        return [read(node.elt, dict(values, **{loop.target.id: value}))
                for value in read(loop.iter)]
    if subscripts and isinstance(node, ast.Subscript):
        container = read(node.value)
        require(type(container) in (list, tuple, str), "unsupported literal subscription")
        def index(part):
            if part is None:
                return None
            # Literal signed integer bounds cannot dispatch a user __index__.
            try:
                value = ast.literal_eval(part)
            except (ValueError, TypeError):
                raise InventoryError("unsupported literal index") from None
            require(type(value) is int, "unsupported literal index")
            return value
        key = (slice(index(node.slice.lower), index(node.slice.upper), index(node.slice.step))
               if isinstance(node.slice, ast.Slice) else index(node.slice))
        try:
            return container[key]
        except (IndexError, ValueError):
            raise InventoryError("invalid literal subscription") from None
    raise InventoryError("selector is not supported literal data: " + ast.dump(node))


class SelectorEffects(ast.NodeVisitor):
    """Visible writes/escapes outside function-local bodies invalidate evidence."""
    def __init__(self, proven_assignment=None, harmless_reflection=None, literal_class=None, values=None):
        self.literal_class = literal_class
        self.values = values
        self.exposed_ids = set()
        self.proven_assignment = proven_assignment
        self.harmless_reflection = harmless_reflection
        self.namespace_exposed = False
        self.writes = set()
        self.deletes = set()
        self.mutations = set()
        self.global_writes = set()
        self.global_mutations = set()
        self.loads = set()

    def references(self, node):
        self.mutations.update(n.id for n in ast.walk(node) if isinstance(n, ast.Name))

    def visit_Name(self, node):
        if isinstance(node.ctx, (ast.Store, ast.Del)):
            self.writes.add(node.id)
            if isinstance(node.ctx, ast.Del):
                self.deletes.add(node.id)
        elif isinstance(node.ctx, ast.Load):
            self.loads.add(node.id)

    def visit_Assign(self, node):
        # Only the already evaluated top-level literal has known alias identity.
        # A nested/unsupported assignment can retain a mutable RHS elsewhere.
        if node is not self.proven_assignment:
            self.references(node.value)
        self.generic_visit(node)

    def visit_NamedExpr(self, node):
        self.references(node.value)
        self.generic_visit(node)

    def visit_AugAssign(self, node):
        # A list += changes its aliases too; rebinding a name normally does not.
        self.references(node.target)
        self.references(node.value)
        self.generic_visit(node)

    def visit_Attribute(self, node):
        if isinstance(node.ctx, (ast.Store, ast.Del)):
            self.references(node.value)
        self.generic_visit(node)

    def visit_Subscript(self, node):
        # All three subscription protocols receive the key as an argument.
        self.references(node.slice)
        self.visit_Attribute(node)

    def visit_Call(self, node):
        if node is self.harmless_reflection:
            # The recognized call inspects a fresh empty object, not a module.
            return
        if isinstance(node.func, ast.Name) and node.func.id in ("exec", "eval", "globals", "locals", "vars"):
            # An escaped namespace can also change bindings created later.
            self.writes.add("*")
            self.namespace_exposed = True
        # Unknown callees may mutate mutable arguments or method receivers.
        if isinstance(node.func, ast.Attribute):
            self.references(node.func.value)
        for argument in [*node.args, *(k.value for k in node.keywords)]:
            self.references(argument)
        self.generic_visit(node)

    def visit_AnnAssign(self, node):
        if node.value is not None:
            self.references(node.value)
            self.visit(node.target)
            self.visit(node.value)
        self.references(node.annotation)
        self.visit(node.annotation)

    def visit_FunctionDef(self, node):
        self.writes.add(node.name)
        # Defining a helper does not run its body; defaults can retain aliases.
        for expression in [*node.decorator_list, node.args]:
            self.references(expression)
            self.visit(expression)
        if node.returns is not None:
            self.references(node.returns)
            self.visit(node.returns)

    visit_AsyncFunctionDef = visit_FunctionDef

    def visit_Lambda(self, node):
        self.references(node.args)
        self.visit(node.args)

    def visit_ClassDef(self, node):
        self.writes.add(node.name)
        for expression in [*node.decorator_list, *node.bases, *node.keywords]:
            self.visit(expression)
        # Only a direct module class with proven construction effects receives
        # this exemption. Unknown hooks can invalidate even immutable reads.
        exposed = None
        if node is self.literal_class and not node.decorator_list:
            try:
                exposed = set()
                for base in node.bases:
                    value = static_value(base.value if isinstance(base, ast.Starred) else base,
                                         self.values, subscripts=True)
                    if isinstance(base, ast.Starred):
                        require(type(value) in (list, tuple, str), "unsupported literal base iterable")
                        exposed.update(identity for item in value for identity in mutable_ids(item))
                    else:
                        exposed.update(mutable_ids(value))
            except (InventoryError, TypeError):
                exposed = None
        if exposed is None:
            for base in node.bases:
                self.references(base)
        else:
            self.exposed_ids.update(exposed)
        for keyword in node.keywords:
            # Metaclass and subclass hooks can retain or mutate keyword values.
            self.references(keyword.value)
        # Class assignments are local unless explicitly declared global.
        def declared_globals(statement):
            if isinstance(statement, ast.Global):
                return set(statement.names)
            if isinstance(statement, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef, ast.Lambda)):
                return set()
            return set().union(*(declared_globals(child) for child in ast.iter_child_nodes(statement)))
        globals_ = set().union(*(declared_globals(statement) for statement in node.body))
        locals_ = set()
        for statement in node.body:
            body = SelectorEffects()
            body.visit(statement)
            self.namespace_exposed |= body.namespace_exposed
            writes = (body.writes & (globals_ | {"*"})) | body.global_writes
            self.writes.update(writes)
            self.global_writes.update(writes)
            # A possible deletion exposes the module binding even to later
            # reads inside this same compound statement.
            locals_.difference_update(body.deletes)
            # A module list retained by a class can be mutated through that alias.
            escaped = ((body.loads | body.mutations) - locals_) | body.global_mutations
            escaped.update((body.loads | body.mutations) & globals_)
            self.mutations.update(escaped)
            self.global_mutations.update(escaped)
            if isinstance(statement, (ast.Assign, ast.AnnAssign, ast.AugAssign,
                                      ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef,
                                      ast.Import, ast.ImportFrom)):
                locals_.update(body.writes - globals_ - body.global_writes)

    def visit_alias(self, node):
        self.writes.add(node.asname or node.name.split(".")[0])

    def visit_ExceptHandler(self, node):
        if node.name:
            self.writes.add(node.name)
            # Python clears the exception target when its handler exits.
            self.deletes.add(node.name)
        self.generic_visit(node)

    def visit_MatchAs(self, node):
        if node.name:
            self.writes.add(node.name)
        self.generic_visit(node)

    visit_MatchStar = visit_MatchAs

    def visit_MatchMapping(self, node):
        if node.rest:
            self.writes.add(node.rest)
        self.generic_visit(node)

    def visit_comprehension(self, node):
        # Comprehension targets are local, but walrus writes in expressions are not.
        self.visit(node.iter)
        for condition in node.ifs:
            self.visit(condition)


def mutable_ids(value):
    if isinstance(value, (list, tuple)):
        # A tuple cannot change, but it can still expose a shared mutable child.
        own = {id(value)} if isinstance(value, list) else set()
        return own.union(*(mutable_ids(item) for item in value))
    return set()


class NamespaceExposure(ast.NodeVisitor):
    """Potential builtin aliases, independent of pristine object-inspection proof."""
    reflective = frozenset({"globals", "locals", "vars", "exec", "eval"})

    def __init__(self, module=None):
        self.bindings = {}
        self.module = self.bindings if module is None else module
        self.globals = set()
        self.exposed = False
        self.harmless = None
        self.conditional = False

    def value(self, node):
        if isinstance(node, ast.Name):
            fallback = self.module.get(node.id, {node.id} if node.id in self.reflective else set())
            return fallback if node.id in self.globals else self.bindings.get(node.id, fallback)
        if (isinstance(node, ast.Attribute) and node.attr in self.reflective
            and "builtins" in self.value(node.value)):
            return {node.attr}
        return set()

    def bind(self, name, value):
        value = set(value)
        if self.conditional:
            value |= self.value(ast.Name(id=name, ctx=ast.Load()))
        if name in self.globals:
            self.module[name] = value
        else:
            self.bindings[name] = value

    def visit(self, node):
        # Unknown control flow cannot prove a previous reflective alias gone.
        # Retain either binding; direct statements still distinguish shadows.
        conditional = self.conditional
        if isinstance(node, (ast.If, ast.For, ast.AsyncFor, ast.While, ast.Try,
                             ast.TryStar, ast.Match)):
            self.conditional = True
        try:
            return super().visit(node)
        finally:
            self.conditional = conditional

    def visit_Call(self, node):
        if node is self.harmless:
            return
        if self.value(node.func) & self.reflective:
            self.exposed = True
        self.generic_visit(node)

    def visit_Assign(self, node):
        value = self.value(node.value)
        self.visit(node.value)
        for target in node.targets:
            self.visit(target)
            if isinstance(target, ast.Name):
                self.bind(target.id, value)

    def visit_NamedExpr(self, node):
        value = self.value(node.value)
        self.visit(node.value)
        if isinstance(node.target, ast.Name):
            self.bind(node.target.id, value)

    def visit_AnnAssign(self, node):
        if node.value is not None:
            value = self.value(node.value)
            self.visit(node.value)
            self.visit(node.target)
            if isinstance(node.target, ast.Name):
                self.bind(node.target.id, value)
        self.visit(node.annotation)

    def visit_Import(self, node):
        # Apply each import in order: a repeated spelling has one final binding.
        for alias in node.names:
            self.bind(alias.asname or alias.name.split('.')[0],
                      {'builtins'} if alias.name == 'builtins' else set())

    def visit_ImportFrom(self, node):
        for alias in node.names:
            if alias.name == '*':
                continue
            self.bind(alias.asname or alias.name,
                      {alias.name} if not node.level and node.module == 'builtins'
                      and alias.name in self.reflective else set())

    def visit_Delete(self, node):
        for target in node.targets:
            self.visit(target)
            if isinstance(target, ast.Name) and not self.conditional:
                self.bindings.pop(target.id, None)
                if target.id in self.globals:
                    self.module.pop(target.id, None)

    def visit_FunctionDef(self, node):
        for expression in [*node.decorator_list, node.args]:
            self.visit(expression)
        if node.returns is not None:
            self.visit(node.returns)
        self.bind(node.name, set())

    visit_AsyncFunctionDef = visit_FunctionDef

    def visit_Lambda(self, node):
        self.visit(node.args)

    def visit_ClassDef(self, node):
        for expression in [*node.decorator_list, *node.bases, *node.keywords]:
            self.visit(expression)
        # A nested class does not close over an enclosing class's locals.
        body = NamespaceExposure(self.module)
        body.conditional = self.conditional
        def globals_in(statement):
            if isinstance(statement, ast.Global):
                return set(statement.names)
            if isinstance(statement, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef, ast.Lambda)):
                return set()
            return set().union(*(globals_in(child) for child in ast.iter_child_nodes(statement)))
        body.globals = set().union(*(globals_in(statement) for statement in node.body))
        for statement in node.body:
            body.visit(statement)
        self.exposed |= body.exposed
        self.bind(node.name, set())


class NamespaceImports:
    """Recognize only direct module-level inspection of a fresh empty object."""
    def __init__(self, values):
        self.values = values
        self.constructors = set()
        self.modules = set()
        self.types_pristine = True
        self.builtin_modules = set()
        self.vars_builtin = True
        self.inert_prefix = True
        self.inert_names = set()
        self.fresh_dicts = set()

    def inert_expression(self, node):
        # Reuse the selector grammar: its proven native values cannot dispatch
        # user callbacks through concatenation or supported comprehensions.
        try:
            static_value(node, self.values)
            return True
        except (InventoryError, TypeError):
            pass
        # Definitions and fresh dictionaries can be inert without literal values.
        if isinstance(node, ast.Name):
            return node.id in self.inert_names
        if isinstance(node, (ast.List, ast.Tuple, ast.Set)):
            return all(self.inert_expression(item) for item in node.elts)
        if isinstance(node, ast.Dict):
            return all(isinstance(key, ast.Constant) and self.inert_expression(value)
                       for key, value in zip(node.keys, node.values))
        if isinstance(node, ast.Lambda):
            return self.inert_arguments(node.args)
        return False

    def inert_arguments(self, arguments):
        annotations = [arg.annotation for arg in [*arguments.posonlyargs, *arguments.args,
                       *arguments.kwonlyargs, *([arguments.vararg] if arguments.vararg else []),
                       *([arguments.kwarg] if arguments.kwarg else [])]]
        return all(value is None or self.inert_expression(value)
                   for value in [*arguments.defaults, *arguments.kw_defaults, *annotations])

    def inert_statement(self, statement, harmless):
        if isinstance(statement, ast.Import):
            return all(alias.name in ("types", "sys", "builtins") for alias in statement.names)
        if isinstance(statement, ast.ImportFrom):
            return (statement.module == "types" and not statement.level
                    and all(alias.name in ("SimpleNamespace", "ModuleType")
                            for alias in statement.names))
        if isinstance(statement, (ast.FunctionDef, ast.AsyncFunctionDef)):
            return (not statement.decorator_list and not getattr(statement, "type_params", [])
                    and self.inert_arguments(statement.args)
                    and (statement.returns is None or self.inert_expression(statement.returns)))
        if isinstance(statement, ast.Pass):
            return True
        if isinstance(statement, ast.Expr):
            return harmless is not None or self.inert_expression(statement.value)
        if isinstance(statement, ast.Assign):
            if all(isinstance(target, ast.Name) for target in statement.targets):
                return harmless is not None or self.inert_expression(statement.value)
            # Existing safe inspections may populate their own fresh dict.
            # Only literal keys and inert values exclude user-defined hooks.
            return (self.inert_expression(statement.value)
                    and all(isinstance(target, ast.Subscript)
                            and isinstance(target.value, ast.Name)
                            and target.value.id in self.fresh_dicts
                            and isinstance(target.slice, ast.Constant)
                            and isinstance(target.slice.value, str)
                            for target in statement.targets))
        return False

    def harmless_call(self, statement):
        if not self.inert_prefix or not self.vars_builtin or not isinstance(statement, (ast.Assign, ast.Expr)):
            return None
        if isinstance(statement, ast.Assign) and not all(isinstance(target, ast.Name)
                                                        for target in statement.targets):
            return None
        call = statement.value
        if not (isinstance(call, ast.Call) and isinstance(call.func, ast.Name)
                and call.func.id == "vars" and len(call.args) == 1 and not call.keywords):
            return None
        instance = call.args[0]
        if not isinstance(instance, ast.Call) or instance.args or instance.keywords:
            return None
        constructor = instance.func
        direct = isinstance(constructor, ast.Name) and constructor.id in self.constructors
        qualified = (isinstance(constructor, ast.Attribute) and constructor.attr == "SimpleNamespace"
                     and isinstance(constructor.value, ast.Name) and constructor.value.id in self.modules)
        return call if direct or qualified else None

    def advance(self, statement, effects):
        harmless = self.harmless_call(statement)
        inert = self.inert_statement(statement, harmless)
        # Later imports cannot undo effects of a call, import, decorator or
        # class hook whose execution was not proved inert. This restricts only
        # the reflection exemption; ordinary literal selectors remain usable.
        self.inert_prefix &= inert
        self.inert_names.difference_update(effects.writes)
        self.fresh_dicts.difference_update(effects.writes)
        if inert and isinstance(statement, ast.Assign):
            names = {target.id for target in statement.targets if isinstance(target, ast.Name)}
            self.inert_names.update(names)
            if harmless is not None:
                self.fresh_dicts.update(names)
        if inert and isinstance(statement, (ast.FunctionDef, ast.AsyncFunctionDef)):
            self.inert_names.add(statement.name)
        # Rebinding, an attribute write or an opaque escape loses provenance.
        touched = effects.writes | effects.mutations
        if self.builtin_modules & effects.mutations:
            self.vars_builtin = False
        self.builtin_modules.difference_update(touched)
        if self.modules & effects.mutations:
            # Imports share a cached module: another alias or reimport cannot
            # restore the original constructor after a possible module edit.
            self.types_pristine = False
            self.modules.clear()
        if "*" in touched:
            self.builtin_modules.clear()
            self.constructors.clear()
            self.modules.clear()
            self.types_pristine = False
            self.vars_builtin = False
        else:
            self.constructors.difference_update(touched)
            self.modules.difference_update(touched)
            if "vars" in effects.writes:
                self.vars_builtin = False
        # A repeated alias keeps the final import binding, not a union of
        # every module/constructor assigned to that spelling in the statement.
        if isinstance(statement, (ast.Import, ast.ImportFrom)):
            for alias in statement.names:
                name = alias.asname or alias.name.split(".")[0]
                self.constructors.discard(name)
                self.modules.discard(name)
                self.builtin_modules.discard(name)
                if isinstance(statement, ast.Import):
                    if alias.name == "builtins":
                        self.builtin_modules.add(name)
                    if self.types_pristine and alias.name == "types":
                        self.modules.add(name)
                elif (self.types_pristine and statement.module == "types"
                      and not statement.level and alias.name == "SimpleNamespace"):
                    self.constructors.add(name)



class ClassExecutionProof:
    """Bounded native-argument functions; no fixture code is executed."""
    def __init__(self):
        self.functions = {}
        self.bound = set()
        self.literal_mappings = set()

    def advance(self, node, effects):
        alias = (self.functions.get(node.value.id)
                 if isinstance(node, ast.Assign) and isinstance(node.value, ast.Name) else None)
        mapping = False
        if isinstance(node, ast.Assign):
            mapping = isinstance(node.value, ast.Name) and node.value.id in self.literal_mappings
            if isinstance(node.value, ast.Dict):
                try:
                    value = ast.literal_eval(node.value)
                    mapping = all(type(key) is str for key in value)
                except (ValueError, TypeError):
                    pass
        self.literal_mappings.difference_update(effects.writes)
        if mapping:
            self.literal_mappings.update(t.id for t in node.targets if isinstance(t, ast.Name))
        self.bound.update(effects.writes)
        for name in effects.writes:
            self.functions.pop(name, None)
        if isinstance(node, ast.FunctionDef):
            self.functions[node.name] = node
        elif alias is not None and all(isinstance(t, ast.Name) for t in node.targets):
            for target in node.targets:
                self.functions[target.id] = alias

    def callable(self, name, visiting=None, *, constructor=False):
        visiting = set() if visiting is None else visiting
        # Helpers must remain native-valued: a constructed class can expose
        # user-defined methods even through a familiar operation such as clear.
        if (name, constructor) in visiting:
            return True
        function = self.functions.get(name)
        if function is None or function.decorator_list or getattr(function, 'type_params', []):
            return False
        args = function.args
        parameters = [*args.posonlyargs, *args.args, *args.kwonlyargs,
                      *([args.vararg] if args.vararg else []), *([args.kwarg] if args.kwarg else [])]
        if (args.defaults or any(x is not None for x in args.kw_defaults)
            or function.returns or any(x.annotation for x in parameters)):
            return False
        # Only straight local bindings and native-container loops are supported.
        locals_ = {x.arg for x in parameters}
        for node in ast.walk(function):
            if isinstance(node, (ast.Assign, ast.For)):
                targets = node.targets if isinstance(node, ast.Assign) else [node.target]
                if any(not isinstance(target, ast.Name) for target in targets):
                    return False
                locals_.update(target.id for target in targets)
        visiting = visiting | {(name, constructor)}
        builtins = {'isinstance', 'list', 'tuple'}
        def builtin(name):
            return name not in self.bound and name not in locals_ and '__builtins__' not in self.bound
        def expression(node):
            if isinstance(node, ast.Constant): return True
            if isinstance(node, ast.Name):
                return node.id in locals_ or node.id in builtins and builtin(node.id)
            if isinstance(node, (ast.List, ast.Tuple)):
                return all(expression(item) for item in node.elts)
            if isinstance(node, ast.Subscript):
                return expression(node.value) and isinstance(node.slice, ast.Constant) and type(node.slice.value) is int
            if isinstance(node, ast.BoolOp): return all(expression(value) for value in node.values)
            if isinstance(node, ast.Call) and not node.keywords:
                if not all(expression(arg) for arg in node.args): return False
                if isinstance(node.func, ast.Attribute):
                    return node.func.attr == 'clear' and not node.args and expression(node.func.value)
                if isinstance(node.func, ast.Name) and node.func.id not in locals_:
                    callee = node.func.id
                    if callee in builtins and builtin(callee): return True
                    return self.callable(callee, visiting)
            return False
        def statement(node):
            if isinstance(node, ast.Pass): return True
            if isinstance(node, ast.Expr): return expression(node.value)
            if isinstance(node, ast.Assign): return expression(node.value)
            if isinstance(node, ast.If):
                return expression(node.test) and all(statement(s) for s in [*node.body, *node.orelse])
            if isinstance(node, ast.For):
                return expression(node.iter) and all(statement(s) for s in [*node.body, *node.orelse])
            if isinstance(node, ast.Return):
                value = node.value
                if value is None: return True
                if (constructor and isinstance(value, ast.Call) and isinstance(value.func, ast.Name)
                    and value.func.id == 'type'
                    and builtin('type') and not value.keywords and len(value.args) == 3
                    and isinstance(value.args[1], ast.Tuple) and not value.args[1].elts):
                    return expression(value.args[0]) and expression(value.args[2])
                return expression(value)
            return False
        return all(statement(node) for node in function.body)

    def safe(self, node, imports, values):
        if not imports.inert_prefix or node.decorator_list: return False
        metaclass = [keyword.value for keyword in node.keywords if keyword.arg == 'metaclass']
        ordinary = (not metaclass or len(metaclass) == 1 and isinstance(metaclass[0], ast.Name)
                    and metaclass[0].id == 'type' and 'type' not in self.bound)
        if not ordinary:
            if (len(metaclass) != 1 or not isinstance(metaclass[0], ast.Name)
                or not self.callable(metaclass[0].id, constructor=True)):
                return False
        else:
            # Empty native expansions use the ordinary object/type construction;
            # no user base or metaclass can add a protocol hook.
            if '__builtins__' in self.bound:
                return False
            try:
                for base in node.bases:
                    if not isinstance(base, ast.Starred):
                        return False
                    value = static_value(base.value, values, subscripts=True)
                    if type(value) not in (list, tuple, str) or value:
                        return False
            except (ValueError, TypeError):
                return False
        def literal(value, scope):
            if isinstance(value, ast.Name) and value.id in self.literal_mappings:
                return True
            if isinstance(value, ast.Dict):
                return all(isinstance(key, ast.Constant) and type(key.value) is str and literal(item, scope)
                           for key, item in zip(value.keys, value.values))
            try:
                static_value(value, scope, subscripts=True)
                return True
            except (ValueError, TypeError):
                return False
        for keyword in node.keywords:
            if keyword.arg == 'metaclass': continue
            if (keyword.arg is None and not isinstance(keyword.value, ast.Dict)
                and not (isinstance(keyword.value, ast.Name) and keyword.value.id in self.literal_mappings)):
                return False
            if not literal(keyword.value, values): return False
        scope = dict(values)
        for statement in node.body:
            if isinstance(statement, ast.Pass): continue
            if isinstance(statement, ast.Expr) and isinstance(statement.value, ast.Constant): continue
            if isinstance(statement, ast.Assign) and all(isinstance(t, ast.Name) for t in statement.targets):
                try:
                    value = static_value(statement.value, scope, subscripts=True)
                except (ValueError, TypeError):
                    return False
                for target in statement.targets: scope[target.id] = value
                continue
            if isinstance(statement, (ast.FunctionDef, ast.AsyncFunctionDef)):
                args = statement.args
                parameters = [*args.posonlyargs, *args.args, *args.kwonlyargs,
                              *([args.vararg] if args.vararg else []), *([args.kwarg] if args.kwarg else [])]
                evaluated = [*args.defaults, *args.kw_defaults, statement.returns,
                             *(parameter.annotation for parameter in parameters)]
                if (statement.decorator_list or getattr(statement, 'type_params', [])
                    or not all(value is None or literal(value, scope) for value in evaluated)):
                    return False
                # Plain function descriptors have no construction-time callback.
                # Do not treat that function as literal data in later statements.
                scope.pop(statement.name, None)
                continue
            return False
        return True


def python_values(tree):
    values = {}
    imports = NamespaceImports(values)
    class_proof = ClassExecutionProof()
    namespace_aliases = NamespaceExposure()
    namespace_exposed = False
    for node in tree.body:
        replacement = None
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            try:
                replacement = (node.targets[0].id, static_value(node.value, values))
            except (InventoryError, TypeError):
                pass
        harmless = imports.harmless_call(node)
        namespace_aliases.harmless = harmless
        namespace_aliases.exposed = False
        namespace_aliases.visit(node)
        literal_class = node if isinstance(node, ast.ClassDef) and class_proof.safe(node, imports, values) else None
        effects = SelectorEffects(node if replacement is not None or harmless is not None else None,
                                  harmless, literal_class, values)
        effects.visit(node)
        if namespace_aliases.exposed:
            effects.writes.add("*")
        class_proof.advance(node, effects)
        imports.advance(node, effects)
        namespace_exposed |= effects.namespace_exposed or namespace_aliases.exposed
        mutated = effects.exposed_ids.union(*(mutable_ids(values[name]) for name in effects.mutations if name in values))
        for name in list(values):
            if name in effects.writes or mutable_ids(values[name]) & mutated or "*" in effects.writes:
                del values[name]
        if replacement is not None and not namespace_exposed:
            values[replacement[0]] = replacement[1]
    return values


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


def script_condition(node):
    """Evaluate the bounded guards used by scripts launched as __main__."""
    if isinstance(node, ast.Constant):
        return bool(node.value)
    if isinstance(node, ast.Name) and node.id == "__name__":
        return True
    if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.Not):
        value = script_condition(node.operand)
        return None if value is None else not value
    if isinstance(node, ast.Compare) and len(node.ops) == 1:
        if isinstance(node.ops[0], (ast.Eq, ast.NotEq)):
            operands = [node.left, node.comparators[0]]
            if not all(isinstance(value, ast.Constant) or
                       isinstance(value, ast.Name) and value.id == "__name__" for value in operands):
                return None
            # Selector data normalizes tuples to lists; guard equality must not.
            left, right = [value.value if isinstance(value, ast.Constant) else "__main__"
                           for value in operands]
            return left == right if isinstance(node.ops[0], ast.Eq) else left != right
    return None


class ScriptNameBindings(ast.NodeVisitor):
    """A direct rebinding makes the interpreter's entry name unsafe to assume."""
    def visit_Name(self, node):
        require(node.id != "__name__" or not isinstance(node.ctx, (ast.Store, ast.Del)),
                "module entry name is rebound; audit the script entry explicitly")

    def visit_FunctionDef(self, node):
        require(node.name != "__name__", "module entry name is rebound")
        # Defaults/decorators execute at definition time, unlike local bodies.
        for expression in [*node.decorator_list, node.args]:
            self.visit(expression)
        if node.returns is not None:
            self.visit(node.returns)

    visit_AsyncFunctionDef = visit_FunctionDef

    def visit_Lambda(self, node):
        self.visit(node.args)

    def visit_ClassDef(self, node):
        require(node.name != "__name__", "module entry name is rebound")
        for expression in [*node.decorator_list, *node.bases, *node.keywords]:
            self.visit(expression)

    def visit_ExceptHandler(self, node):
        require(node.name != "__name__", "module entry name is rebound")
        self.generic_visit(node)

    def visit_MatchAs(self, node):
        require(node.name != "__name__", "module entry name is rebound")
        self.generic_visit(node)

    visit_MatchStar = visit_MatchAs

    def visit_MatchMapping(self, node):
        require(node.rest != "__name__", "module entry name is rebound")
        self.generic_visit(node)

    def visit_alias(self, node):
        require((node.asname or node.name.split(".")[0]) != "__name__",
                "module entry name is rebound")


class PruneModuleEntry(PruneInactive):
    def visit_If(self, node):
        condition = script_condition(node.test)
        if condition is None:
            # Neither branch is evidence when its entry condition is unknown.
            return None
        selected = node.body if condition else node.orelse
        return self.visit(ast.Module(body=selected, type_ignores=[])).body

    def visit_unsupported_control(self, node):
        # These forms need a separate scheduling adapter, not an execution guess.
        return None

    visit_While = visit_unsupported_control
    visit_For = visit_unsupported_control
    visit_AsyncFor = visit_unsupported_control
    visit_Try = visit_unsupported_control
    visit_TryStar = visit_unsupported_control
    visit_With = visit_unsupported_control
    visit_AsyncWith = visit_unsupported_control
    visit_Match = visit_unsupported_control


def python_scope(source, scope):
    tree = ast.parse(source)
    if scope == "<module>":
        ScriptNameBindings().visit(tree)
        body = tree.body
    else:
        functions = [n for n in tree.body if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)) and n.name == scope]
        require(len(functions) == 1, f"missing or ambiguous Python function: {scope}")
        body = [n for n in functions[0].body if not isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))]
    pruner = PruneModuleEntry() if scope == "<module>" else PruneInactive()
    active = pruner.visit(ast.Module(body=body, type_ignores=[]))
    return ast.unparse(ast.fix_missing_locations(active))


def python_tokens(source):
    result = []
    try:
        for token in tokenize.generate_tokens(io.StringIO(source).readline):
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


def check_witness(root, witness):
    source = (root / witness["file"]).read_text(encoding="utf-8")
    language = witness["language"]
    if language == "python":
        tokens = python_tokens(python_scope(source, witness["scope"]))
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
    block = re.search(r"^      matrix:\n((?:        .*\n?)*)", body, re.M)
    if block:
        for line in block[1].splitlines():
            axis = re.fullmatch(r"        ([\w-]+): \[([^\]]*)\]\s*", line)
            require(axis is not None, f"unsupported matrix line needs an explicit audit: {job}: {line.strip()}")
            require(axis[2].strip(), f"owner matrix has no known executed variant: {job}")
            # Only plain tokens: a quoted value may hold a comma, and splitting
            # one would invent variants that never run.
            values = [x.strip() for x in axis[2].split(",")]
            require(all(re.fullmatch(r"[\w.-]+", v) for v in values),
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


def owner_selectors(root, owner):
    selection = owner.get("selection")
    if not selection: return None
    source = (root / selection["file"]).read_text(encoding="utf-8"); tree = ast.parse(source); values = python_values(tree)
    kind = selection["kind"]
    if kind == "data":
        names = static_value(ast.parse(selection["expression"], mode="eval").body, values)
        if isinstance(names, str): names = [names]
        require(isinstance(names, (list, tuple)) and all(isinstance(n, str) for n in names), "selector must contain names")
        return {selection.get("prefix", "") + n for n in names}
    if kind == "calls":
        scoped = ast.parse(python_scope(source, selection["scope"]))
        names = set()
        for call in ast.walk(scoped):
            if not isinstance(call, ast.Call) or not isinstance(call.func, ast.Name) or call.func.id != selection["callee"]: continue
            if selection.get("ignored_keyword") and not any(k.arg == "ignored" and isinstance(k.value, ast.Constant) and k.value.value is True for k in call.keywords): continue
            require(len(call.args) > selection["argument"], "missing selected case argument")
            names.add(static_value(call.args[selection["argument"]], values))
        require(names and all(isinstance(n, str) for n in names), "no literal case calls")
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
        for witness in owner.get("witnesses", []): check_witness(root, witness)
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
        selected = owner_selectors(root, owner)
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
