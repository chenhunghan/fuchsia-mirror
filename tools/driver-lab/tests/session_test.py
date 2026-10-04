# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import tempfile
import unittest
from pathlib import Path
from typing import Any

from driver_lab.api import DriverLab, DriverLabError
from driver_lab.consent import ConsentDecision
from driver_lab.models import AccessRequest, ResourceKind
from driver_lab.session import (
    AccessRequirements,
    Gpio,
    HardwareSession,
    I2c,
    Interrupt,
    MmioRegion,
    SessionCapabilities,
    Spi,
    StateHandle,
    TranslationMetadata,
    UnsupportedCapabilityError,
)
from driver_lab.transport import (
    Denial,
    DirectDescription,
    FakeDirectTarget,
    FakeProxyTarget,
    OpenRejection,
    OpenSessionRejected,
    OperationDenied,
    ProxyDescription,
    ResourceInfo,
    SequenceItem,
    SessionMode,
)

CTRL_DIGEST = "sha256:" + "11" * 32
POLICY_DIGEST = "sha256:" + "22" * 32


class ScriptedPrompt:
    def __init__(self, decision: ConsentDecision) -> None:
        self.decision = decision

    async def request_consent(
        self, request: AccessRequest, warning: str
    ) -> ConsentDecision:
        return self.decision


def make_proxy_description() -> ProxyDescription:
    return ProxyDescription(
        protocol_major=1,
        protocol_minor=0,
        proxy_generation=1,
        boot_id="boot-test-1",
        resource_digest="sha256:" + "33" * 32,
        policy_digest=POLICY_DIGEST,
        resources=(
            ResourceInfo(
                id=1,
                name="control",
                logical_size=0x100,
                digest=CTRL_DIGEST,
            ),
            ResourceInfo(
                id=2,
                name="status",
                logical_size=0x80,
                digest="sha256:" + "44" * 32,
            ),
        ),
        max_snapshot_items=64,
        audit_capacity=256,
        node_moniker="sample/hardware/node",
    )


def make_extended_proxy_description() -> ProxyDescription:
    return ProxyDescription(
        protocol_major=1,
        protocol_minor=0,
        proxy_generation=1,
        boot_id="boot-test-1",
        resource_digest="sha256:" + "33" * 32,
        policy_digest=POLICY_DIGEST,
        resources=(
            ResourceInfo(
                id=1,
                name="control",
                logical_size=0x100,
                digest=CTRL_DIGEST,
            ),
            ResourceInfo(
                id=2,
                name="status",
                logical_size=0x80,
                digest="sha256:" + "44" * 32,
            ),
            ResourceInfo(
                id=3,
                name="pin0",
                kind=ResourceKind.GPIO,
                logical_size=1,
                digest="sha256:" + "55" * 32,
            ),
            ResourceInfo(
                id=4,
                name="i2c-bus",
                kind=ResourceKind.I2C,
                logical_size=32,
                digest="sha256:" + "66" * 32,
            ),
            ResourceInfo(
                id=5,
                name="spi-bus",
                kind=ResourceKind.SPI,
                logical_size=32,
                digest="sha256:" + "77" * 32,
            ),
            ResourceInfo(
                id=6,
                name="irq0",
                kind=ResourceKind.INTERRUPT,
                logical_size=0,
                digest="sha256:" + "88" * 32,
            ),
            ResourceInfo(
                id=7,
                name="state0",
                kind=ResourceKind.MMIO,
                logical_size=0x20,
                digest="sha256:" + "99" * 32,
            ),
        ),
        max_snapshot_items=64,
        audit_capacity=256,
        node_moniker="sample/hardware/node",
    )


class SessionTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        base = Path(self._dir.name)
        self.grants_path = base / "grants.toml"
        self.evidence_root = base / "evidence"

        self.fake_proxy = FakeProxyTarget(make_proxy_description())
        self.fake_direct = FakeDirectTarget(
            node_id="sample-node",
            protocol_name="fuchsia.hardware.gpio/Device",
            bound_driver_url="fuchsia-boot:///sample#meta/sample.cm",
        )

    async def test_proxy_mmio_read_and_write(self) -> None:
        self.fake_proxy.set_value(1, 0x10, 0xCAFE_BABE)
        lab = DriverLab(
            self.fake_proxy,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
        )

        prompt = ScriptedPrompt(ConsentDecision.ALLOW_ONCE)
        async with await lab.attach(
            "node-1",
            mode="proxy",
            session_mode=SessionMode.MUTATING,
            consent=prompt,
        ) as session:
            self.assertEqual(session.mode, "proxy")
            self.assertTrue(session.capabilities.target_policy)
            self.assertTrue(session.capabilities.target_audit)
            self.assertTrue(session.capabilities.target_local_timing)
            self.assertFalse(session.capabilities.production_driver_active)

            mmio = await session.mmio("control")
            self.assertEqual(mmio.name, "control")
            self.assertEqual(mmio.id, 1)
            self.assertEqual(mmio.logical_size, 0x100)

            # Read32
            val = await mmio.read32(0x10)
            self.assertEqual(val, 0xCAFE_BABE)

            # Write32 with readback
            write_res = await mmio.write32(0x14, 0x1234_5678)
            self.assertEqual(write_res.readback_value, 0x1234_5678)
            self.assertEqual(self.fake_proxy.get_value(1, 0x14), 0x1234_5678)

            # Write32 with precondition
            write_res2 = await mmio.write32(
                0x14, 0x8765_4321, expected_before=0x1234_5678
            )
            self.assertEqual(write_res2.readback_value, 0x8765_4321)
            self.assertEqual(self.fake_proxy.get_value(1, 0x14), 0x8765_4321)

            # Precondition failure
            with self.assertRaises(OperationDenied) as ctx:
                await mmio.write32(
                    0x14, 0xAAAA_BBBB, expected_before=0xDEAD_BEEF
                )
            self.assertEqual(ctx.exception.denial, Denial.PRECONDITION_FAILED)

    async def test_proxy_mmio_poll_and_snapshot(self) -> None:
        self.fake_proxy.set_value(1, 0x20, 0x0)
        lab = DriverLab(
            self.fake_proxy,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
        )

        async with await lab.attach("node-1", mode="proxy") as session:
            mmio = await session.mmio("control")

            # Poll matching value
            self.fake_proxy.set_value(1, 0x20, 0x1)
            poll_res = await mmio.poll32(0x20, expected=0x1, mask=0x1)
            self.assertEqual(poll_res.value, 0x1)

            # Poll timeout
            self.fake_proxy.set_value(1, 0x24, 0x0)
            with self.assertRaises(OperationDenied) as ctx:
                await mmio.poll32(0x24, expected=0x1, mask=0x1, timeout_s=0.001)
            self.assertEqual(ctx.exception.denial, Denial.TIMEOUT)

            # Snapshot32
            self.fake_proxy.set_value(1, 0x0, 0x1111)
            self.fake_proxy.set_value(1, 0x4, 0x2222)
            values = await mmio.snapshot32([0x0, 0x4])
            self.assertEqual(values, [0x1111, 0x2222])

    async def test_mmio_bounds_and_alignment_validation(self) -> None:
        lab = DriverLab(
            self.fake_proxy,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
        )
        async with await lab.attach("node-1", mode="proxy") as session:
            mmio = await session.mmio("control")

            with self.assertRaises(ValueError):
                await mmio.read32(-4)

            with self.assertRaises(ValueError):
                await mmio.read32(
                    0x100
                )  # Out of bounds (logical_size is 0x100)

            with self.assertRaises(ValueError):
                await mmio.read32(0x2)  # Misaligned

            with self.assertRaises(ValueError):
                await mmio.write32(0x100, 0x1)

            with self.assertRaises(ValueError):
                await mmio.poll32(0x1, expected=0)

            with self.assertRaises(ValueError):
                await session.mmio("nonexistent_resource")

    async def test_translation_metadata_present(self) -> None:
        self.assertIsInstance(MmioRegion.read32_metadata, TranslationMetadata)
        self.assertTrue(MmioRegion.read32_metadata.directly_translatable)
        self.assertIn("Read32", MmioRegion.read32_metadata.cpp_analogue)

        self.assertIsInstance(MmioRegion.write32_metadata, TranslationMetadata)
        self.assertTrue(MmioRegion.write32_metadata.directly_translatable)

        self.assertIsInstance(MmioRegion.poll32_metadata, TranslationMetadata)
        self.assertTrue(MmioRegion.poll32_metadata.target_local_timing)

        self.assertIsInstance(
            MmioRegion.snapshot32_metadata, TranslationMetadata
        )
        self.assertTrue(MmioRegion.snapshot32_metadata.experiment_only)

        # Protocol adapter translation metadata
        self.assertIsInstance(Gpio.read_metadata, TranslationMetadata)
        self.assertTrue(Gpio.read_metadata.directly_translatable)
        self.assertIsInstance(Gpio.write_metadata, TranslationMetadata)
        self.assertTrue(Gpio.write_metadata.directly_translatable)

        self.assertIsInstance(I2c.transfer_metadata, TranslationMetadata)
        self.assertTrue(I2c.transfer_metadata.directly_translatable)

        self.assertIsInstance(Spi.transmit_metadata, TranslationMetadata)
        self.assertTrue(Spi.transmit_metadata.directly_translatable)

        self.assertIsInstance(Interrupt.wait_metadata, TranslationMetadata)
        self.assertTrue(Interrupt.wait_metadata.directly_translatable)

    async def test_direct_mode_capabilities_and_protocols(self) -> None:
        # Register handlers on fake direct target
        gpio_state = {"value": True, "direction": "output"}

        def handle_read(args: Any) -> dict[str, Any]:
            return {"value": gpio_state["value"]}

        def handle_write(args: Any) -> dict[str, Any]:
            gpio_state["value"] = args.get("value", False)
            return {}

        self.fake_direct.register_method("read", handle_read)
        self.fake_direct.register_method("write", handle_write)

        lab = DriverLab(
            self.fake_direct,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
        )

        async with await lab.attach("node-1", mode="direct") as session:
            self.assertEqual(session.mode, "direct")
            self.assertFalse(session.capabilities.target_policy)
            self.assertFalse(session.capabilities.target_audit)
            self.assertFalse(session.capabilities.target_local_timing)
            self.assertTrue(session.capabilities.production_driver_active)

            # Direct published protocol
            proto = await session.protocol("fuchsia.hardware.gpio/Device")
            res = await proto.read(resource="gpio")
            self.assertTrue(res.response["value"])

            # Gpio adapter
            gpio = await session.gpio("gpio")
            self.assertTrue(await gpio.read())
            await gpio.write(False)
            self.assertFalse(await gpio.read())

            # Attempting MMIO or target sequence in direct mode fails closed
            with self.assertRaises(UnsupportedCapabilityError):
                await session.mmio("control")

            with self.assertRaises(UnsupportedCapabilityError):
                await session.sequence([])

    async def test_proxy_mode_rejects_direct_protocols(self) -> None:
        lab = DriverLab(
            self.fake_proxy,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
        )
        async with await lab.attach("node-1", mode="proxy") as session:
            with self.assertRaises(UnsupportedCapabilityError):
                await session.protocol("fuchsia.hardware.gpio/Device")

            with self.assertRaises(UnsupportedCapabilityError):
                await session.serial("serial")

            with self.assertRaises(UnsupportedCapabilityError):
                await session.clock("clock")

            with self.assertRaises(UnsupportedCapabilityError):
                await session.reset("reset")

            # Gpio request on proxy that has no GPIO resource raises ValueError
            with self.assertRaises(ValueError):
                await session.gpio("gpio")

            # Interrupt request on proxy that has no interrupt resource raises ValueError
            with self.assertRaises(ValueError):
                await session.interrupt("interrupt")

    async def test_proxy_mode_protocol_adapters(self) -> None:
        fake = FakeProxyTarget(make_extended_proxy_description())
        fake.set_i2c_response(4, b"\xde\xad")
        fake.set_spi_response(5, b"\xbe\xef")

        lab = DriverLab(
            fake,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
        )
        prompt = ScriptedPrompt(ConsentDecision.ALLOW_ONCE)
        async with await lab.attach(
            "node-1",
            mode="proxy",
            session_mode=SessionMode.MUTATING,
            consent=prompt,
        ) as session:
            # GPIO adapter in proxy mode
            gpio = await session.gpio("pin0")
            self.assertEqual(gpio.name, "pin0")
            self.assertEqual(gpio.id, 3)
            await gpio.write(True)
            self.assertTrue(await gpio.read())
            self.assertTrue(fake.get_gpio(3))
            await gpio.write(False)
            self.assertFalse(await gpio.read())
            self.assertFalse(fake.get_gpio(3))

            # I2C adapter in proxy mode
            i2c = await session.i2c("i2c-bus")
            self.assertEqual(i2c.name, "i2c-bus")
            self.assertEqual(i2c.id, 4)
            rx_i2c = await i2c.transfer(b"\x01\x02", read_length=2)
            self.assertEqual(rx_i2c, b"\xde\xad")

            # SPI adapter in proxy mode
            spi = await session.spi("spi-bus")
            self.assertEqual(spi.name, "spi-bus")
            self.assertEqual(spi.id, 5)
            rx_spi = await spi.transmit(b"\x03\x04")
            self.assertEqual(rx_spi, b"\xbe\xef")

            # Interrupt adapter in proxy mode
            irq = await session.interrupt("irq0")
            self.assertEqual(irq.name, "irq0")
            self.assertEqual(irq.id, 6)

            # Immediate wait when after_sequence < sequence
            fake.trigger_interrupt(6, timestamp_ns=500_000)
            irq_res = await irq.wait(after_sequence=0)
            self.assertEqual(irq_res.resource, 6)
            self.assertEqual(irq_res.sequence, 1)
            self.assertEqual(irq_res.count, 1)
            self.assertEqual(irq_res.timestamp_ns, 500_000)
            self.assertEqual(irq_res.coalesced_count, 1)

            # Asynchronous wait satisfied by trigger_interrupt
            wait_task = asyncio.create_task(
                irq.wait(after_sequence=1, timeout_s=1.0)
            )
            await asyncio.sleep(0)
            fake.trigger_interrupt(6, timestamp_ns=600_000)
            irq_res2 = await wait_task
            self.assertEqual(irq_res2.resource, 6)
            self.assertEqual(irq_res2.sequence, 2)
            self.assertEqual(irq_res2.count, 2)
            self.assertEqual(irq_res2.timestamp_ns, 600_000)
            self.assertEqual(irq_res2.coalesced_count, 0)

    async def test_heterogeneous_sequence_proxy_mode(self) -> None:
        fake = FakeProxyTarget(make_extended_proxy_description())
        fake.set_value(1, 0x10, 0x1111_2222)
        fake.set_i2c_response(4, b"\xca\xfe")
        fake.set_spi_response(5, b"\xba\xbe")

        lab = DriverLab(
            fake,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
        )
        prompt = ScriptedPrompt(ConsentDecision.ALLOW_ONCE)
        async with await lab.attach(
            "node-1",
            mode="proxy",
            session_mode=SessionMode.MUTATING,
            consent=prompt,
        ) as session:
            seq_items = [
                SequenceItem(kind="read32", resource=1, offset=0x10),
                SequenceItem(
                    kind="write32",
                    resource=1,
                    offset=0x14,
                    value=0x9999,
                    readback=True,
                ),
                SequenceItem(kind="delay_ns", delay_ns=1000),
                SequenceItem(kind="barrier", barrier="memory"),
                SequenceItem(kind="gpio_write", resource=3, value=True),
                SequenceItem(kind="gpio_read", resource=3),
                SequenceItem(
                    kind="i2c_transfer",
                    resource=4,
                    write_data=b"\x01",
                    read_length=2,
                ),
                SequenceItem(kind="spi_transmit", resource=5, tx_data=b"\x02"),
            ]
            outcome = await session.sequence(seq_items)
            self.assertTrue(outcome.complete)
            self.assertEqual(len(outcome.results), 8)
            self.assertEqual(outcome.results[0].kind, "read32")
            self.assertEqual(outcome.results[0].value, 0x1111_2222)
            self.assertEqual(outcome.results[1].kind, "write32")
            self.assertEqual(outcome.results[1].readback_value, 0x9999)
            self.assertEqual(outcome.results[2].kind, "delay_ns")
            self.assertEqual(outcome.results[3].kind, "barrier")
            self.assertEqual(outcome.results[4].kind, "gpio_write")
            self.assertEqual(outcome.results[5].kind, "gpio_read")
            self.assertEqual(outcome.results[5].value, 1)
            self.assertEqual(outcome.results[6].kind, "i2c_transfer")
            self.assertEqual(outcome.results[6].data, b"\xca\xfe")
            self.assertEqual(outcome.results[7].kind, "spi_transmit")
            self.assertEqual(outcome.results[7].data, b"\xba\xbe")

    async def test_selection_rules_in_attach(self) -> None:
        lab_proxy = DriverLab(
            self.fake_proxy,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
        )
        # auto with needs_mmio selects proxy
        session = await lab_proxy.attach(
            "node-1",
            mode="auto",
            requirements=AccessRequirements(needs_mmio=True),
        )
        self.assertEqual(session.mode, "proxy")
        await session.close()

        # direct mode with needs_mmio raises DriverLabError
        with self.assertRaises(DriverLabError):
            await lab_proxy.attach(
                "node-1",
                mode="direct",
                requirements=AccessRequirements(needs_mmio=True),
            )

        # Invalid mode string
        with self.assertRaises(ValueError):
            await lab_proxy.attach("node-1", mode="invalid")  # type: ignore[arg-type]

    async def test_mutating_session_consent_enforcement(self) -> None:
        # Consent denied
        prompt_denied = ScriptedPrompt(ConsentDecision.DENY_ONCE)
        lab_denied = DriverLab(
            self.fake_proxy,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
            consent=prompt_denied,
        )
        with self.assertRaises(DriverLabError):
            await lab_denied.attach(
                "node-1", mode="proxy", session_mode=SessionMode.MUTATING
            )

        # No consent prompt configured for mutating session
        lab_unattended = DriverLab(
            self.fake_proxy,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
            consent=None,
        )
        with self.assertRaises(DriverLabError):
            await lab_unattended.attach(
                "node-1", mode="proxy", session_mode=SessionMode.MUTATING
            )

    async def test_async_cancellation_cleans_up_session(self) -> None:
        prompt = ScriptedPrompt(ConsentDecision.ALLOW_ONCE)
        lab = DriverLab(
            self.fake_proxy,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
            consent=prompt,
        )

        session = await lab.attach(
            "node-1", mode="proxy", session_mode=SessionMode.MUTATING
        )
        self.assertEqual(self.fake_proxy.active_mutating_session, 1)

        async def worker() -> None:
            async with session:
                mmio = await session.mmio("control")
                await mmio.read32(0x10)
                await asyncio.sleep(10.0)

        task = asyncio.create_task(worker())
        await asyncio.sleep(0.01)
        task.cancel()

        with self.assertRaises(asyncio.CancelledError):
            await task

        self.assertTrue(session.is_closed)
        self.assertIsNone(self.fake_proxy.active_mutating_session)

        # Calling on closed session fails
        with self.assertRaises(UnsupportedCapabilityError):
            await session.mmio("control")

    async def test_representative_protocols_direct_mode(self) -> None:
        target = FakeDirectTarget(node_id="dev", protocol_name="multi")
        target.register_method("transfer", lambda a: {"read_data": [1, 2, 3]})
        target.register_method("transmit", lambda a: {"rx_data": [4, 5, 6]})
        target.register_method("write", lambda a: {"bytes_written": 2})
        target.register_method("read", lambda a: {"data": [7, 8]})
        target.register_method("enable", lambda a: {})
        target.register_method("disable", lambda a: {})
        target.register_method("assert_reset", lambda a: {})
        target.register_method("deassert_reset", lambda a: {})
        target.register_method("wait", lambda a: {"timestamp_ns": 12345})

        caps = SessionCapabilities(
            mode="direct",
            target_policy=False,
            target_audit=False,
            target_local_timing=False,
            fault_isolation="driver_host",
            production_driver_active=True,
        )
        desc = DirectDescription(
            node_id="dev", protocol_name="multi", boot_id="b1"
        )
        fake_session = await target.open_direct_session(None)  # type: ignore[arg-type]

        session = HardwareSession(
            capabilities=caps,
            direct_session=fake_session,
            direct_description=desc,
        )

        i2c = await session.i2c("i2c")
        rx_i2c = await i2c.transfer(b"abc", read_length=3)
        self.assertEqual(rx_i2c, bytes([1, 2, 3]))

        spi = await session.spi("spi")
        rx_spi = await spi.transmit(b"xyz")
        self.assertEqual(rx_spi, bytes([4, 5, 6]))

        serial = await session.serial("serial")
        written = await serial.write(b"hi")
        self.assertEqual(written, 2)
        rx_serial = await serial.read(2)
        self.assertEqual(rx_serial, bytes([7, 8]))

        clock = await session.clock("clock")
        await clock.enable()
        await clock.disable()

        reset = await session.reset("reset")
        await reset.assert_reset()
        await reset.deassert_reset()

        irq = await session.interrupt("irq")
        ts = await irq.wait(1.0)
        self.assertEqual(ts, 12345)

        await session.close()

    async def test_disabled_target_rejected(self) -> None:
        self.fake_proxy.enabled = False
        lab = DriverLab(
            self.fake_proxy,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
        )
        with self.assertRaises(OpenSessionRejected) as cm:
            await lab.attach("node-1", mode="proxy")
        self.assertEqual(cm.exception.reason, OpenRejection.NOT_ACCEPTING)

    async def test_rate_limit_exceeded(self) -> None:
        self.fake_proxy.max_ops_per_second = 2
        self.fake_proxy.set_value(1, 0x10, 42)
        lab = DriverLab(
            self.fake_proxy,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
        )
        async with await lab.attach("node-1", mode="proxy") as session:
            mmio = await session.mmio("control")
            val1 = await mmio.read32(0x10)
            self.assertEqual(val1, 42)
            val2 = await mmio.read32(0x10)
            self.assertEqual(val2, 42)
            with self.assertRaises(OperationDenied) as cm:
                await mmio.read32(0x10)
            self.assertEqual(cm.exception.denial, Denial.LIMIT_EXCEEDED)

    async def test_deadline_exceeded(self) -> None:
        self.fake_proxy.max_deadline_ns = 50_000_000
        self.fake_proxy.set_value(1, 0x10, 42)
        lab = DriverLab(
            self.fake_proxy,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
        )
        async with await lab.attach("node-1", mode="proxy") as session:
            mmio = await session.mmio("control")
            with self.assertRaises(OperationDenied) as cm:
                await mmio.poll32(0x10, expected=42, timeout_s=0.1)
            self.assertEqual(cm.exception.denial, Denial.LIMIT_EXCEEDED)

    async def test_proxy_state_handle_read_knob_trigger_poll_snapshot(
        self,
    ) -> None:
        ext_proxy = FakeProxyTarget(make_extended_proxy_description())
        ext_proxy.set_value(7, 0x00, 0)  # STATE_BUSY_FLAG
        ext_proxy.set_value(7, 0x04, 0)  # STATE_INVARIANT_VIOLATIONS
        ext_proxy.set_value(7, 0x08, 0)  # KNOB_RACE_DELAY_US
        ext_proxy.set_value(7, 0x0C, 0)  # KNOB_FIX_ENABLED
        ext_proxy.set_value(7, 0x10, 0)  # TRIGGER_CONCURRENT_OP

        def on_trigger(arg: int) -> None:
            delay_us = ext_proxy.get_value(7, 0x08)
            fix_enabled = ext_proxy.get_value(7, 0x0C)
            if delay_us > 0 and fix_enabled == 0:
                violations = ext_proxy.get_value(7, 0x04)
                ext_proxy.set_value(7, 0x04, violations + max(arg, 1))
            # Trigger slot always reads as 0.
            ext_proxy.set_value(7, 0x10, 0)

        ext_proxy.set_write_hook(7, 0x10, on_trigger)

        prompt = ScriptedPrompt(ConsentDecision.ALLOW_ONCE)
        lab = DriverLab(
            ext_proxy,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
            consent=prompt,
        )

        async with await lab.attach(
            "node-1",
            mode="proxy",
            session_mode=SessionMode.MUTATING,
        ) as session:
            state = await session.state("state0")
            self.assertIsInstance(state, StateHandle)
            self.assertEqual(state.name, "state0")
            self.assertEqual(state.id, 7)

            # 1. Repro bug: widen race window via knob and fire trigger.
            knob_res = await state.write_knob(0x08, 5000)
            self.assertEqual(knob_res.readback_value, 5000)
            trig_res = await state.trigger(0x10, arg=1)
            self.assertIsNone(trig_res.readback_value)
            self.assertEqual(await state.read32(0x04), 1)

            # 2. Prove fix in-situ: enable fix knob, fire trigger, verify counter stays 1.
            await state.write_knob(0x0C, 1)
            await state.trigger(0x10, arg=2)
            poll_res = await state.poll32(0x04, expected=1)
            self.assertEqual(poll_res.value, 1)

            snap = await state.snapshot32([0x04, 0x08, 0x0C])
            self.assertEqual(snap, [1, 5000, 1])


if __name__ == "__main__":
    unittest.main()
