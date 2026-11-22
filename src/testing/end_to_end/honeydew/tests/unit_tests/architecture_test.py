# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests verifying Honeydew package layout and ABC architecture rules."""

import abc
import ast
import dataclasses
import importlib
import inspect
import pathlib
import pkgutil
import textwrap
import types
import unittest
import zipfile
import zipimport
from collections import defaultdict
from collections.abc import Iterator, Mapping, Sequence
from typing import Any

import honeydew
from honeydew import (
    affordances,
    auxiliary_devices,
    device_classes,
    transports,
)
from honeydew.affordances import affordance as affordance_module
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports import transport as transport_module
from honeydew.utils import properties

# Domain ABCs that have only 1 concrete implementation in honeydew because
# additional concrete implementations live in vendor/google (ghoneydew).
# Cross-repo >= 2 enforcement is verified in ghoneydew's init_test.py.
#
# How to update honeydew and ghoneydew together:
# - Adding a 2nd implementation in ghoneydew for a 1-impl honeydew class (1 -> 2):
#   1. In honeydew (fxr/): split `<name>.py` into the `<Name>` ABC in `<name>.py`
#      and `<Name>Using<Impl>` in `<name>_using_<impl>.py`, and add
#      `<name>.<Name>` to `_CROSS_REPO_ABCS` below.
#   2. In ghoneydew (tqr/): add the 2nd implementation `<Name>Using<Impl2>`
#      inheriting from `honeydew`'s `<name>.<Name>`.
# - Removing the ghoneydew implementation so only 1 remains in honeydew (2 -> 1):
#   1. In ghoneydew (tqr/): delete the ghoneydew implementation first.
#   2. In honeydew (fxr/): consolidate `<Name>Using<Impl>` back into `<name>.py`
#      as concrete `class <Name>` and remove `<name>.<Name>` from
#      `_CROSS_REPO_ABCS` below.
_CROSS_REPO_ABCS: set[type[abc.ABC]] = {
    usb_power_hub.UsbPowerHub,
}

# Honeydew library modules mapped to the exact `mobly.*` targets they may
# import. Honeydew library code must remain test-framework agnostic:
# - Prefer native Python `assert`, standard Python exceptions, or Honeydew
#   errors instead of `mobly.asserts` or `mobly.signals`.
# - Never import or pass Mobly testbed configs, controller objects, or
#   `user_params` into Honeydew library code (keep those in `mobly_controller`
#   or test cases).
# - Reserve `_MOBLY_IMPORT_ALLOWLIST` strictly for unavoidable integration
#   points (such as propagating `mobly.signals.TestSignal` in core retry
#   helpers so test aborts/timeouts are not swallowed, or adapting a
#   Mobly-coupled third-party snippet client like `snippet_uiautomator`).
_MOBLY_IMPORT_ALLOWLIST = types.MappingProxyType(
    {
        "honeydew.affordances.ui_navigation.ui_navigation": frozenset(
            {
                "mobly.controllers.android_device",
                "mobly.controllers.android_device_lib.adb",
                "mobly.utils",
            }
        ),
        "honeydew.utils.common": frozenset({"mobly.signals"}),
        "honeydew.utils.control_flows": frozenset({"mobly.signals"}),
    }
)


def _module_name_for_parts(rel_parts: Sequence[str]) -> str | None:
    trimmed = (
        rel_parts[:-1]
        if rel_parts and rel_parts[-1] == "__init__"
        else rel_parts
    )
    if "tests" in trimmed:
        return None
    return ".".join(("honeydew", *trimmed))


def _load_honeydew_module_sources() -> Mapping[str, str]:
    loader = getattr(honeydew, "__loader__", None)
    if isinstance(loader, zipimport.zipimporter):
        prefix = f"{loader.prefix}honeydew/"
        with zipfile.ZipFile(loader.archive) as zf:
            return {
                module_name: zf.read(name).decode("utf-8")
                for name in zf.namelist()
                if name.startswith(prefix)
                and name.endswith(".py")
                and (
                    module_name := _module_name_for_parts(
                        name.removeprefix(prefix).removesuffix(".py").split("/")
                    )
                )
                is not None
            }

    return {
        module_name: py_file.read_text(encoding="utf-8")
        for root_str in honeydew.__path__
        for py_file in pathlib.Path(root_str).rglob("*.py")
        if (
            module_name := _module_name_for_parts(
                py_file.relative_to(root_str).with_suffix("").parts
            )
        )
        is not None
    }


def _iter_mobly_imports(source: str) -> Iterator[tuple[int, str]]:
    for node in ast.walk(ast.parse(source)):
        if isinstance(node, ast.Import):
            for alias in node.names:
                if alias.name == "mobly" or alias.name.startswith("mobly."):
                    yield (node.lineno, alias.name)
        elif (
            isinstance(node, ast.ImportFrom)
            and node.level == 0
            and node.module is not None
            and (node.module == "mobly" or node.module.startswith("mobly."))
        ):
            for alias in node.names:
                yield (node.lineno, f"{node.module}.{alias.name}")


def _find_mobly_imports(source: str) -> Sequence[tuple[int, str]]:
    return tuple(sorted(_iter_mobly_imports(source)))


class ArchitectureTests(unittest.TestCase):
    """Unit tests verifying Honeydew package layout and ABC architecture rules."""

    def test_no_single_implementation_abcs(self) -> None:
        """Verifies ABCs have 2+ implementations and concrete implementations match their ABC contract."""
        discovered_classes: set[type[Any]] = set()
        for pkg, prefix in (
            (affordances, "honeydew.affordances."),
            (auxiliary_devices, "honeydew.auxiliary_devices."),
            (transports, "honeydew.transports."),
        ):
            for _, module_name, ispkg in pkgutil.walk_packages(
                pkg.__path__, prefix
            ):
                parts = module_name.split(".")
                if "tests" in parts or "fidl_servers" in parts:
                    continue
                module = importlib.import_module(module_name)
                for _, obj in inspect.getmembers(module, inspect.isclass):
                    if (
                        obj.__module__ == module_name
                        and obj
                        not in (
                            affordance_module.Affordance,
                            transport_module.Transport,
                        )
                        and not getattr(obj, "_is_protocol", False)
                        and not dataclasses.is_dataclass(obj)
                        and not issubclass(obj, BaseException)
                    ):
                        self.assertFalse(
                            ispkg,
                            f"Affordance/transport class "
                            f"'{obj.__module__}.{obj.__qualname__}' is "
                            "defined directly in '__init__.py'. Move it into "
                            "a dedicated '<name>.py' module with a thin "
                            "'__init__.py' for consistent module layout and "
                            "import paths.",
                        )
                        discovered_classes.add(obj)

        abstract_classes = {
            cls for cls in discovered_classes if inspect.isabstract(cls)
        }
        concrete_classes = {
            cls for cls in discovered_classes if not inspect.isabstract(cls)
        }

        for abc_cls in abstract_classes:
            concrete_subclasses = [
                sub for sub in concrete_classes if issubclass(sub, abc_cls)
            ]
            if abc_cls in _CROSS_REPO_ABCS:
                self.assertGreaterEqual(
                    len(concrete_subclasses),
                    1,
                    f"Cross-repo ABC '{abc_cls.__module__}.{abc_cls.__qualname__}' "
                    "must have at least 1 concrete implementation in honeydew.",
                )
            else:
                self.assertGreaterEqual(
                    len(concrete_subclasses),
                    2,
                    f"Abstract class '{abc_cls.__module__}.{abc_cls.__qualname__}' "
                    f"has {len(concrete_subclasses)} concrete implementation(s) in "
                    f"honeydew ({[c.__qualname__ for c in concrete_subclasses]}). "
                    "Only define an ABC when 2+ concrete implementations exist "
                    "(or add it to _CROSS_REPO_ABCS if another implementation "
                    "lives in ghoneydew); for a single implementation, define "
                    "the concrete class directly in '<name>.py'.",
                )
            is_root_domain_abc = not any(
                issubclass(abc_cls, other) and abc_cls is not other
                for other in abstract_classes
            )
            if is_root_domain_abc:
                mod_leaf = abc_cls.__module__.rsplit(".", 1)[-1]
                self.assertFalse(
                    abc_cls.__name__.endswith("Abc")
                    or mod_leaf.endswith("_abc"),
                    f"Domain ABC '{abc_cls.__module__}.{abc_cls.__qualname__}' "
                    "must be named '<Name>' in '<name>.py' (not '<Name>Abc' in "
                    "'<name>_abc.py').",
                )
                self.assertEqual(
                    mod_leaf.replace("_", ""),
                    abc_cls.__name__.lower(),
                    f"Domain ABC '{abc_cls.__module__}.{abc_cls.__qualname__}' "
                    "must be defined in '<name>.py' matching its class name.",
                )
            for sub_cls in concrete_subclasses:
                for attr_name in dir(sub_cls):
                    if attr_name.startswith("_"):
                        continue
                    self.assertTrue(
                        hasattr(abc_cls, attr_name),
                        f"Concrete implementation '{sub_cls.__qualname__}' "
                        f"defines public attribute/method '{attr_name}' which "
                        f"is not declared on its ABC '{abc_cls.__qualname__}'. "
                        "All public methods and properties of a multi-implementation "
                        "affordance or transport must be part of its ABC contract.",
                    )
                    sub_attr = getattr(sub_cls, attr_name)
                    abc_attr = getattr(abc_cls, attr_name)
                    if callable(sub_attr) and callable(abc_attr):
                        sub_params = [
                            (p.name, p.kind, p.default)
                            for p in inspect.signature(
                                sub_attr
                            ).parameters.values()
                        ]
                        abc_params = [
                            (p.name, p.kind, p.default)
                            for p in inspect.signature(
                                abc_attr
                            ).parameters.values()
                        ]
                        self.assertEqual(
                            sub_params,
                            abc_params,
                            f"Method signature mismatch for "
                            f"'{sub_cls.__qualname__}.{attr_name}': "
                            f"expected parameters {abc_params} from ABC "
                            f"'{abc_cls.__qualname__}', got {sub_params}.",
                        )

    def test_affordance_and_transport_architecture(self) -> None:
        """Verifies single-impl properties return concrete classes and multi-impl properties return their ABC."""
        all_device_classes: list[type[fuchsia_device.FuchsiaDevice]] = [
            fuchsia_device.FuchsiaDevice
        ]
        for _, module_name, ispkg in pkgutil.walk_packages(
            device_classes.__path__, "honeydew.device_classes."
        ):
            if ispkg or "tests" in module_name.split("."):
                continue
            module = importlib.import_module(module_name)
            for _, obj in inspect.getmembers(module, inspect.isclass):
                if (
                    issubclass(obj, fuchsia_device.FuchsiaDevice)
                    and obj is not fuchsia_device.FuchsiaDevice
                    and obj.__module__.startswith("honeydew.device_classes.")
                ):
                    all_device_classes.append(obj)

        # Group concrete affordance, auxiliary device, and transport classes by
        # leaf package first so we know which packages have 1 vs. 2+ impls.
        # Note: Auxiliary devices do not yet share a common base class like
        # Affordance or Transport, so single-implementation auxiliary device
        # classes that do not inherit from an ABC are matched by package name or
        # standard implementation suffixes ("Using" / "Impl").
        package_impls: dict[str, set[type[Any]]] = defaultdict(set)
        for pkg, prefix, base_cls in (
            (
                affordances,
                "honeydew.affordances.",
                affordance_module.Affordance,
            ),
            (
                auxiliary_devices,
                "honeydew.auxiliary_devices.",
                abc.ABC,
            ),
            (
                transports,
                "honeydew.transports.",
                transport_module.Transport,
            ),
        ):
            for _, module_name, ispkg in pkgutil.walk_packages(
                pkg.__path__, prefix
            ):
                parts = module_name.split(".")
                if "tests" in parts or "fidl_servers" in parts:
                    continue
                module = importlib.import_module(module_name)
                if ispkg:
                    loader = getattr(module, "__loader__", None)
                    source = (
                        loader.get_source(module_name)
                        if loader is not None and hasattr(loader, "get_source")
                        else ""
                    ) or ""
                    init_tree = ast.parse(source)
                    non_docstring_stmts = [
                        stmt
                        for stmt in init_tree.body
                        if not (
                            isinstance(stmt, ast.Expr)
                            and isinstance(stmt.value, ast.Constant)
                            and isinstance(stmt.value.value, str)
                        )
                    ]
                    self.assertEqual(
                        non_docstring_stmts,
                        [],
                        f"Package '{module_name}' (__init__.py) must be empty "
                        "(license header / docstring only). Move classes, "
                        "imports, and re-exports into a dedicated '<name>.py' "
                        "module so all packages have a uniform import layout.",
                    )
                pkg_name = module_name.rsplit(".", 1)[0]
                pkg_leaf_flat = pkg_name.rsplit(".", 1)[-1].replace("_", "")
                for _, obj in inspect.getmembers(module, inspect.isclass):
                    if (
                        obj.__module__ == module_name
                        and not inspect.isabstract(obj)
                        and not dataclasses.is_dataclass(obj)
                        and not issubclass(obj, BaseException)
                        and (
                            issubclass(obj, base_cls)
                            or (
                                base_cls is abc.ABC
                                and (
                                    obj.__name__.lower() == pkg_leaf_flat
                                    or "Using" in obj.__name__
                                    or obj.__name__.endswith("Impl")
                                )
                            )
                        )
                    ):
                        package_impls[pkg_name].add(obj)

        property_impls: dict[str, set[type[Any]]] = defaultdict(set)
        property_annotations: list[
            tuple[type[fuchsia_device.FuchsiaDevice], str, type[Any]]
        ] = []

        for dev_cls in all_device_classes:
            for attr_name, attr_val in dev_cls.__dict__.items():
                if not isinstance(
                    attr_val, (properties.Affordance, properties.Transport)
                ):
                    continue

                source = textwrap.dedent(inspect.getsource(attr_val.func))
                tree = ast.parse(source)
                func_def = tree.body[0]
                self.assertIsInstance(
                    func_def, (ast.FunctionDef, ast.AsyncFunctionDef)
                )
                assert isinstance(
                    func_def, (ast.FunctionDef, ast.AsyncFunctionDef)
                )
                self.assertIsNotNone(
                    func_def.returns,
                    f"Property '{dev_cls.__qualname__}.{attr_name}' must have "
                    "a return type annotation.",
                )
                assert func_def.returns is not None
                return_type = eval(  # pylint: disable=eval-used
                    compile(
                        ast.Expression(func_def.returns),
                        filename="<annotation>",
                        mode="eval",
                    ),
                    attr_val.func.__globals__,
                )

                expected_base = (
                    affordance_module.Affordance
                    if isinstance(attr_val, properties.Affordance)
                    else transport_module.Transport
                )
                self.assertTrue(
                    isinstance(return_type, type)
                    and issubclass(return_type, expected_base),
                    f"Return type '{return_type}' of property "
                    f"'{dev_cls.__qualname__}.{attr_name}' must be a class "
                    f"inheriting from '{expected_base.__qualname__}' (not a "
                    "Union or TypeAlias).",
                )
                property_annotations.append((dev_cls, attr_name, return_type))
                if not inspect.isabstract(return_type):
                    property_impls[attr_name].add(return_type)

                # Inspect the AST of the property getter to find all concrete
                # classes instantiated and returned across branches.
                for node in ast.walk(tree):
                    if isinstance(node, ast.Return) and isinstance(
                        node.value, ast.Call
                    ):
                        call_func = node.value.func
                        resolved: Any = None
                        if isinstance(call_func, ast.Name):
                            resolved = attr_val.func.__globals__.get(
                                call_func.id
                            )
                        elif isinstance(
                            call_func, ast.Attribute
                        ) and isinstance(call_func.value, ast.Name):
                            mod_obj = attr_val.func.__globals__.get(
                                call_func.value.id
                            )
                            if mod_obj is not None:
                                resolved = getattr(
                                    mod_obj, call_func.attr, None
                                )
                        if (
                            isinstance(resolved, type)
                            and not inspect.isabstract(resolved)
                            and resolved.__module__.startswith(
                                (
                                    "honeydew.affordances.",
                                    "honeydew.transports.",
                                )
                            )
                        ):
                            property_impls[attr_name].add(resolved)
                            self.assertTrue(
                                issubclass(resolved, return_type),
                                f"Property '{dev_cls.__qualname__}.{attr_name}' "
                                f"returns '{resolved.__qualname__}', which is "
                                f"not a subclass of its return type annotation "
                                f"'{return_type.__qualname__}'.",
                            )

        for dev_cls, attr_name, return_type in property_annotations:
            pkg_name = return_type.__module__.rsplit(".", 1)[0]
            impls = property_impls[attr_name] | {
                cls
                for cls in package_impls.get(pkg_name, set())
                if issubclass(cls, return_type)
            }
            if len(impls) <= 1 and return_type not in _CROSS_REPO_ABCS:
                self.assertFalse(
                    inspect.isabstract(return_type),
                    f"Single-implementation property "
                    f"'{dev_cls.__qualname__}.{attr_name}' is annotated with "
                    f"abstract return type '{return_type.__qualname__}'. "
                    "When only 1 implementation exists, return the concrete "
                    "class directly so IDE Go-to-Definition jumps straight to "
                    "the implementation.",
                )

        generic_bases = {
            abc.ABC,
            affordance_module.Affordance,
            affordance_module.AsyncLazyReady,
            transport_module.Transport,
            object,
        }

        for pkg_name, impls in package_impls.items():
            if len({cls.__module__ for cls in impls}) == 1:
                for single_cls in impls:
                    if any(
                        issubclass(single_cls, cross_abc)
                        for cross_abc in _CROSS_REPO_ABCS
                    ):
                        continue
                    has_domain_abc = any(
                        inspect.isabstract(base) and base not in generic_bases
                        for base in single_cls.__mro__
                    )
                    self.assertFalse(
                        has_domain_abc,
                        f"Single-implementation class "
                        f"'{single_cls.__module__}.{single_cls.__qualname__}' in "
                        f"package '{pkg_name}' inherits from a domain ABC. "
                        "Consolidate single-implementation classes directly into "
                        "'<name>.py' as '<Name>' (or add the ABC to "
                        "_CROSS_REPO_ABCS if another implementation lives in "
                        "ghoneydew).",
                    )
                    mod_leaf = single_cls.__module__.rsplit(".", 1)[-1]
                    self.assertFalse(
                        "_using_" in mod_leaf
                        or mod_leaf.endswith("_impl")
                        or "Using" in single_cls.__name__
                        or single_cls.__name__.endswith("Impl"),
                        f"Single-implementation class "
                        f"'{single_cls.__module__}.{single_cls.__qualname__}' in "
                        f"package '{pkg_name}' uses a multi-implementation module "
                        "or class name. Consolidate it directly into '<name>.py' "
                        "as '<Name>'.",
                    )

        for group_label, impl_set in [
            *(
                (f"property '{name}'", impls)
                for name, impls in property_impls.items()
                if len(impls) >= 2
            ),
            *(
                (f"package '{pkg}'", impls)
                for pkg, impls in package_impls.items()
                if len({cls.__module__ for cls in impls}) >= 2
            ),
        ]:
            common_abcs = set.intersection(
                *(
                    {
                        base
                        for base in cls.__mro__
                        if inspect.isabstract(base)
                        and base not in generic_bases
                    }
                    for cls in impl_set
                )
            )
            self.assertTrue(
                common_abcs,
                f"Multiple concrete implementations {[c.__qualname__ for c in impl_set]} "
                f"found for {group_label}, but they do not share a common "
                "domain ABC. When 2+ implementations exist, define a '<Name>' "
                "ABC in '<name>.py' and place each concrete implementation in "
                "'<name>_using_<impl>.py' (or '<name>_impl.py') inheriting from "
                "'<Name>'.",
            )

    def test_all_packaged_modules_are_walkable(self) -> None:
        """Verifies pkgutil.walk_packages visits every packaged non-test module."""
        module_sources = _load_honeydew_module_sources()
        self.assertTrue(module_sources, "No Honeydew module sources found.")
        walked_modules = {
            "honeydew",
            *(
                module_name
                for _, module_name, _ in pkgutil.walk_packages(
                    honeydew.__path__, "honeydew."
                )
                if "tests" not in module_name.split(".")
            ),
        }
        missing_from_walk = tuple(sorted(set(module_sources) - walked_modules))
        self.assertSequenceEqual(
            missing_from_walk,
            (),
            "Packaged Honeydew modules were skipped by pkgutil.walk_packages "
            "(likely missing an __init__.py entry in "
            f"src/testing/end_to_end/honeydew/BUILD.gn): {missing_from_walk}.",
        )

    def test_no_mobly_imports(self) -> None:
        """Verifies Honeydew library modules do not import mobly directly."""
        module_sources = _load_honeydew_module_sources()
        self.assertTrue(module_sources, "No Honeydew module sources found.")
        imports_by_module = {
            module_name: _find_mobly_imports(source)
            for module_name, source in sorted(module_sources.items())
        }
        violations = tuple(
            (module_name, lineno, target)
            for module_name, imports in imports_by_module.items()
            for lineno, target in imports
            if target
            not in _MOBLY_IMPORT_ALLOWLIST.get(module_name, frozenset())
        )
        self.assertSequenceEqual(
            violations,
            (),
            "Honeydew library modules must not depend on mobly: "
            f"{violations}. Use native Python `assert`, standard Python "
            "exceptions, or Honeydew errors instead. If a mobly import is "
            "genuinely unavoidable (see the criteria on "
            "`_MOBLY_IMPORT_ALLOWLIST`), add the `(module, target)` entry to "
            "`_MOBLY_IMPORT_ALLOWLIST` in "
            "src/testing/end_to_end/honeydew/tests/unit_tests/architecture_test.py.",
        )

        stale_allowlist = tuple(
            sorted(
                (module_name, target)
                for module_name, allowed_targets in _MOBLY_IMPORT_ALLOWLIST.items()
                for target in (
                    allowed_targets
                    - {
                        imported_target
                        for _, imported_target in imports_by_module.get(
                            module_name, ()
                        )
                    }
                )
            )
        )
        self.assertSequenceEqual(
            stale_allowlist,
            (),
            "Entries in _MOBLY_IMPORT_ALLOWLIST are no longer imported and "
            f"should be removed: {stale_allowlist}.",
        )

    def test_find_mobly_imports(self) -> None:
        """Verifies _find_mobly_imports extracts mobly targets and ignores literals."""
        sample = textwrap.dedent(
            '''\
            r"""
            import mobly
            """
            # from mobly import signals
            EXAMPLE = """
            from mobly import signals
            """
            import mobly_utils
            from . import mobly
            from .mobly import signals
            import mobly
            import os, mobly.utils as mobly_utils
            from mobly.controllers import android_device as mobly_android_device


            def connect() -> None:
                from mobly import signals, asserts
            '''
        )
        self.assertSequenceEqual(
            _find_mobly_imports(sample),
            (
                (11, "mobly"),
                (12, "mobly.utils"),
                (13, "mobly.controllers.android_device"),
                (17, "mobly.asserts"),
                (17, "mobly.signals"),
            ),
        )


if __name__ == "__main__":
    unittest.main()
