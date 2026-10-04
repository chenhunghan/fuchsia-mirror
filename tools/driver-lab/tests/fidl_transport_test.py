# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Round-trip tests for the FIDL transport adapter.

A Python-served `fuchsia.driver.lab` implementation bridges to
`FakeProxyTarget`, so the adapter is exercised through real FIDL
encoding over in-process channel pairs -- no emulator required. This is
also the seed of a conformance suite: the same fake semantics the API
tests rely on are here proven reachable through the wire contract.
"""

import asyncio
import json
import os
import tempfile
import unittest
from pathlib import Path
from typing import Any

import fidl_fuchsia_driver_lab as fdl
from driver_lab.api import EXIT_OPERATION, EXIT_STALE, DriverLab
from driver_lab.consent import ConsentDecision
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
    SequenceItem,
    SessionContext,
    SessionMode,
    SnapshotItem,
)
from fidl import DomainError
from fidl._ipc import GlobalHandleWaker
from fuchsia_controller_py import Channel, Context

CTRL_DIGEST = "sha256:" + "ab" * 32
DESCRIPTION_DIGEST = "sha256:" + "ee" * 32
POLICY_DIGEST = "sha256:" + "ff" * 32
TARGET_SCOPE = "example-engineering-target"
NODE_ID = "example-device"


def make_description() -> ProxyDescription:
    return ProxyDescription(
        protocol_major=1,
        protocol_minor=0,
        proxy_generation=7,
        boot_id="boot-1",
        resource_digest=DESCRIPTION_DIGEST,
        policy_digest=POLICY_DIGEST,
        resources=(
            ResourceInfo(
                id=1, name="control", logical_size=0x100, digest=CTRL_DIGEST
            ),
        ),
        max_snapshot_items=64,
        audit_capacity=1024,
    )


class _BridgeSessionServer(fdl.SessionServer):
    """Serves one `Session` channel by delegating to a fake session."""

    def __init__(self, channel: Channel, session: ProxySession) -> None:
        super().__init__(channel)
        self._session = session

    async def read32(self, request: Any) -> Any:
        try:
            outcome = await self._session.read32(
                request.resource, request.offset
            )
        except Exception as error:  # OperationDenied
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
        except Exception as error:  # OperationDenied
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
        except Exception as error:  # OperationDenied
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
        except Exception as error:  # OperationDenied
            denial = getattr(error, "denial", None)
            if denial is None:
                raise
            return DomainError(error=getattr(fdl.OperationError, denial.name))
        return fdl.PollResult(
            value=outcome.value,
            audit_seq=outcome.audit_seq,
            timestamp_ns=outcome.timestamp_ns,
        )

    async def gpio_read(self, request: Any) -> Any:
        try:
            outcome = await self._session.gpio_read(request.resource)
        except Exception as error:  # OperationDenied
            denial = getattr(error, "denial", None)
            if denial is None:
                raise
            return DomainError(error=getattr(fdl.OperationError, denial.name))
        return fdl.GpioReadResult(
            value=outcome.value,
            audit_seq=outcome.audit_seq,
            timestamp_ns=outcome.timestamp_ns,
        )

    async def gpio_write(self, request: Any) -> Any:
        try:
            outcome = await self._session.gpio_write(
                request.resource, request.value
            )
        except Exception as error:  # OperationDenied
            denial = getattr(error, "denial", None)
            if denial is None:
                raise
            return DomainError(error=getattr(fdl.OperationError, denial.name))
        return fdl.GpioWriteResult(
            audit_seq=outcome.audit_seq,
            timestamp_ns=outcome.timestamp_ns,
        )

    async def i2c_transfer(self, request: Any) -> Any:
        try:
            outcome = await self._session.i2c_transfer(
                request.resource,
                write_data=bytes(request.write_data),
                read_length=request.read_length,
            )
        except Exception as error:  # OperationDenied
            denial = getattr(error, "denial", None)
            if denial is None:
                raise
            return DomainError(error=getattr(fdl.OperationError, denial.name))
        return fdl.I2cTransferResult(
            read_data=list(outcome.read_data),
            audit_seq=outcome.audit_seq,
            timestamp_ns=outcome.timestamp_ns,
        )

    async def spi_transmit(self, request: Any) -> Any:
        try:
            outcome = await self._session.spi_transmit(
                request.resource,
                tx_data=bytes(request.tx_data),
            )
        except Exception as error:  # OperationDenied
            denial = getattr(error, "denial", None)
            if denial is None:
                raise
            return DomainError(error=getattr(fdl.OperationError, denial.name))
        return fdl.SpiTransmitResult(
            rx_data=list(outcome.rx_data),
            audit_seq=outcome.audit_seq,
            timestamp_ns=outcome.timestamp_ns,
        )

    async def open_gpio(self, request: Any) -> Any:
        return fdl.SessionOpenGpioResponse()

    async def open_i2c(self, request: Any) -> Any:
        return fdl.SessionOpenI2cResponse()

    async def open_spi(self, request: Any) -> Any:
        return fdl.SessionOpenSpiResponse()

    async def execute_sequence(self, request: Any) -> Any:
        host_items: list[SequenceItem] = []
        for item in request.items:
            if item.read32 is not None:
                host_items.append(
                    SequenceItem(
                        kind="read32",
                        resource=item.read32.resource,
                        offset=item.read32.offset,
                    )
                )
            elif item.write32 is not None:
                p = None
                if item.write32.precondition is not None:
                    p = (
                        item.write32.precondition.expected,
                        item.write32.precondition.mask,
                    )
                host_items.append(
                    SequenceItem(
                        kind="write32",
                        resource=item.write32.resource,
                        offset=item.write32.offset,
                        value=item.write32.value,
                        write_mask=item.write32.write_mask,
                        precondition=p,
                        readback=item.write32.readback,
                    )
                )
            elif item.poll32 is not None:
                host_items.append(
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
                host_items.append(
                    SequenceItem(kind="delay_ns", delay_ns=item.delay_ns)
                )
            elif item.barrier is not None:
                host_items.append(
                    SequenceItem(kind="barrier", barrier="memory")
                )
            elif item.gpio_read is not None:
                host_items.append(
                    SequenceItem(
                        kind="gpio_read",
                        resource=item.gpio_read.resource,
                    )
                )
            elif item.gpio_write is not None:
                host_items.append(
                    SequenceItem(
                        kind="gpio_write",
                        resource=item.gpio_write.resource,
                        value=item.gpio_write.value,
                    )
                )
            elif item.i2c_transfer is not None:
                host_items.append(
                    SequenceItem(
                        kind="i2c_transfer",
                        resource=item.i2c_transfer.resource,
                        write_data=bytes(item.i2c_transfer.write_data),
                        read_length=item.i2c_transfer.read_length,
                    )
                )
            elif item.spi_transmit is not None:
                host_items.append(
                    SequenceItem(
                        kind="spi_transmit",
                        resource=item.spi_transmit.resource,
                        tx_data=bytes(item.spi_transmit.tx_data),
                    )
                )
        try:
            outcome = await self._session.execute_sequence(host_items)
        except Exception as error:  # OperationDenied
            denial = getattr(error, "denial", None)
            if denial is None:
                raise
            return DomainError(error=getattr(fdl.OperationError, denial.name))

        fidl_results: list[fdl.SequenceItemResult] = []
        for res in outcome.results:
            if not res.ok:
                err_name = (
                    res.error.name if res.error is not None else "NOT_ACCEPTING"
                )
                item_outcome = fdl.SequenceItemOutcome(
                    error=getattr(fdl.OperationError, err_name)
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
            elif res.kind == "barrier":
                item_outcome = fdl.SequenceItemOutcome(barrier=fdl.Barrier())
            elif res.kind == "gpio_read":
                item_outcome = fdl.SequenceItemOutcome(
                    gpio_read=fdl.GpioReadResult(
                        value=bool(res.value),
                        audit_seq=res.audit_seq,
                        timestamp_ns=res.timestamp_ns,
                    )
                )
            elif res.kind == "gpio_write":
                item_outcome = fdl.SequenceItemOutcome(
                    gpio_write=fdl.GpioWriteResult(
                        audit_seq=res.audit_seq,
                        timestamp_ns=res.timestamp_ns,
                    )
                )
            elif res.kind == "i2c_transfer":
                item_outcome = fdl.SequenceItemOutcome(
                    i2c_transfer=fdl.I2cTransferResult(
                        read_data=list(res.data),
                        audit_seq=res.audit_seq,
                        timestamp_ns=res.timestamp_ns,
                    )
                )
            elif res.kind == "spi_transmit":
                item_outcome = fdl.SequenceItemOutcome(
                    spi_transmit=fdl.SpiTransmitResult(
                        rx_data=list(res.data),
                        audit_seq=res.audit_seq,
                        timestamp_ns=res.timestamp_ns,
                    )
                )
            else:
                item_outcome = fdl.SequenceItemOutcome(
                    error=fdl.OperationError.NOT_ACCEPTING
                )
            fidl_results.append(
                fdl.SequenceItemResult(
                    index=res.index,
                    ok=res.ok,
                    outcome=item_outcome,
                )
            )
        return fdl.SequenceResult(
            results=fidl_results,
            complete=outcome.complete,
        )

    async def wait_for_interrupt(self, request: Any) -> Any:
        try:
            outcome = await self._session.wait_for_interrupt(
                request.resource,
                after_sequence=request.after_sequence,
                timeout_s=request.timeout_ns / 1_000_000_000,
            )
        except Exception as error:  # OperationDenied
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


class _BridgeProxyServer(fdl.ProxyServer):
    """Serves `Proxy` by delegating to a `FakeProxyTarget`."""

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
        # Mirror the real driver: reserved phase 2 expectations fail
        # closed, and an absent required field falls through to the
        # per-field stale check.
        if (
            request.expectations.topology_generation is not None
            or request.expectations.bound_driver_url is not None
        ):
            return DomainError(
                error=fdl.OpenSessionError.UNSUPPORTED_EXPECTATION
            )
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
                # Decoded enum fields may arrive as raw ints.
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
        server = _BridgeSessionServer(channel, session)
        self._tasks.append(
            asyncio.get_running_loop().create_task(server.serve())
        )
        return fdl.ProxyOpenSessionResponse(session_id=1)


def make_grant(**overrides: object) -> ReadGrant:
    fields: dict[str, Any] = dict(
        schema_version=1,
        target_scope=TARGET_SCOPE,
        node_id=NODE_ID,
        resource_digest=CTRL_DIGEST,
        resource="control",
        offset=0x3C,
        width=4,
        access=AccessClass.READ_ONCE,
        decision=Decision.ALLOW,
        approved_at="2026-07-30T00:00:00Z",
    )
    fields.update(overrides)
    return ReadGrant(**fields)


def make_plan(**overrides: object) -> dict[str, Any]:
    plan: dict[str, Any] = {
        "schema_version": 1,
        "run_id": "run-1",
        "case_id": "case-1",
        "target": {"selector": "lab-target"},
        "node": {
            "id": NODE_ID,
            "expected_resource_digest": DESCRIPTION_DIGEST,
        },
        "access": {"mode": "proxy", "activation": "bind-unclaimed"},
        "operations": [
            {"kind": "mmio_read32", "resource": "control", "offset": "0x3c"},
        ],
    }
    plan.update(overrides)
    return plan


class FidlRoundTripTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        GlobalHandleWaker()._reset_for_testing()
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        base = Path(self._dir.name)
        self.grants_path = base / "grants.toml"
        self.evidence_root = base / "evidence"
        self.fake = FakeProxyTarget(make_description())
        self.fake.set_value(1, 0x3C, 0xDEAD_BEEF)
        self.tasks: list[asyncio.Task[None]] = []
        self._orig_nodename = os.environ.pop("FUCHSIA_NODENAME", None)
        self._orig_device_addr = os.environ.pop("FUCHSIA_DEVICE_ADDR", None)

    async def asyncTearDown(self) -> None:
        for task in self.tasks:
            if task.done() and not task.cancelled():
                exception = task.exception()
                if exception is not None:
                    import traceback

                    traceback.print_exception(exception)
            task.cancel()
        if self._orig_nodename is not None:
            os.environ["FUCHSIA_NODENAME"] = self._orig_nodename
        if self._orig_device_addr is not None:
            os.environ["FUCHSIA_DEVICE_ADDR"] = self._orig_device_addr

    def make_lab(self, consent: Any = None) -> DriverLab:
        context = Context(target="")
        (client_channel, server_channel) = context.channel_create()
        self._context = context
        proxy_server = _BridgeProxyServer(server_channel, self.fake, self.tasks)
        self.tasks.append(
            asyncio.get_running_loop().create_task(proxy_server.serve())
        )
        transport = FidlProxyTransport(
            fdl.ProxyClient(client_channel), context.channel_create
        )
        return DriverLab(
            transport,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope=TARGET_SCOPE,
            node_id=NODE_ID,
            consent=consent,
        )

    async def test_describe_round_trips(self) -> None:
        context = Context(target="")
        (client_channel, server_channel) = context.channel_create()
        proxy_server = _BridgeProxyServer(server_channel, self.fake, self.tasks)
        self.tasks.append(
            asyncio.get_running_loop().create_task(proxy_server.serve())
        )
        adapter = FidlProxyTransport(
            fdl.ProxyClient(client_channel), context.channel_create
        )
        description = await adapter.describe()
        self.assertEqual(description, make_description())

    async def test_run_plan_over_real_fidl(self) -> None:
        save_grants(self.grants_path, [make_grant()])
        lab = self.make_lab()
        result = await lab.run_plan(make_plan())
        self.assertTrue(result.ok, result.failure)
        self.assertEqual(result.reads[0].value, 0xDEAD_BEEF)
        audit_ops = [
            json.loads(line)["operation"]
            for line in (result.evidence_dir / "target-audit.jsonl")
            .read_text()
            .splitlines()
        ]
        self.assertIn("open_session", audit_ops)
        self.assertIn("read32", audit_ops)

    async def test_backend_fault_over_fidl(self) -> None:
        save_grants(self.grants_path, [make_grant()])
        self.fake.fail_at(1, 0x3C)
        lab = self.make_lab()
        result = await lab.run_plan(make_plan())
        self.assertEqual(result.exit_category, EXIT_OPERATION)
        self.assertEqual(result.failure, "backend_fault")

    async def test_session_rejection_over_fidl(self) -> None:
        save_grants(self.grants_path, [make_grant()])
        self.fake.reject_open = OpenRejection.STALE_PROXY_GENERATION
        lab = self.make_lab()
        result = await lab.run_plan(make_plan())
        self.assertEqual(result.exit_category, EXIT_STALE)
        self.assertEqual(
            result.failure, "session rejected: stale_proxy_generation"
        )

    async def test_write32_over_real_fidl(self) -> None:
        class Prompt:
            async def request_consent(
                self, req: Any, warning: str
            ) -> ConsentDecision:
                return ConsentDecision.ALLOW_ONCE

        lab = self.make_lab(consent=Prompt())
        plan = make_plan(
            operations=[
                {
                    "kind": "mmio_write32",
                    "resource": "control",
                    "offset": "0x3c",
                    "value": 0xCAFE_BABE,
                    "readback": True,
                }
            ]
        )
        result = await lab.run_plan(plan)
        self.assertTrue(result.ok, result.failure)
        self.assertEqual(len(result.writes), 1)
        self.assertEqual(result.writes[0].value, 0xCAFE_BABE)
        self.assertEqual(result.writes[0].readback_value, 0xCAFE_BABE)
        self.assertEqual(self.fake.get_value(1, 0x3C), 0xCAFE_BABE)
        audit_ops = [
            json.loads(line)["operation"]
            for line in (result.evidence_dir / "target-audit.jsonl")
            .read_text()
            .splitlines()
        ]
        self.assertIn("write32", audit_ops)

    async def test_poll32_over_real_fidl(self) -> None:
        class Prompt:
            async def request_consent(
                self, req: Any, warning: str
            ) -> ConsentDecision:
                return ConsentDecision.ALLOW_ONCE

        lab = self.make_lab(consent=Prompt())
        plan = make_plan(
            operations=[
                {
                    "kind": "mmio_poll32",
                    "resource": "control",
                    "offset": "0x3c",
                    "expected": 0xDEAD_BEEF,
                    "mask": 0xFFFF_FFFF,
                    "interval_ns": 1000,
                    "timeout_ns": 100_000,
                }
            ]
        )
        result = await lab.run_plan(plan)
        self.assertTrue(result.ok, result.failure)
        self.assertEqual(len(result.polls), 1)
        self.assertEqual(result.polls[0].value, 0xDEAD_BEEF)

    async def test_sequence_over_real_fidl(self) -> None:
        class Prompt:
            async def request_consent(
                self, req: Any, warning: str
            ) -> ConsentDecision:
                return ConsentDecision.ALLOW_ONCE

        lab = self.make_lab(consent=Prompt())
        plan = make_plan(
            operations=[
                {
                    "kind": "sequence",
                    "items": [
                        {
                            "kind": "mmio_read32",
                            "resource": "control",
                            "offset": "0x3c",
                        },
                        {"kind": "delay_ns", "duration_ns": 1000},
                        {"kind": "barrier", "variant": "memory"},
                        {
                            "kind": "mmio_write32",
                            "resource": "control",
                            "offset": "0x3c",
                            "value": 0xFEED_FACE,
                            "readback": True,
                        },
                        {
                            "kind": "mmio_poll32",
                            "resource": "control",
                            "offset": "0x3c",
                            "expected": 0xFEED_FACE,
                            "interval_ns": 1000,
                            "timeout_ns": 100_000,
                        },
                    ],
                }
            ]
        )
        result = await lab.run_plan(plan)
        self.assertTrue(result.ok, result.failure)
        self.assertEqual(len(result.sequences), 1)
        seq = result.sequences[0]
        self.assertTrue(seq.complete)
        self.assertEqual(len(seq.results), 5)
        self.assertEqual(seq.results[0].kind, "read32")
        self.assertEqual(seq.results[0].value, 0xDEAD_BEEF)
        self.assertEqual(seq.results[1].kind, "delay_ns")
        self.assertEqual(seq.results[2].kind, "barrier")
        self.assertEqual(seq.results[3].kind, "write32")
        self.assertEqual(seq.results[3].readback_value, 0xFEED_FACE)
        self.assertEqual(seq.results[4].kind, "poll32")
        self.assertEqual(seq.results[4].value, 0xFEED_FACE)
        self.assertEqual(self.fake.get_value(1, 0x3C), 0xFEED_FACE)

    async def test_protocol_methods_over_real_fidl(self) -> None:
        desc = ProxyDescription(
            protocol_major=1,
            protocol_minor=0,
            proxy_generation=7,
            boot_id="boot-1",
            resource_digest=DESCRIPTION_DIGEST,
            policy_digest=POLICY_DIGEST,
            resources=(
                ResourceInfo(
                    id=1, name="control", logical_size=0x100, digest=CTRL_DIGEST
                ),
                ResourceInfo(
                    id=2,
                    name="pin0",
                    kind=ResourceKind.GPIO,
                    logical_size=1,
                    digest="sha256:" + "01" * 32,
                ),
                ResourceInfo(
                    id=3,
                    name="i2c-bus",
                    kind=ResourceKind.I2C,
                    logical_size=32,
                    digest="sha256:" + "02" * 32,
                ),
                ResourceInfo(
                    id=4,
                    name="spi-bus",
                    kind=ResourceKind.SPI,
                    logical_size=32,
                    digest="sha256:" + "03" * 32,
                ),
            ),
            max_snapshot_items=64,
            audit_capacity=1024,
        )
        self.fake = FakeProxyTarget(desc)
        self.fake.set_i2c_response(3, b"\xca\xfe")
        self.fake.set_spi_response(4, b"\xba\xbe")

        self._context = Context(target="")
        client_channel, server_channel = self._context.channel_create()
        proxy_server = _BridgeProxyServer(server_channel, self.fake, self.tasks)
        self.tasks.append(
            asyncio.get_running_loop().create_task(proxy_server.serve())
        )
        transport = FidlProxyTransport(
            fdl.ProxyClient(client_channel), self._context.channel_create
        )

        expectations = Expectations(
            boot_id="boot-1",
            proxy_generation=7,
            resource_digest=DESCRIPTION_DIGEST,
            policy_digest=POLICY_DIGEST,
        )
        session_ctx = SessionContext(
            run_id="run-proto",
            case_id="case-proto",
            plan_digest="digest-proto",
        )
        allowlist = [
            AllowRule(
                resource=2, offset=0, width=1, access=AccessClass.PROTOCOL
            ),
            AllowRule(
                resource=3, offset=0, width=32, access=AccessClass.PROTOCOL
            ),
            AllowRule(
                resource=4, offset=0, width=32, access=AccessClass.PROTOCOL
            ),
        ]
        session = await transport.open_session(
            session_ctx, expectations, allowlist, mode=SessionMode.MUTATING
        )

        # Gpio write & read
        w_out = await session.gpio_write(2, True)
        self.assertGreater(w_out.audit_seq, 0)
        r_out = await session.gpio_read(2)
        self.assertTrue(r_out.value)
        self.assertGreater(r_out.audit_seq, 0)

        # I2C transfer
        i2c_out = await session.i2c_transfer(
            3, write_data=b"\x01", read_length=2
        )
        self.assertEqual(i2c_out.read_data, b"\xca\xfe")
        self.assertGreater(i2c_out.audit_seq, 0)

        # SPI transmit
        spi_out = await session.spi_transmit(4, tx_data=b"\x02\x03")
        self.assertEqual(spi_out.rx_data, b"\xba\xbe")
        self.assertGreater(spi_out.audit_seq, 0)

        # Heterogeneous sequence over real FIDL
        seq_items = [
            SequenceItem(kind="gpio_write", resource=2, value=False),
            SequenceItem(kind="gpio_read", resource=2),
            SequenceItem(kind="delay_ns", delay_ns=500),
            SequenceItem(kind="barrier", barrier="memory"),
            SequenceItem(
                kind="i2c_transfer",
                resource=3,
                write_data=b"\x05",
                read_length=2,
            ),
            SequenceItem(kind="spi_transmit", resource=4, tx_data=b"\x06"),
        ]
        seq_out = await session.execute_sequence(seq_items)
        self.assertTrue(seq_out.complete)
        self.assertEqual(len(seq_out.results), 6)
        self.assertEqual(seq_out.results[0].kind, "gpio_write")
        self.assertEqual(seq_out.results[1].kind, "gpio_read")
        self.assertEqual(seq_out.results[1].value, 0)
        self.assertEqual(seq_out.results[2].kind, "delay_ns")
        self.assertEqual(seq_out.results[3].kind, "barrier")
        self.assertEqual(seq_out.results[4].kind, "i2c_transfer")
        self.assertEqual(seq_out.results[4].data, b"\xca\xfe")
        self.assertEqual(seq_out.results[5].kind, "spi_transmit")
        self.assertEqual(seq_out.results[5].data, b"\xba\xbe")

        await session.close()

    async def test_interrupt_over_real_fidl(self) -> None:
        desc = ProxyDescription(
            protocol_major=1,
            protocol_minor=0,
            proxy_generation=7,
            boot_id="boot-1",
            resource_digest=DESCRIPTION_DIGEST,
            policy_digest=POLICY_DIGEST,
            resources=(
                ResourceInfo(
                    id=5,
                    name="irq0",
                    kind=ResourceKind.INTERRUPT,
                    logical_size=0,
                    digest="sha256:" + "05" * 32,
                ),
            ),
            max_snapshot_items=64,
            audit_capacity=1024,
        )
        self.fake = FakeProxyTarget(desc)
        self.fake.trigger_interrupt(5, timestamp_ns=123_456)

        self._context = Context(target="")
        client_channel, server_channel = self._context.channel_create()
        proxy_server = _BridgeProxyServer(server_channel, self.fake, self.tasks)
        self.tasks.append(
            asyncio.get_running_loop().create_task(proxy_server.serve())
        )
        transport = FidlProxyTransport(
            fdl.ProxyClient(client_channel), self._context.channel_create
        )

        expectations = Expectations(
            boot_id="boot-1",
            proxy_generation=7,
            resource_digest=DESCRIPTION_DIGEST,
            policy_digest=POLICY_DIGEST,
        )
        session_ctx = SessionContext(
            run_id="run-irq",
            case_id="case-irq",
            plan_digest="digest-irq",
        )
        allowlist = [
            AllowRule(
                resource=5, offset=0, width=0, access=AccessClass.INTERRUPT
            ),
        ]
        session = await transport.open_session(
            session_ctx, expectations, allowlist, mode=SessionMode.READ_ONLY
        )

        outcome = await session.wait_for_interrupt(
            5, after_sequence=0, timeout_s=1.0
        )
        self.assertEqual(outcome.resource, 5)
        self.assertEqual(outcome.sequence, 1)
        self.assertEqual(outcome.count, 1)
        self.assertEqual(outcome.timestamp_ns, 123_456)
        self.assertEqual(outcome.coalesced_count, 1)
        self.assertEqual(int(outcome), 123_456)

        await session.close()


if __name__ == "__main__":
    unittest.main()
