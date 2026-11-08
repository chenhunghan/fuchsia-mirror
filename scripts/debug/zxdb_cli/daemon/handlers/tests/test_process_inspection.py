# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from typing import Any
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from daemon.tests.helpers import DaemonTestCase
from shared.protocol.aspace import AspaceRequest, AspaceResponse
from shared.protocol.handles import HandlesRequest, HandlesResponse
from shared.protocol.mem_read import MemReadRequest, MemReadResponse
from shared.protocol.modules import ModulesRequest, ModulesResponse


class TestProcessInspectionHandlers(DaemonTestCase):
    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_m2_process_inspection_commands(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Verifies zero-pause process inspection handlers (`modules`, `aspace`, `handles`, `mem-read`)."""
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(48291, name="driver_host:pdev")

        executed_commands: list[str] = []

        async def console_side_effect(args: Any) -> Mock:
            cmd = args.command
            executed_commands.append(cmd)
            resp = Mock(success=True)
            if cmd == "process 1 libs":
                resp.body = Mock(
                    output=(
                        "  Load address  Name\n"
                        " 0x40050123000  libxhci.so\n"
                        " 0x40080200000  libdriver_runtime.so\n"
                    )
                )
            elif cmd == "process 2 libs":
                resp.body = Mock(
                    output=(
                        "  Load address  Name\n" " 0x50010000000  libother.so\n"
                    )
                )
            elif cmd == "process 1 sym-stat":
                resp.body = Mock(
                    output=(
                        "Symbol index status\n\n"
                        "Process 1 symbol status\n\n"
                        "  libxhci.so\n"
                        "    Base: 0x40050123000\n"
                        "    Build ID: 4a8f19c2\n"
                        "    Symbols loaded: Yes\n"
                        "    Symbol file: /build-id/4a/8f19c2.debug\n"
                        "    Source files indexed: 42\n"
                        "    Symbols indexed: 318\n\n"
                        "  libdriver_runtime.so\n"
                        "    Base: 0x40080200000\n"
                        "    Build ID: 99aabbcc\n"
                        "    Symbols loaded: No\n\n"
                        '  👉 Use "libs" to refresh the module list\n'
                    )
                )
            elif cmd == "process 2 sym-stat":
                resp.body = Mock(
                    output=(
                        "Symbol index status\n\n"
                        "Process 2 symbol status\n\n"
                        "  libother.so\n"
                        "    Base: 0x50010000000\n"
                        "    Build ID: deadbeef\n"
                        "    Symbols loaded: Yes\n"
                    )
                )
            elif cmd in (
                "process 1 aspace",
                "process 1 aspace -- 0x954e4000",
            ):
                resp.body = Mock(
                    output=(
                        "          Start              End  Prot   Size    "
                        " Koid  Offset  Cmt.Pgs  Name\n"
                        "      0x200000    0x80000000000  ---   128T      "
                        "                       proc-vmar\n"
                        "    0x954e4000       0x954e8000  rw-    16K    "
                        " 4242     0x0        4      mmio-xhci\n"
                        "\n"
                        "              Page size: 4096\n"
                        "     Total mapped bytes: 16384\n"
                        "  Total committed pages: 4 = 16384 bytes\n"
                    )
                )
            elif cmd == "process 2 aspace -- 0x50010000000 - 0x1000":
                resp.body = Mock(
                    output=(
                        "          Start              End  Prot   Size    "
                        " Koid  Offset  Cmt.Pgs  Name\n"
                        " 0x5000ffff000    0x50010000000  r--     4K    "
                        " 7777     0x0        1      other-vmo\n"
                    )
                )
            elif cmd == "process 1 handle -x":
                resp.body = Mock(
                    output=(
                        "  Handle  Type                        Koid\n"
                        "  0x1003  ZX_OBJ_TYPE_INTERRUPT       0x292e\n"
                        "  <none>  ZX_OBJ_TYPE_VMO             0x1092\n"
                        "  0x1007  <unknown (33)>              0x3000\n"
                    )
                )
            elif cmd == "process 2 handle -x":
                resp.body = Mock(
                    output=(
                        "  Handle  Type                        Koid\n"
                        "  0x2001  ZX_OBJ_TYPE_CHANNEL         0x8888\n"
                    )
                )
            elif cmd in (
                "process 1 handle -x 0x1003",
                "process 1 handle -k -x 10542",
            ):
                resp.body = Mock(
                    output=(
                        "                 Handle  0x1003\n"
                        "                   Type  ZX_OBJ_TYPE_INTERRUPT\n"
                        "                   Koid  0x292e\n"
                        "                 Rights  ZX_RIGHT_DUPLICATE\n"
                        "                         ZX_RIGHT_WAIT\n"
                        "           Related koid  0x0\n"
                    )
                )
            elif cmd == "process 1 handle -k -x 4242":
                resp.body = Mock(
                    output=(
                        "                 Handle  <none>\n"
                        "                   Type  ZX_OBJ_TYPE_VMO\n"
                        "                   Koid  0x1092\n"
                        "                   Name  mmio-xhci\n"
                        "      VMO size in bytes  0x4000\n"
                        "           Cache policy "
                        " ZX_CACHE_POLICY_UNCACHED_DEVICE\n"
                    )
                )
            elif cmd.startswith("process 1 mem-read --size=16 -- "):
                resp.body = Mock(
                    output=(
                        "0x7ffee1234000:  01 02 03 04 05 06 07 08-09 0a 0b 0c"
                        " 0d 0e 0f 10  |................\n"
                    )
                )
            elif cmd == "process 2 mem-read --size=8 -- $rsp - 8":
                resp.body = Mock(
                    output="0x7ffee9990000:  aa bb cc dd ee ff 00 11  |........\n"
                )
            else:
                resp.body = Mock(output="ok")
            return resp

        mock_dap_client.zxdb_console_command = AsyncMock(
            side_effect=console_side_effect
        )

        # 1. modules
        mod_resp = await daemon.registry.handle(
            "modules", ModulesRequest(filter="xhci")
        )
        self.assertTrue(mod_resp.success)
        assert isinstance(mod_resp.body, ModulesResponse)
        self.assertEqual(mod_resp.body.pid, 48291)
        self.assertEqual(len(mod_resp.body.modules), 1)
        m0 = mod_resp.body.modules[0]
        self.assertEqual(m0.name, "libxhci.so")
        self.assertEqual(m0.base, "0x40050123000")
        self.assertEqual(m0.build_id, "4a8f19c2")
        self.assertTrue(m0.symbols_loaded)
        self.assertEqual(m0.files_indexed, 42)
        self.assertEqual(m0.functions_indexed, 318)

        # 2. aspace
        as_resp = await daemon.registry.handle(
            "aspace", AspaceRequest(filter="mmio")
        )
        self.assertTrue(as_resp.success)
        assert isinstance(as_resp.body, AspaceResponse)
        self.assertEqual(as_resp.body.page_size, 4096)
        self.assertEqual(len(as_resp.body.regions), 1)
        r0 = as_resp.body.regions[0]
        self.assertEqual(r0.name, "mmio-xhci")
        self.assertEqual(r0.vmo_koid, 4242)
        self.assertEqual(r0.committed_pages, 4)

        # 3. handles with type filter and detail=True
        h_resp = await daemon.registry.handle(
            "handles", HandlesRequest(type="interrupt", detail=True)
        )
        self.assertTrue(h_resp.success)
        assert isinstance(h_resp.body, HandlesResponse)
        self.assertEqual(len(h_resp.body.handles), 1)
        h0 = h_resp.body.handles[0]
        self.assertEqual(h0.handle_value, "0x1003")
        self.assertEqual(h0.type, "ZX_OBJ_TYPE_INTERRUPT")
        self.assertEqual(h0.koid, 0x292E)
        self.assertEqual(h0.rights, ["ZX_RIGHT_DUPLICATE", "ZX_RIGHT_WAIT"])

        # 3b. handles with unknown type and mapped VMO enrichment via handle -k
        h_all_resp = await daemon.registry.handle(
            "handles", HandlesRequest(detail=True)
        )
        self.assertTrue(h_all_resp.success)
        assert isinstance(h_all_resp.body, HandlesResponse)
        self.assertEqual(len(h_all_resp.body.handles), 3)
        self.assertEqual(h_all_resp.body.handles[1].vmo_name, "mmio-xhci")
        self.assertEqual(
            h_all_resp.body.handles[1].cache_policy,
            "ZX_CACHE_POLICY_UNCACHED_DEVICE",
        )
        self.assertEqual(h_all_resp.body.handles[2].type, "<unknown (33)>")

        # 4. mem-read without pausing any thread
        mr_resp = await daemon.registry.handle(
            "mem-read", MemReadRequest(address="0x7ffee1234000", size=16)
        )
        self.assertTrue(mr_resp.success)
        assert isinstance(mr_resp.body, MemReadResponse)
        self.assertEqual(mr_resp.body.address, "0x7ffee1234000")
        self.assertEqual(mr_resp.body.size, 16)
        self.assertEqual(
            mr_resp.body.u32_le,
            ["0x04030201", "0x08070605", "0x0c0b0a09", "0x100f0e0d"],
        )
        self.assertEqual(
            mr_resp.body.u64_le,
            ["0x0807060504030201", "0x100f0e0d0c0b0a09"],
        )

        # 5. Multiple processes without --pid returns error
        daemon.get_or_create_process(99999, name="driver_host:other")
        for cmd_name, req_obj in (
            ("modules", ModulesRequest()),
            ("aspace", AspaceRequest()),
            ("handles", HandlesRequest()),
            ("mem-read", MemReadRequest(address="0x1000", size=16)),
        ):
            multi_resp = await daemon.registry.handle(cmd_name, req_obj)
            self.assertFalse(multi_resp.success)
            self.assertIn("Multiple processes active", multi_resp.message or "")

        # 6. Multi-process --pid selection scopes commands to `process 2`
        executed_commands.clear()
        mod2_resp = await daemon.registry.handle(
            "modules", ModulesRequest(pid=99999)
        )
        self.assertTrue(mod2_resp.success)
        assert isinstance(mod2_resp.body, ModulesResponse)
        self.assertEqual(mod2_resp.body.pid, 99999)
        self.assertEqual(len(mod2_resp.body.modules), 1)
        self.assertEqual(mod2_resp.body.modules[0].name, "libother.so")
        self.assertEqual(mod2_resp.body.modules[0].build_id, "deadbeef")

        as2_resp = await daemon.registry.handle(
            "aspace",
            AspaceRequest(pid=99999, address="0x50010000000 - 0x1000"),
        )
        self.assertTrue(as2_resp.success)
        assert isinstance(as2_resp.body, AspaceResponse)
        self.assertEqual(as2_resp.body.pid, 99999)
        self.assertEqual(len(as2_resp.body.regions), 1)
        self.assertEqual(as2_resp.body.regions[0].name, "other-vmo")

        h2_resp = await daemon.registry.handle(
            "handles", HandlesRequest(pid=99999)
        )
        self.assertTrue(h2_resp.success)
        assert isinstance(h2_resp.body, HandlesResponse)
        self.assertEqual(h2_resp.body.pid, 99999)
        self.assertEqual(len(h2_resp.body.handles), 1)
        self.assertEqual(h2_resp.body.handles[0].koid, 0x8888)

        mr2_resp = await daemon.registry.handle(
            "mem-read",
            MemReadRequest(pid=99999, address="$rsp - 8", size=8),
        )
        self.assertTrue(mr2_resp.success)
        assert isinstance(mr2_resp.body, MemReadResponse)
        self.assertEqual(mr2_resp.body.address, "0x7ffee9990000")
        self.assertEqual(
            executed_commands,
            [
                "process 2 libs",
                "process 2 sym-stat",
                "process 2 aspace -- 0x50010000000 - 0x1000",
                "process 2 handle -x",
                "process 2 mem-read --size=8 -- $rsp - 8",
            ],
        )

        # 7. Unknown --pid is rejected across all four commands
        for cmd_name, req_obj in (
            ("modules", ModulesRequest(pid=11111)),
            ("aspace", AspaceRequest(pid=11111)),
            ("handles", HandlesRequest(pid=11111)),
            ("mem-read", MemReadRequest(pid=11111, address="0x1000", size=16)),
        ):
            unknown_resp = await daemon.registry.handle(cmd_name, req_obj)
            self.assertFalse(unknown_resp.success)
            self.assertEqual(unknown_resp.message, "Process 11111 not found")

        # 8. Non-zero frame_index without thread_id in mem-read is rejected
        bad_frame_resp = await daemon.registry.handle(
            "mem-read",
            MemReadRequest(pid=48291, address="0x1000", size=16, frame_index=1),
        )
        self.assertFalse(bad_frame_resp.success)
        self.assertEqual(
            bad_frame_resp.message,
            "thread_id is required when specifying a non-zero frame_index",
        )


if __name__ == "__main__":
    unittest.main()
