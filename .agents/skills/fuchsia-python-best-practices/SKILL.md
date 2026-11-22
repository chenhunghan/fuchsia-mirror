---
name: fuchsia-python-best-practices
description: >
  Python coding conventions, immutability idioms, type annotation rules,
  visibility and import discipline, exception handling, and unit test mocking
  best practices for authoring and reviewing Python code in Fuchsia.
---

# Fuchsia Python best practices

Apply these rules when authoring or reviewing Python code in Fuchsia. These
practices complement the [Fuchsia Python style guide][python-style] and [Fuchsia
Python testing guide][python-testing].

## Immutability by default

### Construct immutable sequences and sets

- Never construct a mutable `list` when an immutable `tuple` suffices. Use
  `tuple(...)` or tuple literals for fixed sequences, comprehension results that
  are not mutated afterward, and return values.
- Never use a mutable `list`, `dict`, or `set` for a module or class constant.
  Use `frozenset` when element order and duplicates do not matter, and `tuple`
  when element order matters.
- Prefer `@dataclasses.dataclass(frozen=True)` over mutable dataclasses, and
  prefer `@dataclasses.dataclass(frozen=True)` over `typing.NamedTuple` in new
  code.

**Good:**

```python
_REQUIRED_TOOLS = frozenset({"ffx", "fvm", "zbi"})
_SEARCH_DIRS = ("bin", "lib", "share")


@dataclasses.dataclass(frozen=True)
class BuildArtifact:
    name: str
    path: pathlib.Path


def _collect_names(items: Collection[BuildArtifact]) -> Sequence[str]:
    return tuple(item.name for item in items if item.name)
```

**Avoid:**

```python
_REQUIRED_TOOLS = {"ffx", "fvm", "zbi"}
_SEARCH_DIRS = ["bin", "lib", "share"]


@dataclasses.dataclass
class BuildArtifact:
    name: str
    path: pathlib.Path


def _collect_names(items: list[BuildArtifact]) -> list[str]:
    return [item.name for item in items if item.name]
```

### Use abstract read-only collection types in signatures

- Prefer abstract, read-only collection interfaces from `collections.abc`
  (`Collection`, `Sequence`, `Mapping`, `Set`, `Iterator`, `Container`) over
  concrete mutable types (`list`, `dict`, `set`) in parameter and return type
  annotations unless the caller or callee genuinely requires mutation.
- Prefer `Collection[T]` over `Sequence[T]` when consumers only iterate, check
  membership, or query `len()` and do not integer-index or rely on element
  ordering.
- Avoid `collections.abc.Iterable` (and `typing.Iterable`); use `Collection` or
  `Iterator` from `collections.abc` instead (see [fxr/1848156][drop-iterable]):
  - **No multi-pass iteration contract:** `Iterable` does not specify whether it
    can be iterated over more than once. A consumer may iterate over an
    `Iterable` twice while a caller passes a single-pass generator or
    `Iterator`, and the type system will not catch the bug.
  - **No truthiness contract:** `Iterable` defines no contract for `bool(x)`. A
    consumer may write `if items:` assuming it means "yields at least one
    element", while a caller passes a generator expression or `Iterator`, which
    is always truthy (`True`) even when it yields zero elements.
  - Use `Collection[T]` in the common case where the value can be iterated
    repeatedly and has a known length and meaningful truthiness, or
    `Iterator[T]` when the function is genuinely a single-pass stream processor
    (and `Container[T]` on the rare occasion where only `in` membership checks
    are needed).
- Avoid `tuple[T, ...]` in type annotations. Almost every time you are tempted
  to write `tuple[T, ...]` in a parameter or return type specification,
  `Sequence[T]` or `Collection[T]` is the better choice (reserve `tuple[T1, T2]`
  for [fixed-arity heterogeneous tuples][product-type]).
- Use modern standard library types instead of deprecated `typing` aliases:
  - `Set` and `MutableSet` from `collections.abc` (not `typing.AbstractSet` or
    `typing.Set`)
  - `Sequence`, `Mapping`, `Collection`, `Iterator` from `collections.abc` (not
    `typing.*` equivalents)
  - `re.Pattern` and `re.Match` (not `typing.Pattern` or `typing.Match`)
  - `Self` from `typing` for methods returning `self` (such as `__enter__` and
    `__aenter__`)
- Prefer importing `Callable` from `typing` rather than from `collections.abc`
  (`from typing import Callable`). Unlike `Collection`, `Sequence`, `Mapping`,
  and `Set`, a callable is not a collection of elements, and CPython core and
  Typing Council maintainers have confirmed in [python/cpython#131040
  (comment)][cpython-callable] (and the surrounding discussion in that issue)
  that `typing.Callable` will not be removed.

### Use single-assignment form and avoid copy-then-mutate

- Never reassign behavioral function parameters.
- Favor single-assignment form for local variables: tuck the variable assignment
  into both the `if:` and `else:` branches rather than assigning a default value
  before the `if:` and overwriting it inside the `if:` body.
- Avoid calling `.copy()` on a `dict` only to mutate the copy. Use dictionary
  union operators (`|` and `|=`) instead.

**Good:**

```python
if custom_timeout is not None:
    timeout = custom_timeout
else:
    timeout = _DEFAULT_TIMEOUT

env = os.environ | {"FUCHSIA_DIR": str(fuchsia_dir)}
record |= extra_fields
```

**Avoid:**

```python
timeout = _DEFAULT_TIMEOUT
if custom_timeout is not None:
    timeout = custom_timeout

env = os.environ.copy()
env["FUCHSIA_DIR"] = str(fuchsia_dir)
```

## Minimal and non-ornamental type annotations and defaults

### Omit ornamental return annotations

- Omit `-> None` on functions and methods (including `__init__` and unit test
  methods) that do not contain a `return <expression>` statement. Return types
  of procedures that do not return a value are inferred automatically.
- For code that is still being type-checked with mypy, ornamental `-> None`
  annotations may be necessary until Fuchsia's use of mypy is completely
  decommissioned.

### Omit local variable and constant type annotations

- Do not add type annotations to behavior-local variables or
  `_ALL_CAPS_WITH_UNDERSCORES` module constants.
- Do not use `typing.Final` on `_ALL_CAPS_WITH_UNDERSCORES` constants; the
  naming convention already communicates constancy.
- If a local variable's type seems unclear at initialization, give the variable
  a more descriptive name rather than a type annotation.
- If the type checker requires a local variable annotation across a branching
  assignment, declare the type on its own line before the `if:` statement rather
  than attaching it to one branch.

**Good:**

```python
_MAX_RETRIES = 3


def _run_step(command: Sequence[str]):
    completed_process = subprocess.run(command, check=True, text=True)
```

**Avoid:**

```python
_MAX_RETRIES: typing.Final[int] = 3


def _run_step(command: Sequence[str]) -> None:
    completed_process: subprocess.CompletedProcess[str] = subprocess.run(
        command, check=True, text=True
    )
```

### Avoid legacy future-annotations boilerplate

- Fuchsia uses Python 3.14+ (which implements PEP 649 deferred evaluation of
  annotations). Do not add `from __future__ import annotations`, string-quoted
  type annotations, or `if typing.TYPE_CHECKING:` blocks in new code.

### Omit unused parameter defaults

- Do not add default parameter values (`= False`, `= 0`, `| None = None`),
  especially on module-private helper functions or dataclass fields, when every
  caller passes an explicit argument. Add defaults only when callers genuinely
  omit the parameter.
- Represent an absent optional string as `str | None` (with `None` for the
  absent state) rather than using the empty string `""` as an in-band sentinel.

## Visibility, imports, and module layout

### Default to module-private visibility

- Prefix every module-scope constant (`_CONSTANT_NAME`), helper function
  (`_helper_name`), class (`_HelperClass`), and exception (`_CustomError`) with
  a single leading underscore unless it is part of the module's public API and
  imported by external non-test modules.
- Do not define `__all__` when leading underscores on private symbols suffice.

### Import modules and packages, except for typing and collections.abc

- Follow the [Google Python Style Guide import rules][google-py-imports]:
  `import` packages and modules only, never individual classes, functions, or
  constants from a module, and reference imported symbols via their module
  namespace at use sites.
- **Exception for `typing` and `collections.abc` type annotations:** For code
  elements used in type annotations that come from either the `typing` module or
  the `collections.abc` module, import the code element directly and use it
  nakedly in type annotations. For all other code elements (including runtime
  functions like `typing.cast` or types from other modules like `pathlib.Path`
  and `re.Pattern`), the "import only packages and modules" rule still applies.

**Good:**

```python
from collections.abc import Mapping, Sequence
import dataclasses
import pathlib
import typing
from typing import Callable
from unittest import mock

names = typing.cast(Sequence[str], names_json)
```

**Avoid:**

```python
import collections.abc
from dataclasses import dataclass
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, patch


def my_func(names: collections.abc.Set[str]):
    ...
```

### Keep classes short and place helpers before callers

- Do not attach `@staticmethod` methods or helper methods that do not use `self`
  to a class. Extract them into module-scope, module-private helper functions
  (`_helper(...)`) placed earlier in the file than the class.
- Order module contents top-to-bottom so definitions appear before their use:
  1.  Module docstring and `import` statements
  2.  Module-private constants (`_ALL_CAPS`) before all `class` and `def` blocks
  3.  Module-private helper functions and classes before the public functions or
      classes that call them
  4.  Public API classes and functions, followed by `main()` and the `if
      __name__ == "__main__":` guard
- When referring to source file ordering in comments or documentation,
  consistently use *"before"* and *"after"* rather than *"above"* and *"below"*
  (and never mix the two pairs).

### Prefer direct returns, descriptive names, and concise docstrings

- Replace `x = <expr>; return x` with `return <expr>` unless the local variable
  name communicates essential meaning not already clear from the function's name
  and signature.
- When a value is bound to a local variable before being returned, avoid naming
  it `ret`, `res`, `return_value`, `result`, or similar names that only indicate
  it will be returned. Instead, use a name that describes the object bound to
  the name (for example, `matching_targets` or `parsed_header`), just as you
  would for any other field or variable.
- Module-private helper functions with straightforward bodies and clear names do
  not require docstrings.
- Do not include a `Raises:` section in a docstring merely to document how a
  function fails when given invalid arguments that violate its API contract;
  state parameter preconditions in the `Args:` description instead. Reserve
  `Raises:` for exceptions that are part of the normal, expected operational
  interface.
- Attribute `TODO` and `NOTE` comments with a username (`# TODO(username): ...`
  or `Bug: ...`, `# NOTE(username): ...`). Phrase `NOTE` comments descriptively
  (for example, `# NOTE(username): We pass --force because ...`) rather than as
  imperative commands to the reader.

## Exceptions over error sentinels and swallowing

### Let exceptions propagate

- Do not wrap operations in `try: ... except OSError:` (or broad `except
  Exception:`) or pass `check=False` to `subprocess.run` just to log an error
  and return `None`, `False`, or an empty collection. Let exceptions propagate
  to callers or out of `main()`.
- Catch exceptions only when handling a specific, expected recovery case or at a
  top-level batch boundary where continuing across independent items is
  explicitly required.

### Keep return semantics pure

- Never overload a function's return value to represent both normal domain
  results and operational failures (for example, returning `None` for both "no
  lint issues found" and "the linter binary crashed", or returning `(bool,
  result)` status tuples). Return the domain result on success and raise an
  exception on failure.

### Distinguish internal invariants from external input validation

- Use `assert` statements for internal invariants (conditions where a violation
  indicates a bug in the module's own code). Prefer two separate `assert`
  statements over `assert a and b` so failures pinpoint the exact condition.
- Raise `ValueError` (or a domain-specific exception) when validating external
  input, CLI arguments, files, or data across a trust boundary.

### Do not log and raise

- Inside an `except:` block, do not both log an error (`_LOGGER.error(...)` or
  `print(..., file=sys.stderr)`) and raise an exception. Include the relevant
  diagnostic context in the raised exception's message and chain the cause with
  `raise CustomError(f"Failed to parse {path}: {e}") from e`.

## Idioms and unit test mocking

### Write idiomatic control flow and subprocess calls

- Use the assignment expression (walrus) operator (`:=`) to avoid redundant
  function calls, regex matches, or dictionary lookups:

  ```python
  if (match := _HEADER_PATTERN.match(line)) is not None:
      return match.group(1)

  non_empty = tuple(
      stripped for line in text.splitlines() if (stripped := line.strip())
  )
  ```

- Always write `None` checks explicitly as `is None` or `is not None` rather
  than relying on truthiness. Prefer explicit conditional expressions (`return
  "\n".join(lines) if lines else None`) over truthiness short-circuits (`return
  "\n".join(lines) or None`).
- Consider `match` / `case` statements over repetitive `if` / `elif` chains or
  ad-hoc `isinstance` and sequence-length dispatch.
- Decorate `enum.Enum` subclasses with `@enum.unique`.
- Pass `text=True` to `subprocess.run` and `subprocess.Popen` when working with
  text streams; never use the legacy `universal_newlines=True` alias.

### Mock at the system-under-test boundary in unit tests

- Always prefer `mock.patch.object` over string-based `mock.patch`.
- Apply `@mock.patch.object(...)` as a decorator on the test method rather than
  nesting `with mock.patch.object(...):` blocks inside the test method body,
  unless context-manager scoping is strictly necessary for a sub-step of the
  test.
- Patch dependencies on the module under test or instance under test (for
  example, `@mock.patch.object(module_under_test, "subprocess", autospec=True,
  spec_set=True)`) rather than mutating attributes on standard library modules
  globally (never patch `subprocess.run` directly on the global `subprocess`
  module).
- Pass `autospec=True` (and `spec_set=True` where supported) to
  `mock.patch.object` and `mock.create_autospec` so mocks enforce the real API
  signature.
- Prefer constructing real value objects (such as frozen dataclasses,
  `pathlib.Path`, or `subprocess.CompletedProcess`) in tests instead of mocking
  data objects.
- Use `self.assertSequenceEqual(actual, expected)` rather than
  `self.assertEqual(actual, [...])` when asserting the contents and order of
  sequences in `unittest.TestCase`.

**Good:**

```python
from unittest import mock

import my_module


class MyModuleTest(unittest.TestCase):

    @mock.patch.object(
        my_module, "subprocess", autospec=True, spec_set=True
    )
    def test_fetch_targets(self, mock_subprocess):
        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            args=("fx", "list-targets"), returncode=0, stdout="core.x64\n"
        )

        targets = my_module.fetch_targets()

        self.assertSequenceEqual(targets, ("core.x64",))
```

**Avoid:**

```python
from unittest.mock import patch

import my_module


class MyModuleTest(unittest.TestCase):

    def test_fetch_targets(self) -> None:
        with patch("subprocess.run") as mock_run:
            mock_run.return_value.stdout = "core.x64\n"
            targets = my_module.fetch_targets()
            self.assertEqual(targets, ["core.x64"])
```

## Self-review checklist

Before finishing a Python change, verify that:

- [ ] Fixed sequences and comprehension results that are not mutated use
  `tuple`, and unordered constants use `frozenset`.
- [ ] New dataclasses use `@dataclasses.dataclass(frozen=True)`.
- [ ] Function signatures use `collections.abc` read-only interfaces
  (`Collection`, `Sequence`, `Mapping`, `Set`, `Iterator`) instead of `list`,
  `dict`, `set`, `tuple[T, ...]`, or `Iterable`, and `typing.Callable` instead
  of `collections.abc.Callable`.
- [ ] Functions and methods without `return <expr>` omit ornamental `-> None`
  (unless the target is still type-checked with mypy).
- [ ] Local variables and `_ALL_CAPS` constants do not carry type annotations or
  `typing.Final`.
- [ ] `from __future__ import annotations` and `if TYPE_CHECKING:` are not used.
- [ ] Unused parameter defaults and unnecessary `| None = None` defaults are
  removed.
- [ ] All non-exported constants, helpers, classes, and exceptions start with a
  leading `_`, and helpers that do not use `self` are module-private functions
  placed before their callers.
- [ ] Local variables are not named `ret`, `res`, `return_value`, `result`, or
  similar; they are named for the domain objects bound to them (or returned
  directly via `return <expr>`).
- [ ] `import` statements import modules or packages only, except for type
  annotation symbols from `typing` and `collections.abc`, which are imported
  directly and used nakedly.
- [ ] Exceptions propagate naturally without being swallowed into `None` or
  `False` return sentinels or logged-and-reraised.
- [ ] Unit tests use `@mock.patch.object` with `autospec=True` on the module
  under test or instance under test and `self.assertSequenceEqual` for sequence
  comparisons.

[python-style]: /docs/development/languages/python/python_style.md
[python-testing]: /docs/development/languages/python/testing.md
[drop-iterable]: https://fuchsia-review.googlesource.com/c/fuchsia/+/1848156
[product-type]: https://en.wikipedia.org/wiki/Product_type
[cpython-callable]: https://github.com/python/cpython/issues/131040#issuecomment-2711000665
[google-py-imports]: https://google.github.io/styleguide/pyguide.html#22-imports
