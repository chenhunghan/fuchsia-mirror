# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""E2E Power Measurement Tests.

**Prerequisites:**
1. The target device must have On-Device Power Monitor (ODPM) hardware. These tests will fail immediately if ODPM is unavailable.
2. The WLAN chip must be on its own isolated power rail. This ensures that ODPM can isolate and measure the actual WLAN chip's power usage without interference from other components.

This test suite measures power usage using ODPM during various WLAN use cases. The primary goal is to evaluate the power profile
of device states, including Wi-Fi chip off, idle disconnected, active connections
with different levels of traffic, suspend mode, scanning, and firmware power saving modes.
"""

import asyncio
import logging
import struct
from datetime import timedelta
from typing import Any, List

import fidl_fuchsia_wlan_wlanix as fidl_wlanix
import fuchsia_wlan_base_test
import wlanix_testing
from honeydew.utils import power
from honeydew.utils.deadline import Deadline
from legacy_access_point.access_point import AccessPoint, setup_ap
from legacy_access_point.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from mobly import signals, test_runner
from mobly.asserts import assert_equal, assert_less, assert_true
from openwrt_access_point import OpenWrtAP
from openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    BssChannel,
    BssSettings,
    HtMode,
    RadioConfig,
    SecurityWpa2,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)
from wlan_test_utils import odpm_utils
from wlanix_testing import (
    NL80211_ATTR_IFINDEX,
    NL80211_ATTR_SCAN_FREQUENCIES,
    NL80211_ATTR_SCHED_SCAN_MATCH,
    NL80211_CMD_NEW_SCAN_RESULTS,
    NL80211_CMD_STOP_SCHED_SCAN,
    NL80211_CMD_TRIGGER_SCAN,
    Nl80211MulticastServer,
    SupplicantStaIfaceCallbackServer,
    build_nla,
)
from wlanix_testing.sched_scan_utils import (
    build_match_set,
    build_start_sched_scan_payload,
)

_LOGGER = logging.getLogger(__name__)

# ==============================================================================
# TEST CONFIGURATION & CONSTANTS
# ==============================================================================
# Power thresholds for various tested states
# TODO(b/576709859): These are currently overly safe thresholds to be adjusted in the future.
BUDGET_ACTIVE_IDLE_W = 50.0
BUDGET_SUSPEND_W = 50.0
BUDGET_WLAN_TX_W = 50.0

# Durations for test scenarios and power sampling (in seconds)
DURATION_ACTIVE_IDLE_SEC = 30
DURATION_SUSPEND_SEC = 45
DURATION_WLAN_TX_SEC = 30
NUM_CONTINUOUS_SCANS = 5

# Advanced timings
# Time to wait for wlanix association to complete
DURATION_WLANIX_CONNECT_WAIT_SEC = 5

# ==============================================================================


class OdpmPowerTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """Test suite for validating power states via wlanix and ODPM."""

    wifi_chip_proxy: fidl_wlanix.WifiChipClient
    wlanix_proxy: fidl_wlanix.WlanixClient
    nl80211_proxy: fidl_wlanix.Nl80211Client
    iface_name: str
    wlanix: wlanix_testing.Wlanix
    chip: wlanix_testing.WifiChip
    iface: wlanix_testing.StaIface

    def _set_display_power(self, power_on: bool) -> None:
        """Helper to power the display panel on or off."""
        state = "on" if power_on else "off"
        try:
            self.dut.ffx.run_ssh_cmd(f"display-tweak panel --power {state}")
        except Exception as e:
            _LOGGER.warning(f"Failed to set display power {state}: {e}")

    async def setup_class(self) -> None:
        """Initialize the DUT, Wlanix, and Tracing/ODPM."""
        await super().setup_class()

        if not hasattr(self.dut, "tracing"):
            raise signals.TestError(
                "Tracing affordance is missing; required for ODPM power metrics."
            )
        self.wlanix = wlanix_testing.Wlanix(self.dut)
        self.chip = await self.wlanix.ensure_single_chip()

    async def setup_test(self) -> None:
        await super().setup_test()
        # Ensure the device is fully awake. Turning the display on prevents the device
        # from starting in suspend or unexpectedly suspending mid-test due to inherited
        # screen-off states, which would artificially lower power measurements.
        self._set_display_power(power_on=True)
        await self.chip.destroy_all_sta_ifaces()
        self.iface = await self.chip.create_sta_iface()
        self.iface_name = self.iface.name
        # Enforce power save ON (and suspend mode OFF) as the baseline for this
        # suite, matching the driver default and product behavior.
        (
            await self.iface.supplicant_sta_iface.set_power_save(enable=True)
        ).unwrap()
        self.wlanix_proxy = self.wlanix.proxy
        self.wifi_chip_proxy = self.chip.proxy
        self.nl80211_proxy = self.iface.nl80211.proxy

    async def teardown_class(self) -> None:
        """Global cleanup to restore baseline state."""
        self._set_display_power(power_on=True)
        await super().teardown_class()

    async def teardown_test(self) -> None:
        """Ensure tracing is stopped if a test crashes."""
        if getattr(self, "dut", None) and getattr(self.dut, "tracing", None):
            if self.dut.tracing.is_active():
                await self.dut.tracing.stop()
        await super().teardown_test()

    def _assert_power_budget(
        self, metrics_results: List[Any], budget_w: float
    ) -> None:
        """Helper to extract mean power from ODPM results and assert against budget."""
        for r in metrics_results:
            if getattr(r, "label", None) in ("Power", "PowerAverage_wlan"):
                power_result = r
                break
        else:
            power_result = None
        if not power_result or not power_result.values:
            raise signals.TestFailure("No power data collected from ODPM.")

        avg_power = power_result.values[0]
        _LOGGER.info(f"Calculated Mean Power: {avg_power:.3f} W")
        assert_less(
            avg_power,
            budget_w,
            f"Mean power ({avg_power:.3f}W) exceeded budget ({budget_w}W)",
        )

    async def _connect_via_wlanix(
        self, ssid: str, password: str | None = None
    ) -> None:
        """Helper to establish a connection directly via wlanix (Supplicant API)."""
        _LOGGER.info(f"Connecting to {ssid} via wlanix Supplicant API...")

        client, server = self.dut.fuchsia_controller.channel_create()
        async with SupplicantStaIfaceCallbackServer(client, server) as ctx:
            state_change_queue = ctx.state_change_queue
            self.iface.supplicant_sta_iface.register_callback(
                callback=ctx.callback_channel.take()
            )

            (proxy, server) = self.dut.fuchsia_controller.channel_create()
            self.iface.supplicant_sta_iface.add_network(network=server.take())
            supplicant_sta_network_proxy = (
                fidl_wlanix.SupplicantStaNetworkClient(proxy)
            )

            supplicant_sta_network_proxy.set_ssid(
                ssid=list(ssid.encode("ascii"))
            )

            if password:
                supplicant_sta_network_proxy.set_psk_passphrase(
                    passphrase=list(password.encode("ascii"))
                )

            try:
                (await supplicant_sta_network_proxy.select()).unwrap()
            except AssertionError as e:
                raise signals.TestFailure(
                    f'Failed to connect to "{ssid}"'
                ) from e

            state_change = await state_change_queue.get()
            assert isinstance(
                state_change,
                fidl_wlanix.SupplicantStaIfaceCallbackOnStateChangedRequest,
            ), f"Expected OnStateChanged. Got {state_change!r}"
            assert_equal(
                state_change.new_state,
                fidl_wlanix.StaIfaceCallbackState.COMPLETED,
            )
            assert_true(
                state_change_queue.empty(),
                "Unexpectedly received additional callback messages.",
            )

        _LOGGER.info(f"Successfully connected to {ssid} via wlanix.")

    def _setup_ap_and_get_credentials(self) -> tuple[str, str]:
        """Configure the AP and return (ssid, password)."""
        ssid = AccessPointConfig.random_string(12)
        password = AccessPointConfig.random_string(15)
        security = SecurityWpa2()

        if isinstance(self.openwrt_ap, OpenWrtAP):
            self.openwrt_ap.configure_wifi(
                AccessPointConfig(
                    radios=[
                        RadioConfig(
                            channel=BssChannel(Band.BAND_5G, 36, HtMode(bw=20)),
                            bss_settings=[
                                BssSettings(
                                    ssid=ssid,
                                    password=password,
                                    security=security,
                                )
                            ],
                        )
                    ]
                )
            )
        elif isinstance(self.access_point, AccessPoint):
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=36,
                ssid=ssid,
                security=DeprecatedSecurity(
                    security_mode=ConfigMapper.to_hostapd_security(security),
                    password=password,
                ),
            )
        else:
            raise signals.TestError("Unknown AP type.")
        return ssid, password

    async def test_idle_not_connected_power(self) -> None:
        """Measure power impact when Wi-Fi is on but not connected."""

        self._set_display_power(power_on=False)

        # Start tracing
        await odpm_utils.start_trace(self.dut.tracing)
        _LOGGER.info(
            "ODPM trace started, waiting for steady state power measurement..."
        )

        # Spend enough time in the state to get power ODPM data
        await asyncio.sleep(DURATION_ACTIVE_IDLE_SEC)

        # Stop ODPM trace and process metrics
        results = await odpm_utils.stop_trace_and_save_metrics(
            tracing=self.dut.tracing,
            test_case_path=self.current_test_info.output_path,
            test_name=self.current_test_info.name,
            rail=odpm_utils.ODPM_WLAN_RAIL,
        )

        self._assert_power_budget(results, BUDGET_ACTIVE_IDLE_W)

    async def test_wifi_off_power(self) -> None:
        """Measure power impact when Wi-Fi is completely powered down.

        This tests the absolute minimum power state when the device is awake,
        but the WLAN chip is completely powered off via the Wlanix HAL.
        """
        self._set_display_power(power_on=False)

        # Power down the Wi-Fi chip completely
        try:
            (
                await self.wifi_chip_proxy.remove_sta_iface(
                    iface_name=self.iface_name
                )
            ).unwrap()
        except Exception:
            pass

        proxy, server = self.dut.fuchsia_controller.channel_create()
        self.wlanix_proxy.get_wifi(wifi=server.take())
        wifi_proxy = fidl_wlanix.WifiClient(proxy)
        (await wifi_proxy.stop()).unwrap()

        try:
            # Start tracing
            await odpm_utils.start_trace(self.dut.tracing)
            _LOGGER.info(
                "ODPM trace started, waiting for steady state power measurement..."
            )

            # Spend enough time in the state to get power ODPM data
            await asyncio.sleep(DURATION_ACTIVE_IDLE_SEC)

            # Stop ODPM trace and process metrics
            results = await odpm_utils.stop_trace_and_save_metrics(
                tracing=self.dut.tracing,
                test_case_path=self.current_test_info.output_path,
                test_name=self.current_test_info.name,
                rail=odpm_utils.ODPM_WLAN_RAIL,
            )

            self._assert_power_budget(results, BUDGET_ACTIVE_IDLE_W)
        finally:
            # Restart for teardown
            (await wifi_proxy.start()).unwrap()

    async def test_connected_no_traffic_power(self) -> None:
        """Measure power impact when Wi-Fi is connected but idle (no traffic moving across the
        connection)."""
        if not self.openwrt_ap and not self.access_point:
            raise signals.TestError("No Access Point available in environment.")

        # Connect to the AP using the wlanix Supplicant interface
        ssid, password = self._setup_ap_and_get_credentials()
        await self._connect_via_wlanix(ssid, password)

        await odpm_utils.start_trace(self.dut.tracing)
        _LOGGER.info(
            "ODPM trace started, waiting for steady state power measurement..."
        )

        # Spend enough time connected and idle to gather data.
        await asyncio.sleep(DURATION_ACTIVE_IDLE_SEC)

        # Stop ODPM trace and process metrics
        results = await odpm_utils.stop_trace_and_save_metrics(
            tracing=self.dut.tracing,
            test_case_path=self.current_test_info.output_path,
            test_name=self.current_test_info.name,
            rail=odpm_utils.ODPM_WLAN_RAIL,
        )

        self._assert_power_budget(results, BUDGET_ACTIVE_IDLE_W)

    async def test_active_wlan_tx_traffic_power(self) -> None:
        """Measure power impact of heavy WLAN Tx traffic via wlanix."""

        if not self.openwrt_ap and not self.access_point:
            raise signals.TestError("No Access Point available in environment.")

        # Connect to the AP using the wlanix Supplicant interface
        ssid, password = self._setup_ap_and_get_credentials()
        await self._connect_via_wlanix(ssid, password)

        # Start an iperf3 server on the Host/AP, and an iperf3 client on the DUT
        iperf_server = await self.setup_iperf_server()
        try:
            iperf_server_addr = iperf_server.get_addr()

            # Start capturing ODPM power metrics
            await odpm_utils.start_trace(self.dut.tracing)
            _LOGGER.info(
                "ODPM trace started, waiting for steady state power measurement..."
            )

            # Run iperf3 Tx on the device (async to prevent blocking tracing loop)
            await asyncio.to_thread(
                self.dut.ffx.run_ssh_cmd,
                f"iperf3 -c {iperf_server_addr} -u -b 50M -t {DURATION_WLAN_TX_SEC}",
            )

            # Stop ODPM trace and process metrics
            results = await odpm_utils.stop_trace_and_save_metrics(
                tracing=self.dut.tracing,
                test_case_path=self.current_test_info.output_path,
                test_name=self.current_test_info.name,
                rail=odpm_utils.ODPM_WLAN_RAIL,
            )

            self._assert_power_budget(results, BUDGET_WLAN_TX_W)
        finally:
            iperf_server.stop()

    async def test_connected_suspend_no_traffic_power(self) -> None:
        """Measure power impact of connected (associated) suspend with no traffic."""

        if not self.openwrt_ap and not self.access_point:
            raise signals.TestError("No Access Point available in environment.")

        # Connect to the AP using the wlanix Supplicant interface
        ssid, password = self._setup_ap_and_get_credentials()
        await self._connect_via_wlanix(ssid, password)

        self._set_display_power(power_on=False)

        # Start tracing before suspending. We cannot start the ODPM trace
        # while the system is suspended because the testbed drops USB power to trigger S3.
        await odpm_utils.start_trace(self.dut.tracing)
        _LOGGER.info(
            "ODPM trace started, waiting for steady state power measurement..."
        )

        _LOGGER.info(
            f"Suspending device via USB cutoff for {DURATION_SUSPEND_SEC}s..."
        )

        try:
            await power.suspend_resume(
                self.dut,
                deadline=Deadline.from_timeout(timedelta(minutes=2)),
                base_idle_duration=timedelta(seconds=DURATION_SUSPEND_SEC),
            )
        except Exception as e:
            _LOGGER.error(f"Failed to suspend/resume: {e}")
            raise
        finally:
            # Explicitly kill iperf3 on the device to avoid hanging tests
            try:
                self.dut.ffx.run_ssh_cmd("pkill iperf3")
            except Exception:
                pass

        # Stop ODPM trace and process metrics
        results = await odpm_utils.stop_trace_and_save_metrics(
            tracing=self.dut.tracing,
            test_case_path=self.current_test_info.output_path,
            test_name=self.current_test_info.name,
            rail=odpm_utils.ODPM_WLAN_RAIL,
        )

        self._assert_power_budget(results, BUDGET_SUSPEND_W)

    async def test_connected_suspend_traffic_power(self) -> None:
        """Measure power impact of connected (associated) suspend with active background traffic."""

        if not self.openwrt_ap and not self.access_point:
            raise signals.TestError("No Access Point available in environment.")

        # Connect to the AP using the wlanix Supplicant interface
        ssid, password = self._setup_ap_and_get_credentials()
        await self._connect_via_wlanix(ssid, password)

        self._set_display_power(power_on=False)

        # Start an iperf3 server on the Host/AP
        iperf_server = await self.setup_iperf_server()
        traffic_task: asyncio.Task[Any] | None = None
        try:
            iperf_server_addr = iperf_server.get_addr()

            # Start traffic in the background
            _LOGGER.info("Starting background iperf3 traffic...")
            traffic_duration = DURATION_SUSPEND_SEC + 60
            traffic_task = asyncio.create_task(
                asyncio.to_thread(
                    self.dut.ffx.run_ssh_cmd,
                    f"iperf3 -c {iperf_server_addr} -u -b 50M -t {traffic_duration}",
                )
            )
            # Wait for traffic to start
            await asyncio.sleep(1)

            # Start tracing before suspending. We cannot start the ODPM trace
            # while the system is suspended because the testbed drops USB power to trigger S3.
            await odpm_utils.start_trace(self.dut.tracing)
            _LOGGER.info(
                "ODPM trace started, waiting for steady state power measurement..."
            )

            _LOGGER.info(
                f"Suspending device via USB cutoff for {DURATION_SUSPEND_SEC}s with active traffic..."
            )

            await power.suspend_resume(
                self.dut,
                deadline=Deadline.from_timeout(timedelta(minutes=2)),
                base_idle_duration=timedelta(seconds=DURATION_SUSPEND_SEC),
            )
        except Exception as e:
            _LOGGER.error(f"Failed to suspend/resume: {e}")
            raise
        finally:
            _LOGGER.info("Cleaning up background traffic...")
            if traffic_task is not None and not traffic_task.done():
                traffic_task.cancel()
            try:
                self.dut.ffx.run_ssh_cmd("pkill iperf3")
            except Exception:
                pass
            iperf_server.stop()

        # Stop ODPM trace and process metrics
        results = await odpm_utils.stop_trace_and_save_metrics(
            tracing=self.dut.tracing,
            test_case_path=self.current_test_info.output_path,
            test_name=self.current_test_info.name,
            rail=odpm_utils.ODPM_WLAN_RAIL,
        )

        self._assert_power_budget(results, BUDGET_SUSPEND_W)

    async def test_fw_power_mode_off(self) -> None:
        """Measure power impact when firmware power saving is explicitly
        turned off during a connection.

        Note: `setup_test()` configures power save enabled (matching the driver
        default and product behavior), so `test_connected_no_traffic_power`
        serves as the power-save-ON baseline. Power consumption in this test
        (power save OFF) is expected to be equal to or higher than that
        baseline idle connection test.
        """

        if not self.openwrt_ap and not self.access_point:
            raise signals.TestError("No Access Point available in environment.")

        ssid, password = self._setup_ap_and_get_credentials()
        await self._connect_via_wlanix(ssid, password)

        # Instruct the Wlanix Supplicant to disable Power Save mode
        _LOGGER.info("Disabling FW Power Save via Wlanix...")
        (
            await self.iface.supplicant_sta_iface.set_power_save(enable=False)
        ).unwrap()

        await odpm_utils.start_trace(self.dut.tracing)
        _LOGGER.info(
            "ODPM trace started, waiting for steady state power measurement..."
        )
        await asyncio.sleep(DURATION_ACTIVE_IDLE_SEC)

        results = await odpm_utils.stop_trace_and_save_metrics(
            tracing=self.dut.tracing,
            test_case_path=self.current_test_info.output_path,
            test_name=self.current_test_info.name,
            rail=odpm_utils.ODPM_WLAN_RAIL,
        )

        self._assert_power_budget(results, BUDGET_ACTIVE_IDLE_W)

    async def test_fw_power_mode_suspend(self) -> None:
        """Measure power impact when Android explicitly sets suspend mode
        during a connection (Ultra low power mode).
        """

        if not self.openwrt_ap and not self.access_point:
            raise signals.TestError("No Access Point available in environment.")

        ssid, password = self._setup_ap_and_get_credentials()
        await self._connect_via_wlanix(ssid, password)

        # Instruct the Wlanix Supplicant to enable Android Suspend Mode
        _LOGGER.info("Enabling Android Suspend Mode via Wlanix...")
        (
            await self.iface.supplicant_sta_iface.set_suspend_mode_enabled(
                enable=True
            )
        ).unwrap()

        await odpm_utils.start_trace(self.dut.tracing)
        _LOGGER.info(
            "ODPM trace started, waiting for steady state power measurement..."
        )
        await asyncio.sleep(DURATION_ACTIVE_IDLE_SEC)

        results = await odpm_utils.stop_trace_and_save_metrics(
            tracing=self.dut.tracing,
            test_case_path=self.current_test_info.output_path,
            test_name=self.current_test_info.name,
            rail=odpm_utils.ODPM_WLAN_RAIL,
        )

        self._assert_power_budget(results, BUDGET_SUSPEND_W)

    async def _run_continuous_scans(
        self, num_scans: int = NUM_CONTINUOUS_SCANS
    ) -> None:
        iface_index = await self.iface.get_iface_index()
        payload = [
            NL80211_CMD_TRIGGER_SCAN,
            0x01,
            0x00,
            0x00,
            *list(
                build_nla(NL80211_ATTR_IFINDEX, struct.pack("<I", iface_index))
            ),
        ]

        client, server = self.dut.fuchsia_controller.channel_create()
        async with Nl80211MulticastServer(client, server) as ctx:
            scan_queue = ctx.message_queue
            self.nl80211_proxy.get_multicast(
                group="scan", multicast=ctx.callback_channel.take()
            )

            for i in range(num_scans):
                _LOGGER.info(f"Triggering continuous scan {i+1}/{num_scans}")
                (
                    await self.nl80211_proxy.message(
                        message=fidl_wlanix.Nl80211Message(
                            message=fidl_wlanix.Message(payload=payload)
                        )
                    )
                ).unwrap()

                # Wait for scan results
                try:
                    while True:
                        msg = await asyncio.wait_for(
                            scan_queue.get(), timeout=15.0
                        )
                        if msg.message and msg.message.payload:
                            if (
                                msg.message.payload[0]
                                == NL80211_CMD_NEW_SCAN_RESULTS
                            ):
                                break
                except asyncio.TimeoutError:
                    _LOGGER.warning("Timeout waiting for scan results")
                await asyncio.sleep(1.0)  # Small delay before next scan

    async def test_passive_scan_power(self) -> None:
        """Measure power impact of continuous passive scanning.

        Currently, Wlanix treats all NL80211_CMD_TRIGGER_SCAN commands as
        passive scans by default.
        TODO(b/579560662): Explicitly pass NL80211_SCAN_FLAG_PASSIVE in the
        payload, and create test_active_scan_power, once Wlanix supports
        parsing scan flags.
        """
        self._set_display_power(power_on=False)

        await odpm_utils.start_trace(self.dut.tracing)
        _LOGGER.info(
            "ODPM trace started, waiting for steady state power measurement..."
        )

        await self._run_continuous_scans(num_scans=NUM_CONTINUOUS_SCANS)

        results = await odpm_utils.stop_trace_and_save_metrics(
            tracing=self.dut.tracing,
            test_case_path=self.current_test_info.output_path,
            test_name=self.current_test_info.name,
            rail=odpm_utils.ODPM_WLAN_RAIL,
        )
        self._assert_power_budget(results, BUDGET_ACTIVE_IDLE_W)

    async def test_pno_scan_power(self) -> None:
        """Measure power impact of Preferred Network Offload (Scheduled) scanning."""
        self._set_display_power(power_on=False)

        iface_index = await self.iface.get_iface_index()

        # Simulate an optimized Android PNO scan targeting specific 2.4GHz channels (1, 6, 11)
        frequencies = [2412, 2437, 2462]
        nested_freqs_payload = b""
        for i, freq in enumerate(frequencies):
            nested_freqs_payload += build_nla(i, struct.pack("<I", freq))

        freqs_attr = build_nla(
            NL80211_ATTR_SCAN_FREQUENCIES, nested_freqs_payload
        )
        match_attr = build_nla(
            NL80211_ATTR_SCHED_SCAN_MATCH, build_match_set("dummy_pno_ssid")
        )

        payload = build_start_sched_scan_payload(
            iface_index=iface_index,
            match_attr=match_attr,
            interval_ms=3000,
            freqs_attr=freqs_attr,
        )

        # Start PNO scan
        (
            await self.nl80211_proxy.message(
                message=fidl_wlanix.Nl80211Message(
                    message=fidl_wlanix.Message(payload=payload)
                )
            )
        ).unwrap()

        await odpm_utils.start_trace(self.dut.tracing)
        _LOGGER.info(
            "ODPM trace started, waiting for steady state power measurement..."
        )

        try:
            # Let it idle with PNO running
            await asyncio.sleep(DURATION_ACTIVE_IDLE_SEC)

            results = await odpm_utils.stop_trace_and_save_metrics(
                tracing=self.dut.tracing,
                test_case_path=self.current_test_info.output_path,
                test_name=self.current_test_info.name,
                rail=odpm_utils.ODPM_WLAN_RAIL,
            )
        finally:
            # Stop PNO scan
            stop_payload = [
                NL80211_CMD_STOP_SCHED_SCAN,
                0x01,
                0x00,
                0x00,
                *list(
                    build_nla(
                        NL80211_ATTR_IFINDEX, struct.pack("<I", iface_index)
                    )
                ),
            ]
            (
                await self.nl80211_proxy.message(
                    message=fidl_wlanix.Nl80211Message(
                        message=fidl_wlanix.Message(payload=stop_payload)
                    )
                )
            ).unwrap()

        self._assert_power_budget(results, BUDGET_ACTIVE_IDLE_W)


if __name__ == "__main__":
    test_runner.main()
