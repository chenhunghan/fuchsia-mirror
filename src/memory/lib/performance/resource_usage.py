# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import gc
import resource
import sys
from types import FunctionType, ModuleType
from typing import Any, NamedTuple


class MaxRss(NamedTuple):
    self_bytes: int
    children_bytes: int


# Referents of Function objects include the modules they're in, which isn't
# really the kind of "size" we're trying to get at here. Module and type objects
# have similar issues.
_DENYLIST = type, ModuleType, FunctionType

# Built-in leaf types that never hold outgoing Python object references in
# CPython. Checked with exact type identity (`type(obj) in _ATOMIC_TYPES`) so
# user subclasses (e.g. IntEnum or custom int/str subclasses with __dict__)
# still have their referents traversed.
_ATOMIC_TYPES = frozenset(
    {
        int,
        str,
        float,
        bytes,
        bool,
        type(None),
        complex,
        bytearray,
        range,
    }
)

_BIT_MASKS = (1, 2, 4, 8, 16, 32, 64, 128)

# 64-bit CPython (pymalloc) aligns heap object addresses to 16 bytes (2**4).
# Bitmap pages are internal 8 KiB (2**13 byte) radix chunks (independent of the
# OS virtual memory page size), so each page tracks 2**16 aligned slots = 1 MiB
# of address space.
_ALIGN_SHIFT = 4
_ALIGN_MASK = (1 << _ALIGN_SHIFT) - 1
_BITS_PER_BYTE_SHIFT = 3
_BIT_INDEX_MASK = (1 << _BITS_PER_BYTE_SHIFT) - 1
_BYTE_SHIFT = _ALIGN_SHIFT + _BITS_PER_BYTE_SHIFT
_BITMAP_PAGE_SHIFT = 13
_BITMAP_PAGE_BYTES = 1 << _BITMAP_PAGE_SHIFT
_BITMAP_BYTE_MASK = _BITMAP_PAGE_BYTES - 1
_BITMAP_PAGE_KEY_SHIFT = _BYTE_SHIFT + _BITMAP_PAGE_SHIFT


class _SeenIdsBitmap:
    """Compact two-level sparse page-table bitmap for tracking visited `id(obj)`.

    In 64-bit CPython (pymalloc), object addresses (`oid = id(obj)`) are 16-byte
    aligned (`oid & _ALIGN_MASK == 0`). Each 16-byte slot `oid >> _ALIGN_SHIFT`
    maps to 1 bit in a `_BITMAP_PAGE_BYTES` (8 KiB) `bytearray` chunk indexed by
    `page_key = oid >> _BITMAP_PAGE_KEY_SHIFT` (covering `2**16` slots = 1 MiB
    of address space per chunk). Any address that is not 16-byte aligned falls
    back to `unaligned_ids`.
    """

    __slots__ = ("pages", "unaligned_ids")

    def __init__(self) -> None:
        self.pages: dict[int, bytearray] = {}
        self.unaligned_ids: set[int] = set()

    def check_and_add(self, oid: int) -> bool:
        """Marks `oid` as seen. Returns True if `oid` was newly added."""
        if oid & _ALIGN_MASK:
            if oid in self.unaligned_ids:
                return False
            self.unaligned_ids.add(oid)
            return True
        page_key = oid >> _BITMAP_PAGE_KEY_SHIFT
        page = self.pages.get(page_key)
        if page is None:
            page = bytearray(_BITMAP_PAGE_BYTES)
            self.pages[page_key] = page
        byte_idx = (oid >> _BYTE_SHIFT) & _BITMAP_BYTE_MASK
        mask = _BIT_MASKS[(oid >> _ALIGN_SHIFT) & _BIT_INDEX_MASK]
        byte_val = page[byte_idx]
        if byte_val & mask:
            return False
        page[byte_idx] = byte_val | mask
        return True


def get_deep_size(obj: Any) -> int:
    """Traverse obj and its members, summing their sizes."""
    if isinstance(obj, _DENYLIST):
        raise TypeError(
            f"get_deep_size() does not take argument of type: {str(type(obj))}"
        )
    if type(obj) in _ATOMIC_TYPES:
        return sys.getsizeof(obj)

    seen = _SeenIdsBitmap()
    seen.check_and_add(id(obj))
    size = sys.getsizeof(obj)
    stack: list[Any] = [obj]
    while stack:
        current = stack.pop()
        for ref in gc.get_referents(current):
            if seen.check_and_add(id(ref)):
                if type(ref) in _ATOMIC_TYPES:
                    size += sys.getsizeof(ref)
                elif not isinstance(ref, _DENYLIST):
                    size += sys.getsizeof(ref)
                    stack.append(ref)
    return size


def get_max_rss_bytes() -> MaxRss:
    """Returns the maximum resident set size of the current process tree in bytes."""
    return MaxRss(
        self_bytes=_get_current_process_max_rss_kb() * 1024,
        children_bytes=_get_child_processes_max_rss_kb() * 1024,
    )


def _get_current_process_max_rss_kb() -> int:
    """Returns the maximum resident set size of the current process in kilobytes."""
    return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss


def _get_child_processes_max_rss_kb() -> int:
    """
    Returns the maximum resident set size of all terminated child processes
    in kilobytes.
    """
    return resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
