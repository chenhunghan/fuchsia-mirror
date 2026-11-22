# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from shared.protocol import make_request
from shared.protocol.aspace import AspaceRequest, AspaceResponse
from shared.protocol.diagnose_hang import (
    DiagnoseHangRequest,
    DiagnoseHangResponse,
)
from shared.protocol.handles import HandlesRequest, HandlesResponse
from shared.protocol.mem_read import MemReadRequest, MemReadResponse
from shared.protocol.modules import ModulesRequest, ModulesResponse
from shared.tests.helpers import ProtocolTestCase


class TestProcessInspectionSchemas(ProtocolTestCase):
    def test_process_inspection_schemas(self) -> None:
        mod_req = ModulesRequest(pid=48291, filter="xhci", force_reload=True)
        self.assertEqual(mod_req.command, "modules")
        self.assertEqual(ModulesRequest.response_type, ModulesResponse)
        self.assertIsInstance(ModulesResponse(), ModulesResponse)

        as_req = AspaceRequest(pid=48291, address="0x1000", filter="mmio")
        self.assertEqual(as_req.command, "aspace")
        self.assertEqual(AspaceRequest.response_type, AspaceResponse)
        self.assertIsInstance(AspaceResponse(), AspaceResponse)

        h_req = HandlesRequest(pid=48291, type="interrupt", detail=True)
        self.assertEqual(h_req.command, "handles")
        self.assertEqual(HandlesRequest.response_type, HandlesResponse)
        self.assertIsInstance(HandlesResponse(), HandlesResponse)

        mr_req = MemReadRequest(address="0x7ffee1234000", size=32, pid=48291)
        self.assertEqual(mr_req.command, "mem-read")
        self.assertEqual(MemReadRequest.response_type, MemReadResponse)
        self.assertIsInstance(
            MemReadResponse(address="0x7ffee1234000", size=32, hex_bytes=""),
            MemReadResponse,
        )

    def test_diagnose_hang_schema(self) -> None:
        hang_req = make_request(
            {"command": "diagnose-hang", "pid": 48291, "auto_resume": False}
        )
        self.assertIsInstance(hang_req, DiagnoseHangRequest)
        assert isinstance(hang_req, DiagnoseHangRequest)
        self.assertEqual(hang_req.pid, 48291)
        self.assertFalse(hang_req.auto_resume)
        self.assertEqual(
            DiagnoseHangRequest.response_type, DiagnoseHangResponse
        )


if __name__ == "__main__":
    unittest.main()
