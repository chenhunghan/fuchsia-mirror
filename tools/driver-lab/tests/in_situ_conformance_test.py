# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Phase 2 end-to-end in-situ conformance test suite.

Exercises the Phase 2 embedded driver library contract (`fuchsia.driver.lab`)
over real FIDL channel pairs (`FidlProxyTransport` + `fdl.ProxyServer` /
`fdl.SessionServer`), `DriverLab`, and the `driver-lab` CLI against a
synthetic `lab_sample_driver` target model:
1. Discovery of active debug-capable drivers (`fuchsia.driver.lab.Service`).
2. In-situ `describe` / `inspect` without unbinding the active driver.
3. Safe concurrent read-only MMIO observation while the driver is running.
4. Target ceiling enforcement (`hard_denied` FIFO register and `writable` mask).
5. Cooperative quiesce interlock (`quiesce_engaged` / `quiesce_released` and
   `STATUS_QUIESCED` bit) on `Mutating` session open and fail-safe disconnect.
6. Non-intrusive interrupt observation via ISR event tapping.
7. Full `run_plan` in-situ execution and evidence bundle provenance.
"""

import asyncio
import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

import fidl_fuchsia_driver_lab as fdl
from driver_lab import cli
from driver_lab.api import DriverLab
from driver_lab.discovery import (
    DRIVER_LAB_SERVICE,
    FakeNodeDiscovery,
    FakeProxyActivator,
    NodeDescription,
)
from driver_lab.fidl_transport import FidlProxyTransport
from driver_lab.models import AccessClass, Decision, ReadGrant, ResourceKind
from driver_lab.permissions import save_grants
from driver_lab.transport import (
    AllowRule,
    Expectations,
    FakeProxyTarget,
    OpenRejection,
    OpenSessionRejected,
    ProxyDescription,
    ProxySession,
    ResourceInfo,
    SessionContext,
    SessionMode,
    SnapshotItem,
)
from fidl import DomainError
from fidl._ipc import GlobalHandleWaker
from fuchsia_controller_py import Channel, Context

SAMPLE_MONIKER = "dev.sys.platform.sample-device"
SAMPLE_DRIVER_URL = (
    "fuchsia-pkg://fuchsia.com/driver-lab-testing#meta/lab_sample_driver.cm"
)
TARGET_SCOPE = "sample-engineering-target"
MMIO_DIGEST = "sha256:" + "11" * 32
IRQ_DIGEST = "sha256:" + "22" * 32
STATE_DIGEST = "sha256:" + "55" * 32
RESOURCE_DIGEST = "sha256:" + "33" * 32
POLICY_DIGEST = "sha256:" + "44" * 32

# Register layout matching src/devices/driver-lab/testing/src/sample_driver.rs
REG_DEVICE_ID = 0x00
REG_STATUS = 0x04
REG_CONTROL = 0x08
REG_SCRATCH = 0x10
REG_FIFO_DATA = 0x40

# Software state / knob / trigger layout matching sample_driver.rs
STATE_BUSY_FLAG = 0x00
STATE_INVARIANT_VIOLATIONS = 0x04
KNOB_RACE_DELAY_US = 0x08
KNOB_FIX_ENABLED = 0x0C
TRIGGER_CONCURRENT_OP = 0x10

DEVICE_ID_VALUE = 0x5341_4D50  # "SAMP"
STATUS_RUNNING = 1 << 0
STATUS_QUIESCED = 1 << 4

_RESOURCE_KIND_TO_FIDL = {
    ResourceKind.MMIO: fdl.ResourceKind.MMIO,
    ResourceKind.GPIO: fdl.ResourceKind.GPIO,
    ResourceKind.I2C: fdl.ResourceKind.I2_C,
    ResourceKind.SPI: fdl.ResourceKind.SPI,
    ResourceKind.INTERRUPT: fdl.ResourceKind.INTERRUPT,
    ResourceKind.CLOCK: fdl.ResourceKind.CLOCK,
    ResourceKind.RESET: fdl.ResourceKind.RESET,
    ResourceKind.SERIAL: fdl.ResourceKind.SERIAL,
}


class SyntheticSampleDriverTarget:
    """Models `lab_sample_driver` with embedded `driver_lab_lib::LabInstance`."""

    def __init__(self) -> None:
        self.description = ProxyDescription(
            protocol_major=1,
            protocol_minor=0,
            proxy_generation=1,
            boot_id="boot-sample-1",
            resource_digest=RESOURCE_DIGEST,
            policy_digest=POLICY_DIGEST,
            resources=(
                ResourceInfo(
                    id=1,
                    name="mmio0",
                    kind=ResourceKind.MMIO,
                    logical_size=0x1000,
                    digest=MMIO_DIGEST,
                ),
                ResourceInfo(
                    id=2,
                    name="irq0",
                    kind=ResourceKind.INTERRUPT,
                    logical_size=0x10,
                    digest=IRQ_DIGEST,
                ),
                ResourceInfo(
                    id=3,
                    name="state0",
                    kind=ResourceKind.MMIO,
                    logical_size=0x14,
                    digest=STATE_DIGEST,
                ),
            ),
            max_snapshot_items=64,
            audit_capacity=256,
        )
        self.fake = FakeProxyTarget(self.description)
        self.fake.set_value(1, REG_DEVICE_ID, DEVICE_ID_VALUE)
        self.fake.set_value(1, REG_STATUS, STATUS_RUNNING)
        self.fake.set_value(1, REG_CONTROL, 0x0000_0001)
        self.fake.set_value(1, REG_SCRATCH, 0x1234_5678)
        self.fake.set_value(1, REG_FIFO_DATA, 0xDEAD_BEEF)

        # Target-side ceilings matching sample_driver.rs:
        # - 0x40..0x44 (REG_FIFO_DATA) is hard-denied (read-sensitive pop).
        # - Only REG_CONTROL (0x08) and REG_SCRATCH (0x10) are writable.
        self.fake.set_hard_denied_ranges(
            1, [(REG_FIFO_DATA, REG_FIFO_DATA + 4)]
        )
        self.fake.set_writable_registers(1, [REG_CONTROL, REG_SCRATCH])
        self.fake.set_quiesce_hook(self._on_quiesce)

        # Software state bank ("state0", resource 3) matching sample_driver.rs:
        self.fake.set_value(3, STATE_BUSY_FLAG, 0)
        self.fake.set_value(3, STATE_INVARIANT_VIOLATIONS, 0)
        self.fake.set_value(3, KNOB_RACE_DELAY_US, 0)
        self.fake.set_value(3, KNOB_FIX_ENABLED, 0)
        self.fake.set_value(3, TRIGGER_CONCURRENT_OP, 0)
        self.fake.set_writable_registers(
            3, [KNOB_RACE_DELAY_US, KNOB_FIX_ENABLED, TRIGGER_CONCURRENT_OP]
        )
        self.fake.set_write_hook(
            3, TRIGGER_CONCURRENT_OP, self._on_trigger_concurrent_op
        )

        self.driver_isr_count = 0

    @property
    def is_quiesced(self) -> bool:
        return self.fake.is_quiesced

    def _on_quiesce(self, quiesced: bool) -> None:
        status = self.fake.get_value(1, REG_STATUS)
        if quiesced:
            status |= STATUS_QUIESCED
        else:
            status &= ~STATUS_QUIESCED
        self.fake.set_value(1, REG_STATUS, status)

    def _on_trigger_concurrent_op(self, arg: int) -> None:
        self.fake.set_value(3, STATE_BUSY_FLAG, 1)
        delay_us = self.fake.get_value(3, KNOB_RACE_DELAY_US)
        fix_enabled = self.fake.get_value(3, KNOB_FIX_ENABLED)
        if delay_us > 0 and fix_enabled == 0:
            violations = self.fake.get_value(3, STATE_INVARIANT_VIOLATIONS)
            self.fake.set_value(
                3, STATE_INVARIANT_VIOLATIONS, violations + max(arg, 1)
            )
        self.fake.set_value(3, STATE_BUSY_FLAG, 0)
        # Trigger slot always reads as 0.
        self.fake.set_value(3, TRIGGER_CONCURRENT_OP, 0)

    def trigger_hardware_irq(self) -> None:
        """Simulates hardware firing irq0: driver ISR runs and taps LabInstance."""
        self.driver_isr_count += 1
        self.fake.trigger_interrupt(2, timestamp_ns=50_000)


class _InSituSessionServer(fdl.SessionServer):
    """Serves one `Session` channel backed by the synthetic sample driver."""

    def __init__(self, channel: Channel, session: ProxySession) -> None:
        super().__init__(channel)
        self._session = session

    async def read32(self, request: Any) -> Any:
        try:
            outcome = await self._session.read32(
                request.resource, request.offset
            )
        except Exception as error:
            denial = getattr(error, "denial", None)
            if denial is None:
                raise
            return DomainError(error=getattr(fdl.OperationError, denial.name))
        return fdl.ReadResult(
            value=outcome.value,
            audit_seq=outcome.audit_seq,
            timestamp_ns=outcome.timestamp_ns,
        )

    async def snapshot(self, request: Any) -> Any:
        items = [
            SnapshotItem(resource=item.resource, offset=item.offset)
            for item in request.items
        ]
        try:
            outcome = await self._session.snapshot(items)
        except Exception as error:
            denial = getattr(error, "denial", None)
            if denial is None:
                raise
            return DomainError(error=getattr(fdl.OperationError, denial.name))
        return fdl.SnapshotResult(
            results=[
                fdl.SnapshotItemResult(
                    item=fdl.SnapshotItem(
                        resource=result.item.resource, offset=result.item.offset
                    ),
                    ok=result.ok,
                    value=result.value,
                    audit_seq=result.audit_seq,
                    timestamp_ns=result.timestamp_ns,
                )
                for result in outcome.results
            ],
            complete=outcome.complete,
        )

    async def write32(self, request: Any) -> Any:
        precondition = None
        if request.precondition is not None:
            precondition = (
                request.precondition.expected,
                request.precondition.mask,
            )
        try:
            outcome = await self._session.write32(
                request.resource,
                request.offset,
                request.value,
                write_mask=request.write_mask,
                precondition=precondition,
                readback=request.readback,
            )
        except Exception as error:
            denial = getattr(error, "denial", None)
            if denial is None:
                raise
            return DomainError(error=getattr(fdl.OperationError, denial.name))
        return fdl.WriteResult(
            readback_value=outcome.readback_value or 0,
            audit_seq=outcome.audit_seq,
            timestamp_ns=outcome.timestamp_ns,
        )

    async def poll32(self, request: Any) -> Any:
        try:
            outcome = await self._session.poll32(
                request.resource,
                request.offset,
                request.expected,
                mask=request.mask,
                interval_ns=request.interval_ns,
                timeout_ns=request.timeout_ns,
            )
        except Exception as error:
            denial = getattr(error, "denial", None)
            if denial is None:
                raise
            return DomainError(error=getattr(fdl.OperationError, denial.name))
        return fdl.PollResult(
            value=outcome.value,
            audit_seq=outcome.audit_seq,
            timestamp_ns=outcome.timestamp_ns,
        )

    async def execute_sequence(self, request: Any) -> Any:
        from driver_lab.transport import SequenceItem

        seq_items: list[SequenceItem] = []
        for item in request.items:
            if item.read32 is not None:
                seq_items.append(
                    SequenceItem(
                        kind="read32",
                        resource=item.read32.resource,
                        offset=item.read32.offset,
                    )
                )
            elif item.write32 is not None:
                precond = None
                if item.write32.precondition is not None:
                    precond = (
                        item.write32.precondition.expected,
                        item.write32.precondition.mask,
                    )
                seq_items.append(
                    SequenceItem(
                        kind="write32",
                        resource=item.write32.resource,
                        offset=item.write32.offset,
                        value=item.write32.value,
                        write_mask=item.write32.write_mask,
                        precondition=precond,
                        readback=item.write32.readback,
                    )
                )
            elif item.poll32 is not None:
                seq_items.append(
                    SequenceItem(
                        kind="poll32",
                        resource=item.poll32.resource,
                        offset=item.poll32.offset,
                        expected=item.poll32.expected,
                        mask=item.poll32.mask,
                        interval_ns=item.poll32.interval_ns,
                        timeout_ns=item.poll32.timeout_ns,
                    )
                )
            elif item.delay_ns is not None:
                seq_items.append(
                    SequenceItem(kind="delay_ns", delay_ns=item.delay_ns)
                )
            elif item.barrier is not None:
                seq_items.append(SequenceItem(kind="barrier"))
        try:
            outcome = await self._session.execute_sequence(seq_items)
        except Exception as error:
            denial = getattr(error, "denial", None)
            if denial is None:
                raise
            return DomainError(error=getattr(fdl.OperationError, denial.name))
        fidl_results = []
        for res in outcome.results:
            if not res.ok and res.error is not None:
                item_outcome = fdl.SequenceItemOutcome(
                    error=getattr(fdl.OperationError, res.error.name)
                )
            elif res.kind == "read32":
                item_outcome = fdl.SequenceItemOutcome(
                    read32=fdl.ReadResult(
                        value=res.value or 0,
                        audit_seq=res.audit_seq,
                        timestamp_ns=res.timestamp_ns,
                    )
                )
            elif res.kind == "write32":
                item_outcome = fdl.SequenceItemOutcome(
                    write32=fdl.WriteResult(
                        readback_value=res.readback_value or 0,
                        audit_seq=res.audit_seq,
                        timestamp_ns=res.timestamp_ns,
                    )
                )
            elif res.kind == "poll32":
                item_outcome = fdl.SequenceItemOutcome(
                    poll32=fdl.PollResult(
                        value=res.value or 0,
                        audit_seq=res.audit_seq,
                        timestamp_ns=res.timestamp_ns,
                    )
                )
            elif res.kind == "delay_ns":
                item_outcome = fdl.SequenceItemOutcome(delay_ns=fdl.DelayNs())
            else:
                item_outcome = fdl.SequenceItemOutcome(barrier=fdl.Barrier())
            fidl_results.append(
                fdl.SequenceItemResult(
                    index=res.index,
                    ok=res.ok,
                    outcome=item_outcome,
                )
            )
        return fdl.SequenceResult(
            results=fidl_results, complete=outcome.complete
        )

    async def gpio_read(self, request: Any) -> Any:
        return fdl.GpioReadResult(value=False, audit_seq=1, timestamp_ns=100)

    async def gpio_write(self, request: Any) -> Any:
        return fdl.GpioWriteResult(audit_seq=1, timestamp_ns=100)

    async def i2c_transfer(self, request: Any) -> Any:
        return fdl.I2cTransferResult(
            read_data=[], audit_seq=1, timestamp_ns=100
        )

    async def spi_transmit(self, request: Any) -> Any:
        return fdl.SpiTransmitResult(rx_data=[], audit_seq=1, timestamp_ns=100)

    async def open_gpio(self, request: Any) -> Any:
        return fdl.SessionOpenGpioResponse()

    async def open_i2c(self, request: Any) -> Any:
        return fdl.SessionOpenI2cResponse()

    async def open_spi(self, request: Any) -> Any:
        return fdl.SessionOpenSpiResponse()

    async def clock_enable(self, request: Any) -> Any:
        return fdl.ClockEnableResult(audit_seq=1, timestamp_ns=100)

    async def clock_disable(self, request: Any) -> Any:
        return fdl.ClockDisableResult(audit_seq=1, timestamp_ns=100)

    async def clock_is_enabled(self, request: Any) -> Any:
        return fdl.ClockIsEnabledResult(
            enabled=True, audit_seq=1, timestamp_ns=100
        )

    async def clock_set_rate(self, request: Any) -> Any:
        return fdl.ClockSetRateResult(audit_seq=1, timestamp_ns=100)

    async def clock_query_rate(self, request: Any) -> Any:
        return fdl.ClockQueryRateResult(
            hz_out=request.hz_in, audit_seq=1, timestamp_ns=100
        )

    async def clock_get_rate(self, request: Any) -> Any:
        return fdl.ClockGetRateResult(
            hz=24000000, audit_seq=1, timestamp_ns=100
        )

    async def reset_assert(self, request: Any) -> Any:
        return fdl.ResetAssertResult(audit_seq=1, timestamp_ns=100)

    async def reset_deassert(self, request: Any) -> Any:
        return fdl.ResetDeassertResult(audit_seq=1, timestamp_ns=100)

    async def reset_toggle(self, request: Any) -> Any:
        return fdl.ResetToggleResult(audit_seq=1, timestamp_ns=100)

    async def reset_status(self, request: Any) -> Any:
        return fdl.ResetStatusResult(
            asserted=False, audit_seq=1, timestamp_ns=100
        )

    async def serial_read(self, request: Any) -> Any:
        return fdl.SerialReadResult(data=[], audit_seq=1, timestamp_ns=100)

    async def serial_write(self, request: Any) -> Any:
        return fdl.SerialWriteResult(audit_seq=1, timestamp_ns=100)

    async def wait_for_interrupt(self, request: Any) -> Any:
        try:
            outcome = await self._session.wait_for_interrupt(
                request.resource,
                after_sequence=request.after_sequence,
                timeout_s=request.timeout_ns / 1_000_000_000,
            )
        except Exception as error:
            denial = getattr(error, "denial", None)
            if denial is None:
                raise
            return DomainError(error=getattr(fdl.OperationError, denial.name))
        return fdl.InterruptResult(
            resource=outcome.resource,
            sequence=outcome.sequence,
            count=outcome.count,
            timestamp_ns=outcome.timestamp_ns,
            coalesced_count=outcome.coalesced_count,
        )

    async def read_audit(self, request: Any) -> Any:
        page = await self._session.read_audit(request.cursor, request.limit)
        return fdl.AuditPage(
            entries=[
                fdl.AuditEntry(
                    seq=entry.seq,
                    session=entry.session,
                    resource=entry.resource,
                    operation=entry.operation,
                    offset=entry.offset,
                    decision=getattr(fdl.AuditDecision, entry.decision.upper()),
                    denial=(
                        getattr(fdl.OperationError, entry.denial.upper())
                        if entry.denial
                        else None
                    ),
                    status=getattr(fdl.AuditStatus, entry.status.upper()),
                    value=entry.value,
                    timestamp_ns=entry.timestamp_ns,
                    run_id=entry.run_id,
                    item_index=entry.item_index,
                    proxy_generation=entry.proxy_generation,
                    boot_id=entry.boot_id,
                )
                for entry in page.entries
            ],
            has_oldest=page.oldest_retained is not None,
            oldest_retained=page.oldest_retained or 0,
            next_cursor=page.next_cursor,
        )


class _LifecycleBridgeProxyServer(fdl.ProxyServer):
    """Bridge proxy server that closes the underlying session when the channel closes."""

    def __init__(
        self,
        channel: Channel,
        fake: FakeProxyTarget,
        tasks: list[asyncio.Task[None]],
    ) -> None:
        super().__init__(channel)
        self._fake = fake
        self._tasks = tasks

    async def describe(self) -> Any:
        description = await self._fake.describe()
        return fdl.ProxyDescription(
            protocol_major=description.protocol_major,
            protocol_minor=description.protocol_minor,
            proxy_generation=description.proxy_generation,
            boot_id=description.boot_id,
            resource_digest=description.resource_digest,
            policy_digest=description.policy_digest,
            resources=[
                fdl.ResourceDescription(
                    id_=resource.id,
                    name=resource.name,
                    kind=_RESOURCE_KIND_TO_FIDL.get(
                        resource.kind, fdl.ResourceKind.MMIO
                    ),
                    logical_size=resource.logical_size,
                    digest=resource.digest,
                )
                for resource in description.resources
            ],
            max_snapshot_items=description.max_snapshot_items,
            audit_capacity=description.audit_capacity,
        )

    async def open_session(self, request: Any) -> Any:
        expectations = Expectations(
            boot_id=request.expectations.boot_id or "",
            proxy_generation=request.expectations.proxy_generation or 0,
            resource_digest=request.expectations.resource_digest or "",
            policy_digest=request.expectations.policy_digest or "",
        )
        allowlist = [
            AllowRule(
                resource=rule.resource,
                offset=rule.offset,
                width=rule.width,
                access=AccessClass(fdl.AccessClass(rule.class_).name.lower()),
            )
            for rule in request.allowlist
        ]
        context = SessionContext(
            run_id=request.context.run_id or "",
            case_id=request.context.case_id or "",
            plan_digest=request.context.plan_digest or "",
        )
        mode_val = getattr(request, "mode", None)
        mode = (
            SessionMode.MUTATING
            if mode_val == fdl.SessionMode.MUTATING
            else SessionMode.READ_ONLY
        )
        try:
            session = await self._fake.open_session(
                context, expectations, allowlist, mode=mode
            )
        except OpenSessionRejected as error:
            return DomainError(
                error=getattr(fdl.OpenSessionError, error.reason.name)
            )
        channel = request.session
        if not isinstance(channel, Channel):
            channel = Channel(channel)
        server = _InSituSessionServer(channel, session)

        async def serve_and_cleanup() -> None:
            try:
                await server.serve()
            finally:
                await session.close()

        self._tasks.append(
            asyncio.get_running_loop().create_task(serve_and_cleanup())
        )
        return fdl.ProxyOpenSessionResponse(session_id=1)


class InSituConformanceTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        GlobalHandleWaker()._reset_for_testing()
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.base = Path(self._dir.name)
        self.grants_path = self.base / "grants.toml"
        self.evidence_root = self.base / "evidence"
        self.sample = SyntheticSampleDriverTarget()
        self.tasks: list[asyncio.Task[None]] = []
        self.discovery = FakeNodeDiscovery(
            [
                NodeDescription(
                    moniker=SAMPLE_MONIKER,
                    bound_driver_url=SAMPLE_DRIVER_URL,
                    offers=(DRIVER_LAB_SERVICE,),
                ),
                NodeDescription(
                    moniker="dev.sys.platform.normal-driver",
                    bound_driver_url="fuchsia-pkg://fuchsia.com/normal#meta/normal.cm",
                    offers=(),
                ),
                NodeDescription(
                    moniker="dev.sys.platform.unbound-node",
                    bound_driver_url=None,
                    offers=(),
                ),
            ]
        )
        self.activator = FakeProxyActivator(self.discovery)

    async def asyncTearDown(self) -> None:
        for task in self.tasks:
            task.cancel()
        if self.tasks:
            await asyncio.gather(*self.tasks, return_exceptions=True)

    def _make_fidl_transport(self) -> FidlProxyTransport:
        context = Context(target="")
        self._context = context
        client_channel, server_channel = context.channel_create()
        proxy_server = _LifecycleBridgeProxyServer(
            server_channel, self.sample.fake, self.tasks
        )
        self.tasks.append(
            asyncio.get_running_loop().create_task(proxy_server.serve())
        )
        return FidlProxyTransport(
            fdl.ProxyClient(client_channel), context.channel_create
        )

    def _run_cli(self, *argv: str) -> tuple[int, dict[str, Any]]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(
            stderr
        ):
            code = cli.main(list(argv))
        payload = json.loads(stdout.getvalue()) if stdout.getvalue() else {}
        return code, payload

    def test_discovery_filters_debug_capable_drivers(self) -> None:
        nodes = asyncio.run(self.discovery.find_debug_capable())
        self.assertEqual([n.moniker for n in nodes], [SAMPLE_MONIKER])
        self.assertTrue(nodes[0].debug_capable)

        with mock.patch.object(
            cli, "_connect_discovery", return_value=self.discovery
        ):
            code, payload = self._run_cli("list", "--debug-capable")
        self.assertEqual(code, 0)
        self.assertEqual(len(payload["nodes"]), 1)
        self.assertEqual(payload["nodes"][0]["moniker"], SAMPLE_MONIKER)
        self.assertTrue(payload["nodes"][0]["debug_capable"])

    def test_in_situ_describe_and_inspect_preserve_bound_driver(self) -> None:
        with mock.patch.object(
            cli, "_connect_transport", return_value=self.sample.fake
        ), mock.patch.object(
            cli, "_connect_discovery", return_value=self.discovery
        ), mock.patch.object(
            cli, "_connect_activator", return_value=self.activator
        ):
            code, desc = self._run_cli("describe", "--moniker", SAMPLE_MONIKER)
            self.assertEqual(code, 0)
            self.assertEqual(
                [r["name"] for r in desc["description"]["resources"]],
                ["mmio0", "irq0", "state0"],
            )
            self.assertEqual(
                desc["node"]["bound_driver_url"], SAMPLE_DRIVER_URL
            )
            # Active driver was never unbound or rebound to a standalone proxy.
            self.assertEqual(self.activator.bind_calls, [])
            self.assertEqual(self.activator.end_calls, [])

            code_insp, inspected = self._run_cli(
                "inspect",
                "--moniker",
                SAMPLE_MONIKER,
                "--resource",
                "mmio0",
                "--offset",
                "0x00",
            )
            self.assertEqual(code_insp, 0)
            self.assertEqual(inspected["read"]["value"], DEVICE_ID_VALUE)
            self.assertEqual(inspected["driver_url"], SAMPLE_DRIVER_URL)
            self.assertEqual(self.activator.bind_calls, [])
            self.assertEqual(self.activator.end_calls, [])

    async def test_concurrent_readonly_observation_over_fidl(self) -> None:
        transport = self._make_fidl_transport()
        desc = await transport.describe()
        session = await transport.open_session(
            SessionContext(
                run_id="run-ro", case_id="ro", plan_digest="sha256:00"
            ),
            Expectations(
                boot_id=desc.boot_id,
                proxy_generation=desc.proxy_generation,
                resource_digest=desc.resource_digest,
                policy_digest=desc.policy_digest,
            ),
            [
                AllowRule(
                    resource=1,
                    offset=REG_DEVICE_ID,
                    width=4,
                    access=AccessClass.READ_ONCE,
                ),
                AllowRule(
                    resource=1,
                    offset=REG_STATUS,
                    width=4,
                    access=AccessClass.READ_ONCE,
                ),
            ],
            mode=SessionMode.READ_ONLY,
        )
        try:
            self.assertFalse(self.sample.is_quiesced)
            dev_id = await session.read32(1, REG_DEVICE_ID)
            status = await session.read32(1, REG_STATUS)
            self.assertEqual(dev_id.value, DEVICE_ID_VALUE)
            self.assertEqual(status.value, STATUS_RUNNING)
            self.assertEqual(status.value & STATUS_QUIESCED, 0)
        finally:
            await session.close()

    async def test_target_ceiling_blocks_hard_denied_and_non_writable_registers(
        self,
    ) -> None:
        transport = self._make_fidl_transport()
        desc = await transport.describe()
        expectations = Expectations(
            boot_id=desc.boot_id,
            proxy_generation=desc.proxy_generation,
            resource_digest=desc.resource_digest,
            policy_digest=desc.policy_digest,
        )

        # 1. Attempting to include hard-denied REG_FIFO_DATA (0x40) in the
        # session allowlist is rejected at OpenSession with REJECTED_ALLOWLIST.
        with self.assertRaises(OpenSessionRejected) as ctx_fifo:
            await transport.open_session(
                SessionContext(
                    run_id="run-fifo", case_id="fifo", plan_digest="sha256:00"
                ),
                expectations,
                [
                    AllowRule(
                        resource=1,
                        offset=REG_FIFO_DATA,
                        width=4,
                        access=AccessClass.READ_ONCE,
                    ),
                ],
                mode=SessionMode.READ_ONLY,
            )
        self.assertEqual(
            ctx_fifo.exception.reason, OpenRejection.REJECTED_ALLOWLIST
        )

        # 2. Attempting to request WRITE access to read-only REG_DEVICE_ID (0x00)
        # is rejected at OpenSession with REJECTED_ALLOWLIST.
        with self.assertRaises(OpenSessionRejected) as ctx_ro:
            await transport.open_session(
                SessionContext(
                    run_id="run-ro-write",
                    case_id="ro-write",
                    plan_digest="sha256:00",
                ),
                expectations,
                [
                    AllowRule(
                        resource=1,
                        offset=REG_DEVICE_ID,
                        width=4,
                        access=AccessClass.WRITE,
                    ),
                ],
                mode=SessionMode.MUTATING,
            )
        self.assertEqual(
            ctx_ro.exception.reason, OpenRejection.REJECTED_ALLOWLIST
        )

    async def test_cooperative_quiesce_interlock_and_disconnect_recovery(
        self,
    ) -> None:
        transport = self._make_fidl_transport()
        desc = await transport.describe()
        self.assertFalse(self.sample.is_quiesced)

        session = await transport.open_session(
            SessionContext(
                run_id="run-mut", case_id="mut", plan_digest="sha256:11"
            ),
            Expectations(
                boot_id=desc.boot_id,
                proxy_generation=desc.proxy_generation,
                resource_digest=desc.resource_digest,
                policy_digest=desc.policy_digest,
            ),
            [
                AllowRule(
                    resource=1,
                    offset=REG_STATUS,
                    width=4,
                    access=AccessClass.READ_ONCE,
                ),
                AllowRule(
                    resource=1,
                    offset=REG_CONTROL,
                    width=4,
                    access=AccessClass.WRITE,
                ),
            ],
            mode=SessionMode.MUTATING,
        )
        # Opening a Mutating session must engage the cooperative quiesce hook.
        self.assertTrue(self.sample.is_quiesced)
        status = await session.read32(1, REG_STATUS)
        self.assertNotEqual(status.value & STATUS_QUIESCED, 0)

        write_res = await session.write32(
            1, REG_CONTROL, 0xA5A5_0001, readback=True
        )
        self.assertEqual(write_res.readback_value, 0xA5A5_0001)

        # Closing the session channel must automatically release quiesce.
        await session.close()
        await asyncio.sleep(0.02)
        self.assertFalse(self.sample.is_quiesced)
        self.assertEqual(
            self.sample.fake.get_value(1, REG_STATUS) & STATUS_QUIESCED,
            0,
        )

        ops = [entry.operation for entry in self.sample.fake.audit_entries]
        self.assertIn("quiesce_engaged", ops)
        self.assertIn("quiesce_released", ops)

    async def test_interrupt_tapping_observes_irq_concurrently_with_driver_isr(
        self,
    ) -> None:
        transport = self._make_fidl_transport()
        desc = await transport.describe()
        session = await transport.open_session(
            SessionContext(
                run_id="run-irq", case_id="irq", plan_digest="sha256:22"
            ),
            Expectations(
                boot_id=desc.boot_id,
                proxy_generation=desc.proxy_generation,
                resource_digest=desc.resource_digest,
                policy_digest=desc.policy_digest,
            ),
            [
                AllowRule(
                    resource=2,
                    offset=0,
                    width=4,
                    access=AccessClass.INTERRUPT,
                ),
            ],
            mode=SessionMode.READ_ONLY,
        )
        try:
            # Hardware fires irq0: both the driver's own ISR and the
            # LabInstance interrupt tap receive the event.
            self.sample.trigger_hardware_irq()
            self.assertEqual(self.sample.driver_isr_count, 1)

            irq = await session.wait_for_interrupt(
                2, after_sequence=0, timeout_s=0.5
            )
            self.assertEqual(irq.resource, 2)
            self.assertEqual(irq.sequence, 1)
            self.assertEqual(irq.count, 1)
            ops = [entry.operation for entry in self.sample.fake.audit_entries]
            self.assertIn("interrupt_triggered", ops)
        finally:
            await session.close()

    async def test_end_to_end_in_situ_run_plan_produces_complete_evidence_bundle(
        self,
    ) -> None:
        from driver_lab.consent import ConsentDecision

        class AllowOncePrompt:
            async def request_consent(
                self, req: Any, warning: str
            ) -> ConsentDecision:
                return ConsentDecision.ALLOW_ONCE

        save_grants(
            self.grants_path,
            [
                ReadGrant(
                    schema_version=1,
                    target_scope=TARGET_SCOPE,
                    node_id=SAMPLE_MONIKER,
                    resource_digest=MMIO_DIGEST,
                    resource="mmio0",
                    offset=REG_DEVICE_ID,
                    width=4,
                    access=AccessClass.READ_ONCE,
                    decision=Decision.ALLOW,
                    approved_at="2026-09-22T00:00:00Z",
                ),
                ReadGrant(
                    schema_version=1,
                    target_scope=TARGET_SCOPE,
                    node_id=SAMPLE_MONIKER,
                    resource_digest=MMIO_DIGEST,
                    resource="mmio0",
                    offset=REG_STATUS,
                    width=4,
                    access=AccessClass.READ_ONCE,
                    decision=Decision.ALLOW,
                    approved_at="2026-09-22T00:00:00Z",
                ),
            ],
        )

        plan: dict[str, Any] = {
            "schema_version": 1,
            "run_id": "run-in-situ-conformance",
            "case_id": "quiesced-control-poke",
            "target": {"selector": "lab-target"},
            "node": {
                "id": SAMPLE_MONIKER,
                "driver_moniker": SAMPLE_MONIKER,
                "expected_unclaimed": False,
                "expected_resource_digest": RESOURCE_DIGEST,
            },
            "access": {"mode": "in-situ", "activation": "in-situ"},
            "operations": [
                {
                    "kind": "mmio_read32",
                    "resource": "mmio0",
                    "offset": "0x00",
                },
                {
                    "kind": "mmio_read32",
                    "resource": "mmio0",
                    "offset": "0x04",
                },
                {
                    "kind": "mmio_write32",
                    "resource": "mmio0",
                    "offset": "0x08",
                    "value": "0x00000007",
                    "readback": True,
                },
            ],
        }

        transport = self._make_fidl_transport()
        lab = DriverLab(
            transport,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope=TARGET_SCOPE,
            node_id=SAMPLE_MONIKER,
            driver_moniker=SAMPLE_MONIKER,
            driver_url=SAMPLE_DRIVER_URL,
            discovery=self.discovery,
            activator=self.activator,
            consent=AllowOncePrompt(),
        )
        result = await lab.run_plan(plan)
        self.assertTrue(result.ok, result.failure)

        # Wait briefly for async server channel cleanup.
        await asyncio.sleep(0.02)
        self.assertFalse(self.sample.is_quiesced)
        self.assertEqual(self.activator.bind_calls, [])
        self.assertEqual(self.activator.end_calls, [])

        manifest = json.loads(
            (result.evidence_dir / "manifest.json").read_text(encoding="utf-8")
        )
        self.assertEqual(manifest["driver_moniker"], SAMPLE_MONIKER)
        self.assertEqual(manifest["driver_url"], SAMPLE_DRIVER_URL)

        audit_ops = [
            json.loads(line)["operation"]
            for line in (result.evidence_dir / "target-audit.jsonl")
            .read_text(encoding="utf-8")
            .splitlines()
        ]
        self.assertIn("open_session", audit_ops)
        self.assertIn("quiesce_engaged", audit_ops)
        self.assertIn("read32", audit_ops)
        self.assertIn("write32", audit_ops)

    async def test_in_situ_software_state_knob_and_trigger_single_build_repro_and_fix_proof(
        self,
    ) -> None:
        from driver_lab.consent import ConsentDecision

        class AllowOncePrompt:
            async def request_consent(
                self, req: Any, warning: str
            ) -> ConsentDecision:
                return ConsentDecision.ALLOW_ONCE

        save_grants(
            self.grants_path,
            [
                ReadGrant(
                    schema_version=1,
                    target_scope=TARGET_SCOPE,
                    node_id=SAMPLE_MONIKER,
                    resource_digest=STATE_DIGEST,
                    resource="state0",
                    offset=STATE_INVARIANT_VIOLATIONS,
                    width=4,
                    access=AccessClass.READ_ONCE,
                    decision=Decision.ALLOW,
                    approved_at="2026-09-22T00:00:00Z",
                ),
                ReadGrant(
                    schema_version=1,
                    target_scope=TARGET_SCOPE,
                    node_id=SAMPLE_MONIKER,
                    resource_digest=STATE_DIGEST,
                    resource="state0",
                    offset=STATE_INVARIANT_VIOLATIONS,
                    width=4,
                    access=AccessClass.POLL,
                    decision=Decision.ALLOW,
                    approved_at="2026-09-22T00:00:00Z",
                    max_poll_hz=1000,
                    max_poll_timeout_s=1.0,
                ),
            ],
        )

        # Plan 1: Reproduce the race bug in-situ by widening KNOB_RACE_DELAY_US
        # with KNOB_FIX_ENABLED=0 and firing TRIGGER_CONCURRENT_OP.
        repro_plan: dict[str, Any] = {
            "schema_version": 1,
            "run_id": "run-mode-b-repro",
            "case_id": "repro-race-violation",
            "target": {"selector": "lab-target"},
            "node": {
                "id": SAMPLE_MONIKER,
                "driver_moniker": SAMPLE_MONIKER,
                "expected_unclaimed": False,
                "expected_resource_digest": RESOURCE_DIGEST,
            },
            "access": {"mode": "in-situ", "activation": "in-situ"},
            "operations": [
                {
                    "kind": "knob_write32",
                    "offset": KNOB_RACE_DELAY_US,
                    "value": 5000,
                },
                {
                    "kind": "knob_write32",
                    "offset": KNOB_FIX_ENABLED,
                    "value": 0,
                },
                {
                    "kind": "trigger_write32",
                    "offset": TRIGGER_CONCURRENT_OP,
                    "arg": 2,
                },
                {
                    "kind": "state_read32",
                    "offset": STATE_INVARIANT_VIOLATIONS,
                },
            ],
        }

        transport1 = self._make_fidl_transport()
        lab1 = DriverLab(
            transport1,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope=TARGET_SCOPE,
            node_id=SAMPLE_MONIKER,
            driver_moniker=SAMPLE_MONIKER,
            driver_url=SAMPLE_DRIVER_URL,
            discovery=self.discovery,
            activator=self.activator,
            consent=AllowOncePrompt(),
        )
        repro_res = await lab1.run_plan(repro_plan)
        self.assertTrue(repro_res.ok, repro_res.failure)
        self.assertEqual(len(repro_res.reads), 1)
        self.assertEqual(repro_res.reads[0].value, 2)

        repro_ops = [
            json.loads(line)["kind"]
            for line in (repro_res.evidence_dir / "operations.jsonl")
            .read_text(encoding="utf-8")
            .splitlines()
        ]
        self.assertEqual(
            repro_ops,
            [
                "knob_write32",
                "knob_write32",
                "trigger_write32",
                "state_read32",
            ],
        )

        # Plan 2: Prove the fix in the SAME boot (without a rebuild) by toggling
        # KNOB_FIX_ENABLED=1, firing TRIGGER_CONCURRENT_OP inside a sequence,
        # and polling STATE_INVARIANT_VIOLATIONS to confirm it stays at 2.
        fix_proof_plan: dict[str, Any] = {
            "schema_version": 1,
            "run_id": "run-mode-b-fix-proof",
            "case_id": "prove-fix-via-knob",
            "target": {"selector": "lab-target"},
            "node": {
                "id": SAMPLE_MONIKER,
                "driver_moniker": SAMPLE_MONIKER,
                "expected_unclaimed": False,
                "expected_resource_digest": RESOURCE_DIGEST,
            },
            "access": {"mode": "in-situ", "activation": "in-situ"},
            "operations": [
                {
                    "kind": "sequence",
                    "items": [
                        {
                            "kind": "knob_write32",
                            "offset": KNOB_FIX_ENABLED,
                            "value": 1,
                        },
                        {
                            "kind": "trigger_write32",
                            "offset": TRIGGER_CONCURRENT_OP,
                            "arg": 3,
                        },
                        {
                            "kind": "state_read32",
                            "offset": STATE_INVARIANT_VIOLATIONS,
                        },
                    ],
                },
                {
                    "kind": "state_poll32",
                    "offset": STATE_INVARIANT_VIOLATIONS,
                    "expected": 2,
                },
            ],
        }

        transport2 = self._make_fidl_transport()
        lab2 = DriverLab(
            transport2,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope=TARGET_SCOPE,
            node_id=SAMPLE_MONIKER,
            driver_moniker=SAMPLE_MONIKER,
            driver_url=SAMPLE_DRIVER_URL,
            discovery=self.discovery,
            activator=self.activator,
            consent=AllowOncePrompt(),
        )
        proof_res = await lab2.run_plan(fix_proof_plan)
        self.assertTrue(proof_res.ok, proof_res.failure)
        self.assertEqual(len(proof_res.polls), 1)
        self.assertEqual(proof_res.polls[0].value, 2)


if __name__ == "__main__":
    unittest.main()
