# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for resource_usage.py."""

import enum
import gc
import sys
import types
import unittest
from types import MappingProxyType
from typing import Any
from unittest import mock

from memory import resource_usage
from parameterized import parameterized


def _sum_unique_sizeof(*objs: Any) -> int:
    """Returns the sum of sys.getsizeof(o) for the distinct objects in objs."""
    seen: set[int] = set()
    total = 0
    for o in objs:
        oid = id(o)
        if oid not in seen:
            seen.add(oid)
            total += sys.getsizeof(o)
    return total


class _SampleClass:
    pass


def _sample_func() -> None:
    pass


class _SlottedPoint:
    __slots__ = ("_ticks",)

    def __init__(self, ticks: int) -> None:
        self._ticks = ticks


class _SlottedRecord:
    __slots__ = ("start", "tid", "prio", "_args")

    def __init__(
        self,
        start: _SlottedPoint,
        tid: int,
        prio: int | None,
        args: MappingProxyType[str, Any] | None = None,
    ) -> None:
        self.start = start
        self.tid = tid
        self.prio = prio
        self._args = args


class _SlottedWithDict(_SlottedPoint):
    """Subclass of slotted class without __slots__, so it gets a __dict__."""

    def __init__(self, ticks: int, label: str) -> None:
        super().__init__(ticks)
        self.label = label


class _CustomInt(int):
    """Subclass of int that carries a __dict__ and extra attributes."""


class _CustomStr(str):
    """Subclass of str that carries a __dict__ and extra attributes."""


class _CustomTuple(tuple[Any, ...]):
    """Subclass of tuple that carries a __dict__ and extra attributes."""


class _SampleEnum(enum.Enum):
    ALPHA = 1
    BETA = 2


class _SampleIntEnum(enum.IntEnum):
    ONE = 1001
    TWO = 1002


class ResourceUsageTest(unittest.TestCase):
    # These tests verify get_deep_size() by constructing object graphs from
    # explicit constituent objects whose individual shallow sizes are directly
    # measurable via sys.getsizeof(), then asserting that get_deep_size()
    # matches the sum of sys.getsizeof() across those distinct constituents.
    @parameterized.expand(
        [
            ("builtin_type", int),
            ("user_class", _SampleClass),
            ("stdlib_module", sys),
            ("imported_module", unittest),
            ("function", _sample_func),
            ("lambda", lambda: 1),
        ]
    )
    def test_denylisted_root_raises_type_error(
        self, _: str, invalid: Any
    ) -> None:
        with self.assertRaises(TypeError):
            resource_usage.get_deep_size(invalid)

    @parameterized.expand(
        [
            ("none", None),
            ("true", True),
            ("false", False),
            ("zero_int", 0),
            ("small_int", 42),
            ("big_int", 10**50),
            ("float", 3.14159),
            ("complex", 1 + 2j),
            ("empty_str", ""),
            ("non_empty_str", "hello world" * 10),
            ("empty_bytes", b""),
            ("non_empty_bytes", b"bytes payload" * 10),
            ("bytearray", bytearray(b"mutable bytes")),
            ("range", range(0, 1000, 3)),
        ]
    )
    def test_atomic_leaf_primitive(self, _: str, val: Any) -> None:
        self.assertEqual(resource_usage.get_deep_size(val), sys.getsizeof(val))

    def test_slice_and_memoryview_referents(self) -> None:
        # slice holds references to (start, stop, step).
        start, stop, step = 1001, 2002, 3003
        s = slice(start, stop, step)
        self.assertEqual(
            resource_usage.get_deep_size(s),
            _sum_unique_sizeof(s, start, stop, step),
        )

        # memoryview holds a reference to a managedbuffer wrapping the underlying buffer.
        buf = b"abcde" * 10
        mv = memoryview(buf)
        managed_buf = gc.get_referents(mv)[0]
        self.assertEqual(
            resource_usage.get_deep_size(mv),
            _sum_unique_sizeof(mv, managed_buf, buf),
        )

    def test_primitive_subclasses_with_attributes(self) -> None:
        # Note: CPython's dict_traverse skips str keys on unicode-only dicts
        # (including __dict__), visiting only the dict values.
        ci = _CustomInt(999999)
        v_int = 1234567
        inner_dict = {"k": v_int}
        leaf_str = "nested_list"
        extra_list = [leaf_str, inner_dict]
        ci.extra = extra_list  # type: ignore[attr-defined]
        self.assertEqual(
            resource_usage.get_deep_size(ci),
            _sum_unique_sizeof(
                ci,
                ci.__dict__,
                extra_list,
                leaf_str,
                inner_dict,
                v_int,
            ),
        )

        # str subclasses use CPython 3.11+ inline values (Py_TPFLAGS_MANAGED_DICT),
        # so cs.__dict__ is not materialized unless explicitly accessed.
        cs = _CustomStr("custom_str_value")
        meta_int = 987654
        meta_str = "tag"
        meta_tuple = (meta_int, meta_str)
        cs.meta = meta_tuple  # type: ignore[attr-defined]
        self.assertEqual(
            resource_usage.get_deep_size(cs),
            _sum_unique_sizeof(cs, meta_tuple, meta_int, meta_str),
        )

        el1, el2 = 10101, 20202
        ct = _CustomTuple([el1, el2])
        tag_v = "value"
        tag_dict = {"nested": tag_v}
        ct.tag = tag_dict  # type: ignore[attr-defined]
        self.assertEqual(
            resource_usage.get_deep_size(ct),
            _sum_unique_sizeof(ct, el1, el2, ct.__dict__, tag_dict, tag_v),
        )

        # Also verify primitive subclasses when traversed as inner referents
        # inside a container (exercising the inner-loop _ATOMIC_TYPES check).
        subclasses = [ci, cs, ct]
        self.assertEqual(
            resource_usage.get_deep_size(subclasses),
            _sum_unique_sizeof(
                subclasses,
                ci,
                ci.__dict__,
                extra_list,
                leaf_str,
                inner_dict,
                v_int,
                cs,
                meta_tuple,
                meta_int,
                meta_str,
                ct,
                el1,
                el2,
                ct.__dict__,
                tag_dict,
                tag_v,
            ),
        )

    def test_enums(self) -> None:
        # Standard Enum members use inline instance values (_value_, _name_,
        # _sort_order_), whereas IntEnum (subclassing int) materializes __dict__.
        alpha = _SampleEnum.ALPHA
        self.assertEqual(
            resource_usage.get_deep_size(alpha),
            _sum_unique_sizeof(
                alpha,
                alpha._value_,
                alpha._name_,
                alpha._sort_order_,  # type: ignore[attr-defined]
            ),
        )

        one = _SampleIntEnum.ONE
        self.assertEqual(
            resource_usage.get_deep_size(one),
            _sum_unique_sizeof(
                one,
                one.__dict__,
                one._value_,
                one._name_,
                one._sort_order_,  # type: ignore[attr-defined]
            ),
        )

        # Duplicate references to the same Enum / IntEnum member inside a
        # container are traversed and counted once.
        for member in (_SampleEnum.ALPHA, _SampleIntEnum.ONE):
            pair = [member, member]
            self.assertEqual(
                resource_usage.get_deep_size(pair),
                sys.getsizeof(pair) + resource_usage.get_deep_size(member),
            )

    def test_diamond_dag_and_shared_leaves_counted_once(self) -> None:
        shared_leaf = "shared_string_" * 20
        shared_int = 10**30 + 7
        shared_sublist = [shared_leaf, shared_int]
        branch_a = {"a": shared_sublist, "leaf": shared_leaf, "num": shared_int}
        branch_b = {"b": shared_sublist, "leaf": shared_leaf, "num": shared_int}
        pair_tuple = (shared_leaf, shared_int)
        root = [branch_a, branch_b, pair_tuple]

        expected = _sum_unique_sizeof(
            root,
            branch_a,
            branch_b,
            pair_tuple,
            shared_sublist,
            shared_leaf,
            shared_int,
        )
        self.assertEqual(resource_usage.get_deep_size(root), expected)

        # Adding another reference to already-visited objects only adds outer container slot size.
        root_with_dup = [branch_a, branch_b, pair_tuple, shared_sublist]
        self.assertEqual(
            resource_usage.get_deep_size(root_with_dup) - expected,
            sys.getsizeof(root_with_dup) - sys.getsizeof(root),
        )

    def test_reference_cycles(self) -> None:
        # Direct self-cycle.
        self_list: list[Any] = []
        self_list.append(self_list)
        self.assertEqual(
            resource_usage.get_deep_size(self_list), sys.getsizeof(self_list)
        )

        # Mutual cycle across dict and list.
        val_str = "a"
        node_a: dict[str, Any] = {"name": val_str}
        node_b: list[Any] = [node_a]
        node_a["partner"] = node_b
        self.assertEqual(
            resource_usage.get_deep_size(node_a),
            _sum_unique_sizeof(node_a, node_b, val_str),
        )

    def test_slotted_objects_and_mapping_proxy(self) -> None:
        empty_backing: dict[str, Any] = {}
        shared_empty: MappingProxyType[str, Any] = MappingProxyType(
            empty_backing
        )
        pt1 = _SlottedPoint(100_000_000_001)
        rec1 = _SlottedRecord(pt1, tid=5001, prio=20, args=shared_empty)

        non_empty_backing: dict[str, Any] = {"cpu": 3, "reason": "preempted"}
        non_empty_proxy: MappingProxyType[str, Any] = MappingProxyType(
            non_empty_backing
        )
        pt2 = _SlottedPoint(100_000_000_002)
        rec2 = _SlottedRecord(pt2, tid=5001, prio=20, args=non_empty_proxy)

        hybrid = _SlottedWithDict(100_000_000_003, "hybrid_node")
        rec_list = [rec1, rec2]
        hybrid.dynamic_field = rec_list  # type: ignore[attr-defined]

        tuple_val = (hybrid, shared_empty)
        container = {0: rec_list, 1: tuple_val}

        self.assertEqual(
            resource_usage.get_deep_size(container),
            _sum_unique_sizeof(
                container,
                0,
                1,
                rec_list,
                tuple_val,
                rec1,
                pt1,
                pt1._ticks,
                rec1.tid,
                rec1.prio,
                shared_empty,
                empty_backing,
                rec2,
                pt2,
                pt2._ticks,
                rec2.tid,
                rec2.prio,
                non_empty_proxy,
                non_empty_backing,
                *non_empty_backing.values(),
                hybrid,
                hybrid._ticks,
                hybrid.label,
            ),
        )

    def test_normal_classes_inline_and_materialized_dict(self) -> None:
        class NormalObj:
            def __init__(self, a: int, b: str, c: list[Any]) -> None:
                self.a = a
                self.b = b
                self.c = c

        # Without accessing __dict__ (uses CPython 3.11+ inline values).
        c_inline = [101, 202, 303]
        inline_obj = NormalObj(123456789, "inline_str", c_inline)
        self.assertEqual(
            resource_usage.get_deep_size(inline_obj),
            _sum_unique_sizeof(
                inline_obj, inline_obj.a, inline_obj.b, c_inline, *c_inline
            ),
        )

        # With materialized __dict__, the dict object itself is also a referent.
        c_mat = [404, 505, 606]
        mat_obj = NormalObj(987654321, "mat_str", c_mat)
        mat_dict = mat_obj.__dict__
        self.assertEqual(
            resource_usage.get_deep_size(mat_obj),
            _sum_unique_sizeof(
                mat_obj,
                mat_dict,
                mat_obj.a,
                mat_obj.b,
                c_mat,
                *c_mat,
            ),
        )

    def test_denylisted_referents_are_ignored(self) -> None:
        class InnerClass:
            pass

        def helper_fn() -> int:
            return 7

        data_list = [1001, 2002, "valid_string"]
        container = {
            "data": data_list,
            "cls": InnerClass,
            "fn": helper_fn,
            "mod": types,
        }
        self.assertEqual(
            resource_usage.get_deep_size(container),
            _sum_unique_sizeof(container, data_list, *data_list),
        )

    def test_deep_chain_no_recursion_error(self) -> None:
        # Build a linked chain of 5,000 nodes (> sys.getrecursionlimit()).
        tail = "tail_leaf"
        head: Any = tail
        nodes: list[Any] = [tail]
        for i in range(5000):
            idx = 10_000 + i
            inner_list = [head]
            head = (idx, inner_list)
            nodes.extend((idx, inner_list, head))
        self.assertEqual(
            resource_usage.get_deep_size(head),
            _sum_unique_sizeof(*nodes),
        )

    def test_multi_page_bitmap_and_shared_singletons(self) -> None:
        # Allocate enough objects (~100k objects > 4+ MiB) to span multiple 1 MiB
        # address-space pages alongside distant pre-existing singletons.
        records: list[_SlottedRecord] = []
        constituents: list[Any] = []
        for i in range(25_000):
            ticks = 10**12 + i
            pt = _SlottedPoint(ticks)
            tid = i & 0xFF
            prio = 20
            rec = _SlottedRecord(pt, tid=tid, prio=prio, args=None)
            records.append(rec)
            constituents.extend((ticks, pt, tid, prio, None, rec))
        dup_slice = records[:1000]
        singletons: list[Any] = [None, True, False, 0, 1, 255, "", "sys"]
        graph = {
            "records": records,
            "duplicate_slice": dup_slice,
            "singletons": singletons,
        }
        constituents.extend(
            (
                graph,
                records,
                dup_slice,
                singletons,
                *singletons,
            )
        )
        page_keys = {id(r) >> 20 for r in records} | {
            id(s) >> 20 for s in singletons
        }
        self.assertGreater(len(page_keys), 2)
        self.assertEqual(
            resource_usage.get_deep_size(graph),
            _sum_unique_sizeof(*constituents),
        )

    @parameterized.expand(
        [(f"page_{page_num:#x}", page_num) for page_num in (0, 1, 0x7FFF_1234)]
    )
    def test_seen_ids_bitmap_aligned_addresses_and_bit_preservation(
        self, _: str, page_num: int
    ) -> None:
        # Verify 16-byte-aligned slots across the same byte (slots 0, 1, 7 in
        # byte 0; slots 65534, 65535 in byte 8191), adjacent bytes (7, 8, 9),
        # and multiple pages within a single _SeenIdsBitmap instance so setting
        # new bits preserves previously set bits in the same byte.
        bitmap = resource_usage._SeenIdsBitmap()
        other_page = 0x7FFF_1234 if page_num != 0x7FFF_1234 else 1
        slots = (0, 1, 7, 8, 9, 65534, 65535)
        addrs = [(page_num << 20) | (slot << 4) for slot in slots] + [
            (other_page << 20) | (0 << 4),
            (other_page << 20) | (65535 << 4),
        ]

        for addr in addrs:
            self.assertTrue(bitmap.check_and_add(addr))

        # Re-check all addresses after sibling bits in the same bytes/pages
        # have been populated to verify `byte_val | mask` bit preservation.
        for addr in addrs:
            self.assertFalse(bitmap.check_and_add(addr))

        # Unvisited neighboring slots in the same bytes remain unset.
        for unvisited_slot in (2, 6, 10, 65533):
            self.assertTrue(
                bitmap.check_and_add((page_num << 20) | (unvisited_slot << 4))
            )
        self.assertEqual(len(bitmap.unaligned_ids), 0)
        self.assertEqual(set(bitmap.pages.keys()), {page_num, other_page})

    @parameterized.expand(
        [(f"offset_{offset}", offset) for offset in (1, 2, 4, 8, 15)]
    )
    def test_seen_ids_bitmap_unaligned_addresses(
        self, _: str, offset: int
    ) -> None:
        # Verify non-16-byte-aligned addresses use `unaligned_ids` without
        # colliding with 16-byte-aligned addresses sharing the same base slot.
        bitmap = resource_usage._SeenIdsBitmap()
        base_addr = (0x7FFF_1234 << 20) | (0x10 << 4)
        unaligned_addr = base_addr | offset

        self.assertTrue(bitmap.check_and_add(unaligned_addr))
        self.assertFalse(bitmap.check_and_add(unaligned_addr))
        self.assertEqual(bitmap.unaligned_ids, {unaligned_addr})
        self.assertEqual(len(bitmap.pages), 0)

        # The aligned base slot is still independent and unmarked.
        self.assertTrue(bitmap.check_and_add(base_addr))
        self.assertFalse(bitmap.check_and_add(base_addr))

    def test_denylisted_referents_marked_as_seen(self) -> None:
        captured: list[resource_usage._SeenIdsBitmap] = []
        orig_cls = resource_usage._SeenIdsBitmap

        def make_bitmap() -> resource_usage._SeenIdsBitmap:
            inst = orig_cls()
            captured.append(inst)
            return inst

        root: list[Any] = [_SampleClass, types, _sample_func, "payload"]
        with mock.patch.object(
            resource_usage, "_SeenIdsBitmap", side_effect=make_bitmap
        ):
            size = resource_usage.get_deep_size(root)

        self.assertEqual(size, _sum_unique_sizeof(root, "payload"))
        self.assertEqual(len(captured), 1)
        seen = captured[0]
        for denylisted in (_SampleClass, types, _sample_func):
            self.assertFalse(seen.check_and_add(id(denylisted)))

    def test_get_max_rss_bytes(self) -> None:
        rss = resource_usage.get_max_rss_bytes()
        self.assertGreater(rss.self_bytes, 0)
        self.assertGreaterEqual(rss.children_bytes, 0)


if __name__ == "__main__":
    unittest.main()
