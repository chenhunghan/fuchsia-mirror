# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from typing import Any

from pydantic import ValidationError
from shared.protocol.attach import AttachRequest
from shared.protocol.detach import DetachRequest
from shared.protocol.driver_list import (
    DriverHostEntry,
    DriverListRequest,
    DriverListResponse,
)
from shared.tests.helpers import ProtocolTestCase


class TestAttachRequestSchema(ProtocolTestCase):
    def test_valid_int_pid(self) -> None:
        req = AttachRequest(filter=1234)
        self.assertEqual(req.filter, 1234)
        self.assertEqual(AttachRequest.response_type, dict[str, Any])

    def test_valid_string_name(self) -> None:
        req = AttachRequest(filter="my_process")
        self.assertEqual(req.filter, "my_process")

    def test_missing_filter(self) -> None:
        with self.assertRaises(ValidationError):
            AttachRequest()

    def test_weak_and_exact_flags(self) -> None:
        req = AttachRequest(
            filter="xhci", weak=True, exact=True, recursive=True
        )
        self.assertTrue(req.weak)
        self.assertTrue(req.exact)
        self.assertTrue(req.recursive)

    def test_driver_and_node_flags(self) -> None:
        req_drv = AttachRequest(driver="xhci", weak=True, wait=True)
        self.assertEqual(req_drv.driver, "xhci")
        self.assertTrue(req_drv.weak)
        self.assertTrue(req_drv.wait)

        req_node = AttachRequest(node="dev.sys.platform.xhci")
        self.assertEqual(req_node.node, "dev.sys.platform.xhci")

        with self.assertRaises(ValidationError):
            AttachRequest(filter="xhci", driver="xhci")

        with self.assertRaises(ValidationError):
            AttachRequest(driver="xhci", node="dev.sys.platform.xhci")

        with self.assertRaises(ValidationError):
            AttachRequest(driver="xhci", exact=True)

        with self.assertRaises(ValidationError):
            AttachRequest(node="dev.sys.platform.xhci", exact=True)

        valid_exact = AttachRequest(filter="xhci", exact=True)
        self.assertTrue(valid_exact.exact)


class TestDriverListSchema(ProtocolTestCase):
    def test_driver_list_schema(self) -> None:
        req = DriverListRequest(filter="usb")
        self.assertEqual(req.command, "driver-list")
        self.assertEqual(req.filter, "usb")
        self.assertEqual(DriverListRequest.response_type, DriverListResponse)
        resp = DriverListResponse(
            driver_hosts=[
                DriverHostEntry(
                    host_koid=48291,
                    host_name="driver_host:pdev",
                    drivers=["fuchsia-boot:///xhci#meta/xhci.cm"],
                    devices=["dev.sys.platform.xhci"],
                    colocated=False,
                    transport_hazard=True,
                    transport_sensitive=["fuchsia-boot:///xhci#meta/xhci.cm"],
                )
            ]
        )
        self.assertEqual(resp.driver_hosts[0].host_koid, 48291)


class TestDetachRequestSchema(ProtocolTestCase):
    def test_valid_request_pid(self) -> None:
        req = DetachRequest(pid=1234)
        self.assertEqual(req.pid, 1234)
        self.assertFalse(req.all)
        self.assertEqual(DetachRequest.response_type, dict[str, Any])

    def test_valid_request_all(self) -> None:
        req = DetachRequest(all=True)
        self.assertIsNone(req.pid)
        self.assertTrue(req.all)

    def test_malformed_request_both(self) -> None:
        with self.assertRaises(ValidationError):
            DetachRequest(pid=1234, all=True)

    def test_malformed_request_neither(self) -> None:
        with self.assertRaises(ValidationError):
            DetachRequest()


if __name__ == "__main__":
    unittest.main()
