# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import collections
import os
import struct
from collections.abc import Mapping, Sequence

import fuchsia_base_test
import tp_shell
from mobly import asserts, test_runner
from trace_processing import trace_importing, trace_model, trace_utils

_ZX_OBJ_TYPE_PROCESS = 1
_ZX_OBJ_TYPE_THREAD = 2

# The process name is the one the ELF runner gives the helper component. The
# thread name and count must match the constants in set_name_helper.cc.
_HELPER_PROCESS_NAME = b"set_name_helper.cm"
_HELPER_THREAD_NAME = b"nul_padded_thread"
_HELPER_THREAD_COUNT = 8

_FXT_KERNEL_OBJECT_RECORD = 7
_FXT_LARGE_RECORD = 15


def _read_kernel_object_names(
    trace_path: str,
) -> Mapping[tuple[int, int], Sequence[bytes]]:
    """Reads the inline names of the kernel object records in an FXT file.

    Returns:
        The name bytes of each record, in file order, keyed by (object type,
        koid). Records that refer to their name by string table index are
        skipped; the kernel writes names inline.
    """
    with open(trace_path, "rb") as f:
        data = f.read()
    names: dict[tuple[int, int], list[bytes]] = collections.defaultdict(list)
    offset = 0
    while offset + 8 <= len(data):
        (header,) = struct.unpack_from("<Q", data, offset)
        record_type = header & 0xF
        if record_type == _FXT_LARGE_RECORD:
            size_words = (header >> 4) & 0xFFFFFFFF
        else:
            size_words = (header >> 4) & 0xFFF
        if size_words == 0:
            raise ValueError(f"zero-sized FXT record at offset {offset}")
        record_end = offset + size_words * 8
        if record_end > len(data):
            raise ValueError(
                f"FXT record at offset {offset} extends past end of file "
                f"({record_end} > {len(data)})"
            )
        if record_type == _FXT_KERNEL_OBJECT_RECORD:
            if size_words < 2:
                raise ValueError(
                    f"FXT kernel object record at offset {offset} too small "
                    f"({size_words} words)"
                )
            name_ref = (header >> 24) & 0xFFFF
            if name_ref & 0x8000:
                obj_type = (header >> 16) & 0xFF
                (koid,) = struct.unpack_from("<Q", data, offset + 8)
                name_start = offset + 16
                name_length = name_ref & 0x7FFF
                if name_start + name_length > record_end:
                    raise ValueError(
                        f"FXT kernel object name at offset {offset} extends "
                        f"past record end ({name_start + name_length} > "
                        f"{record_end})"
                    )
                names[(obj_type, koid)].append(
                    data[name_start : name_start + name_length]
                )
        offset = record_end
    if offset != len(data):
        raise ValueError(
            f"trailing {len(data) - offset} bytes at end of FXT file"
        )
    return {k: tuple(v) for k, v in names.items()}


class PerfTestTraceEventsTest(fuchsia_base_test.FuchsiaBaseTest):
    async def test_perftest_library_trace_events(self) -> None:
        async with self.dut.tracing.trace_session(
            categories=[
                "kernel",
                "perftest",
            ],
            buffer_size=36,
            download=True,
            directory=self.log_path,
            trace_file="trace.fxt",
        ):
            self.dut.ffx.run_test_component(
                "fuchsia-pkg://fuchsia.com/fuchsia_microbenchmarks#meta/fuchsia_microbenchmarks.cm",
                ffx_test_args=["--realm", "/core/testing/system-tests"],
                test_component_args=[
                    "-p",
                    "--quiet",
                    "--runs",
                    "4",
                    "--enable-tracing",
                    "--filter=^Null$",
                ],
                capture_output=False,
            )

        expected_event_names = [
            "test_group",
            "test_setup",
            "test_run",
            "test_run",
            "test_run",
            "test_run",
            "test_teardown",
        ]
        model = trace_importing.create_model_from_trace_file_path(
            os.path.join(self.log_path, "trace.fxt"),
            patterns=set(expected_event_names),
        )
        event_names = [
            event.name
            for event in trace_utils.filter_events(
                model.all_events(),
                category="perftest",
                type=trace_model.Event,
            )
        ]
        asserts.assert_equal(event_names, expected_event_names)

    async def test_process_and_thread_rename_with_nul_padded_buffer(
        self,
    ) -> None:
        """Tests that renaming a process and threads with a NUL-padded buffer
        via ZX_PROP_NAME while tracing is active records names without trailing
        NULs, and that trace_processor_shell matches those names exactly
        (b/585382990).
        """
        trace_file = "rename_trace.fxt"
        trace_path = os.path.join(self.log_path, trace_file)
        async with self.dut.tracing.trace_session(
            categories=[
                "kernel:meta",
            ],
            buffer_size=36,
            download=True,
            directory=self.log_path,
            trace_file=trace_file,
        ):
            self.dut.ffx.run_test_component(
                "fuchsia-pkg://fuchsia.com/set_name_helper#meta/set_name_helper.cm",
                capture_output=False,
            )

        # Kernel object records carry no timestamp, so a trace reader may apply
        # an object's creation record and its rename record in either order
        # (b/586604601). Check every record directly instead of relying on
        # which name a reader ends up with.
        names_by_object = _read_kernel_object_names(trace_path)
        for obj_type, name, expected_renamed in (
            (_ZX_OBJ_TYPE_PROCESS, _HELPER_PROCESS_NAME, 1),
            (_ZX_OBJ_TYPE_THREAD, _HELPER_THREAD_NAME, _HELPER_THREAD_COUNT),
        ):
            # Match on the name with NULs stripped so that a record with a
            # padded name is selected and then fails the comparison below.
            record_names = tuple(
                names
                for (record_type, _), names in sorted(names_by_object.items())
                if record_type == obj_type
                and any(n.rstrip(b"\0") == name for n in names)
            )
            for names in record_names:
                asserts.assert_equal(names, (name,) * len(names))
            # A renamed object has a creation record and a rename record.
            # Fewer objects than expected with two records means the rename
            # records are missing and the check above proved nothing.
            asserts.assert_equal(
                sum(1 for names in record_names if len(names) >= 2),
                expected_renamed,
            )

        # The helper renames each object to the name it already has, so the
        # name trace_processor_shell reports does not depend on record order.
        with tp_shell.PerfettoTraceProcessor(trace_path) as tp:
            processes = tp.run_query(
                "SELECT pid FROM process "
                f"WHERE name = '{_HELPER_PROCESS_NAME.decode()}'"
            )
            asserts.assert_greater(len(processes), 0)
            threads = tp.run_query(
                "SELECT tid FROM thread "
                f"WHERE name = '{_HELPER_THREAD_NAME.decode()}'"
            )
            asserts.assert_equal(len(threads), _HELPER_THREAD_COUNT)


if __name__ == "__main__":
    test_runner.main()
