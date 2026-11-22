# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""FuchsiaDevice abstract base class implementation."""

import contextlib
import dataclasses
import inspect
import json
import logging
import os
import time
from collections.abc import Awaitable, Callable, Iterator
from datetime import datetime
from typing import Any

import fidl_fuchsia_buildinfo as f_buildinfo
import fidl_fuchsia_developer_remotecontrol as fd_remotecontrol
import fidl_fuchsia_diagnostics_types as f_diagnostics_types
import fidl_fuchsia_feedback as f_feedback
import fidl_fuchsia_hardware_power_statecontrol as fhp_statecontrol
import fidl_fuchsia_hwinfo as f_hwinfo
import fidl_fuchsia_io as f_io
import fuchsia_controller_py as fcp
import fuchsia_inspect
from honeydew import affordances_capable, errors
from honeydew.affordances.connectivity.bluetooth.gap import gap
from honeydew.affordances.connectivity.bluetooth.le import le
from honeydew.affordances.connectivity.netstack import netstack
from honeydew.affordances.connectivity.thread import thread
from honeydew.affordances.connectivity.wlan.core import wlan_core
from honeydew.affordances.connectivity.wlan.wlan_policy import wlan_policy
from honeydew.affordances.connectivity.wlan.wlan_policy_ap import wlan_policy_ap
from honeydew.affordances.drivers.battery_manager import battery_manager
from honeydew.affordances.drivers.fake_battery import (
    fake_battery as fake_battery_affordance,
)
from honeydew.affordances.drivers.hardware.power import power as hardware_power
from honeydew.affordances.hello_fuchsia import (
    hello_fuchsia as hello_fuchsia_affordance,
)
from honeydew.affordances.hello_fuchsia import (
    hello_fuchsia_using_fc,
    hello_fuchsia_using_ffx,
)
from honeydew.affordances.hello_fuchsia import types as hello_fuchsia_types
from honeydew.affordances.hello_world import (
    hello_world as hello_world_affordance,
)
from honeydew.affordances.location import location
from honeydew.affordances.media import media as media_affordance
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller as system_power_state_controller_interface,
)
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller_using_starnix,
)
from honeydew.affordances.rtc import rtc as rtc_affordance
from honeydew.affordances.session import session as session_affordance
from honeydew.affordances.starnix import errors as starnix_errors
from honeydew.affordances.starnix import starnix as starnix_affordance
from honeydew.affordances.tracing import tracing as tracing_affordance
from honeydew.affordances.tracing import types as tracing_types
from honeydew.affordances.ui.screenshot import (
    screenshot as screenshot_affordance,
)
from honeydew.affordances.ui.user_input import (
    user_input as user_input_affordance,
)
from honeydew.affordances.usb import errors as usb_errors
from honeydew.affordances.usb import usb as usb_affordance
from honeydew.affordances.virtual_audio import audio
from honeydew.auxiliary_devices.power_switch import (
    power_switch as power_switch_interface,
)
from honeydew.auxiliary_devices.usb_power_hub import (
    usb_power_hub as usb_power_hub_interface,
)
from honeydew.transports.fastboot import fastboot
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.ffx import ffx_impl, ffx_with_monitor
from honeydew.transports.ffx.config import FfxConfigData
from honeydew.transports.fuchsia_controller import errors as fc_errors
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.transports.serial import serial as serial_transport
from honeydew.typing import custom_types
from honeydew.utils import common, properties

_FC_PROXIES: dict[str, custom_types.FidlEndpoint] = {
    "BuildInfo": custom_types.FidlEndpoint(
        "/core/build-info", "fuchsia.buildinfo.Provider"
    ),
    "DeviceInfo": custom_types.FidlEndpoint(
        "/core/hwinfo", "fuchsia.hwinfo.Device"
    ),
    "Feedback": custom_types.FidlEndpoint(
        "/core/feedback", "fuchsia.feedback.DataProvider"
    ),
    "LastRebootInfo": custom_types.FidlEndpoint(
        "/core/feedback", "fuchsia.feedback.LastRebootInfoProvider"
    ),
    "ProductInfo": custom_types.FidlEndpoint(
        "/core/hwinfo", "fuchsia.hwinfo.Product"
    ),
    "PowerAdmin": custom_types.FidlEndpoint(
        "/bootstrap/shutdown_shim", "fuchsia.hardware.power.statecontrol.Admin"
    ),
    "RemoteControl": custom_types.FidlEndpoint(
        "/core/remote-control", "fuchsia.developer.remotecontrol.RemoteControl"
    ),
}

_LOG_SEVERITIES: dict[custom_types.LEVEL, f_diagnostics_types.Severity] = {
    custom_types.LEVEL.INFO: f_diagnostics_types.Severity.INFO,
    custom_types.LEVEL.WARNING: f_diagnostics_types.Severity.WARN,
    custom_types.LEVEL.ERROR: f_diagnostics_types.Severity.ERROR,
}

_CHECK_CONNECTIONS_TIMEOUT_SECS: int = 30
# 30s (ffx) + 30s (fc) + 3 * 30s (adb retries) + 10s buffer.
_HEALTH_CHECK_TIMEOUT_SECS: int = 160
_RESOLVE_DEVICE_ADDRESS_MAX_ATTEMPTS = 3

_LOGGER: logging.Logger = logging.getLogger(__name__)


class FuchsiaDevice(
    affordances_capable.RebootCapableDevice,
    affordances_capable.SuspendResumeCapableDevice,
    affordances_capable.FuchsiaDeviceLogger,
    affordances_capable.FuchsiaDeviceClose,
    affordances_capable.InspectCapableDevice,
    affordances_capable.FuchsiaDeviceAddressChange,
    affordances_capable.HealthCheckCapable,
):
    """Class that provides access to an assortment of capabilities available
    on a Fuchsia device.

    Args:
        device_info: Fuchsia device information.
        ffx_config_data: Config that need to be used while running FFX commands.
        config: Honeydew device configuration, if any.
            Format:
                {
                    "transports": {
                        <transport_name>: {
                            <key>: <value>,
                            ...
                        },
                        ...
                    },
                    "affordances": {
                        <affordance_name>: {
                            <key>: <value>,
                            ...
                        },
                        ...
                    },
                    "auxiliary_devices": {
                        <auxiliary_device_name>: {
                            "hardware": {
                                <key>: <value>,
                                ...
                            },
                            "implementation": {
                                "module": <module_path>,
                                "class": <class_name>,
                            },
                            ...
                        },
                        ...
                    },
                }
            Example:
                {
                    "transports": {
                        "fuchsia_controller": {
                            "timeout": 30,
                        }
                    },
                    "affordances": {
                        "tracing": {
                            "implementation": "ffx",
                        },
                    },
                    "auxiliary_devices": {
                        "power_switch": {
                            "hardware": {
                                "pdu_host": "10.0.0.1",
                                "outlet": 1,
                            },
                            "implementation": {
                                "module": "honeydew.auxiliary_devices.power_switch.power_switch_using_pdu",
                                "class": "PowerSwitchUsingPdu",
                            },
                        },
                    },
                }

    Raises:
        FFXCommandError: if FFX connection check fails.
        FuchsiaControllerError: if FC connection check fails.
    """

    def __init__(
        self,
        *,
        device_info: custom_types.DeviceInfo,
        ffx_config_data: FfxConfigData,
        # intentionally made this a Dict instead of dataclass to minimize the changes in remaining Lacewing stack every time we need to add a new configuration item
        config: dict[str, Any] | None = None,
    ) -> None:
        _LOGGER.debug("Initializing FuchsiaDevice")

        self._device_info: custom_types.DeviceInfo = device_info

        self._ffx_config_data: FfxConfigData = ffx_config_data

        self._on_device_boot_fns: list[
            Callable[[], None] | Callable[[], Awaitable[None]]
        ] = []
        self._on_device_close_fns: list[
            Callable[[], None] | Callable[[], Awaitable[None]]
        ] = []
        self._on_device_address_change_fns: list[
            Callable[[custom_types.TargetAddr], None]
        ] = []
        self._on_device_suspend_fns: list[
            Callable[[], None] | Callable[[], Awaitable[None]]
        ] = []
        self._on_device_resume_fns: list[
            Callable[[], None] | Callable[[], Awaitable[None]]
        ] = []
        self._pre_suspend_boot_id: str | None = None

        self._config: dict[str, Any] | None = config
        self._created_context = False

        self._power_switch = power_switch_interface.PowerSwitch.from_config(
            device_config=self._config,
        )

        self._health_check_leg_statuses: list[str] = []
        self._current_health_check_leg: str | None = None

        self.health_check()

        _LOGGER.debug("Initialized FuchsiaDevice")

    # List all the persistent properties
    @properties.PersistentProperty
    def board(self) -> str:
        """Returns the board value of the device.

        Returns:
            board value of the device.

        Raises:
            FfxCommandError: On failure.
        """
        return self.ffx.get_target_board()

    @properties.PersistentProperty
    def device_name(self) -> str:
        """Returns the name of the device.

        Returns:
            Name of the device.
        """
        return self._device_info.name

    @property
    def is_static_address(self) -> bool:
        """Returns True if the device was created with a statically provided address."""
        return (
            isinstance(self._device_info.address, custom_types.IpPort)
            and common.is_infra()
        )

    async def manufacturer(self) -> str:
        """Returns the manufacturer of the device, cached after the first retrieval.

        Returns:
            Manufacturer of device.

        Raises:
            FuchsiaDeviceError: On failure.
        """
        return (await self._product_info())["manufacturer"]

    async def model(self) -> str:
        """Returns the model of the device, cached after the first retrieval.

        Returns:
            Model of device.

        Raises:
            FuchsiaDeviceError: On failure.
        """
        return (await self._product_info())["model"]

    @properties.PersistentProperty
    def product(self) -> str:
        """Returns the product value of the device.

        Returns:
            product value of the device.

        Raises:
            FfxCommandError: On failure.
        """
        return self.ffx.get_target_product()

    async def product_name(self) -> str:
        """Returns the product name of the device, cached after the first retrieval.

        Returns:
            Product name of the device.

        Raises:
            FuchsiaDeviceError: On failure.
        """
        return (await self._product_info())["name"]

    async def serial_number(self) -> str:
        """Returns the serial number of the device, cached after the first retrieval.

        Returns:
            Serial number of the device.
        """
        return (await self._device_info_from_fidl())["serial_number"]

    # List all the dynamic properties
    async def firmware_version(self) -> str:
        """Returns the firmware version of the device.

        Returns:
            Firmware version of the device.
        """
        return (await self._build_info())["version"]

    async def last_reboot_reason(self) -> str:
        """Returns the last reboot reason of the device.

        Returns:
            Last reboot reason of the device. Empty string if it doesn't exist.
        """
        reason = (await self._last_reboot_info())["reason"]
        if reason is None:
            return ""
        return f_feedback.RebootReason(reason).name

    # List all transports
    @properties.Transport
    def ffx(self) -> ffx_transport.FFX:
        """Returns the FFX transport object.

        Returns:
            FfxImpl or FFXWithMonitor transport instance.

        Raises:
            FfxCommandError: Failed to instantiate.
        """
        use_monitor_state = False
        if self._config is not None:
            # Read monitor state
            config_use_monitor_state = common.read_from_dict(
                self._config,
                key_path=("transports", "ffx", "use_monitor_state"),
                should_exist=False,
            )
            if config_use_monitor_state is not None:
                use_monitor_state = bool(config_use_monitor_state)
        ffx_obj: ffx_transport.FFX
        if use_monitor_state:
            ffx_obj = ffx_with_monitor.FFXWithMonitor(
                target_name=self.device_name,
                config_data=self._ffx_config_data,
                target_addr=self._device_info.address,
                device_address_change=self,
            )
        else:
            ffx_obj = ffx_impl.FfxImpl(
                target_name=self.device_name,
                config_data=self._ffx_config_data,
                target_addr=self._device_info.address,
                device_address_change=self,
            )
        if isinstance(self._device_info.address, custom_types.UsbAddress):
            self._device_info = dataclasses.replace(
                self._device_info,
                address=ffx_obj.get_target_address(),
            )
        return ffx_obj

    @properties.Transport
    def fuchsia_controller(
        self,
    ) -> fc_transport.FuchsiaController:
        """Returns the Fuchsia-Controller transport object.

        Returns:
            Fuchsia-Controller transport interface implementation.

        Raises:
            FuchsiaControllerError: Failed to instantiate.
        """
        fuchsia_controller_obj: fc_transport.FuchsiaController = (
            fc_transport.FuchsiaController(
                target_name=self.device_name,
                ffx_config_data=self._ffx_config_data,
                target_addr=self._device_info.address,
                device_address_change=self,
            )
        )
        return fuchsia_controller_obj

    @properties.Transport
    def fastboot(self) -> fastboot.Fastboot:
        """Returns the Fastboot transport object.

        Returns:
            Fastboot transport interface implementation.

        Raises:
            FuchsiaDeviceError: Failed to instantiate.
        """
        fastboot_node_id: str | None = self._device_info.serial_number
        if fastboot_node_id is None:
            try:
                fastboot_node_id = self.ffx.serial_number
            except errors.HoneydewError as err:
                _LOGGER.debug(
                    "Failed to get serial number from FFX for %s: %s",
                    self.device_name,
                    err,
                )
                fastboot_node_id = None

        if fastboot_node_id is None:
            raise errors.FuchsiaDeviceError(
                f"Failed to get the fastboot node id of '{self.device_name}' as "
                f"'serial_number' was not provided and could not be retrieved "
                f"via FFX"
            )

        fastboot_obj: fastboot.Fastboot = fastboot.Fastboot(
            device_name=self.device_name,
            reboot_affordance=self,
            ffx_transport=self.ffx,
            fastboot_node_id=fastboot_node_id,
        )
        return fastboot_obj

    @properties.Transport
    def serial(self) -> serial_transport.Serial:
        """Returns the Serial transport object.

        Returns:
            Serial transport object.
        """
        if self._device_info.serial_socket is None:
            raise errors.FuchsiaDeviceError(
                "'serial_socket' arg need to be provided during the init to use Serial affordance"
            )

        serial_obj: serial_transport.Serial = serial_transport.Serial(
            device_name=self.device_name,
            socket_path=self._device_info.serial_socket,
        )
        return serial_obj

    # List all the affordances
    @properties.Affordance
    def session(self) -> session_affordance.Session:
        """Returns a session affordance object.

        Returns:
            session.Session object
        """
        return session_affordance.Session(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_device_close=self,
        )

    @properties.Affordance
    def screenshot(self) -> screenshot_affordance.Screenshot:
        """Returns a screenshot affordance object.

        Returns:
            screenshot.Screenshot object
        """
        return screenshot_affordance.Screenshot(self.ffx)

    @properties.Affordance
    def virtual_audio(self) -> audio.VirtualAudio:
        """Returns a virtual audio affordance object.

        Connecting to the protocols this connects to on startup will inject
        the virtual audio device which does the following things:

        - Input audio will only come from the virtual device. Actual microphones are disabled.
        - Output audio will only go to the virtual device. Actual speakers are disabled.

        TODO(https://fxbug.dev/417759272): There is currently no way to disable this
        behavior other than rebooting the device.

        Returns:
            audio.VirtualAudio object
        """
        return audio.VirtualAudio(
            device_name=self.device_name,
            fuchsia_controller=self.fuchsia_controller,
            ffx_transport=self.ffx,
        )

    @properties.Affordance
    def starnix(self) -> starnix_affordance.Starnix:
        """Returns a starnix affordance object.

        Returns:
            starnix.Starnix object
        """
        return starnix_affordance.Starnix(
            device_name=self.device_name,
            ffx=self.ffx,
            inspect=self,
        )

    @properties.Affordance
    def system_power_state_controller(
        self,
    ) -> system_power_state_controller_interface.SystemPowerStateController:
        """Returns a SystemPowerStateController affordance object.

        Returns:
            system_power_state_controller_using_starnix.SystemPowerStateControllerUsingStarnix object

        Raises:
            errors.NotSupportedError: If Fuchsia device does not support Starnix
        """
        return system_power_state_controller_using_starnix.SystemPowerStateControllerUsingStarnix(
            device_name=self.device_name,
            ffx=self.ffx,
            inspect=self,
            device_logger=self,
            starnix_affordance=self.starnix,
        )

    @properties.Affordance
    def rtc(self) -> rtc_affordance.Rtc:
        """Returns an RTC affordance object.

        Returns:
            rtc.Rtc object
        """
        return rtc_affordance.Rtc(
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
        )

    @properties.Affordance
    def tracing(
        self,
    ) -> tracing_affordance.Tracing:
        """Returns a tracing affordance object.

        Returns:
            tracing.Tracing object
        """
        self._get_tracing_affordance_implementation()
        return tracing_affordance.Tracing(
            device_name=self.device_name,
            ffx_inst=self.ffx,
            reboot_affordance=self,
        )

    @properties.Affordance
    def user_input(self) -> user_input_affordance.UserInput:
        """Returns an user input affordance object.

        Returns:
            user_input.UserInput object
        """
        return user_input_affordance.UserInput(
            device_name=self.device_name,
            fuchsia_controller=self.fuchsia_controller,
            ffx_transport=self.ffx,
        )

    @properties.Affordance
    def bluetooth_le(self) -> le.LE:
        """Returns a Bluetooth LE affordance object.

        Returns:
            Bluetooth LE object
        """
        return le.LE(
            device_name=self.device_name,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
        )

    @properties.Affordance
    def bluetooth_gap(self) -> gap.Gap:
        """Returns a Bluetooth Gap affordance object.

        Returns:
            Bluetooth Gap object
        """
        return gap.Gap(
            device_name=self.device_name,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
        )

    @properties.Affordance
    def wlan_policy(self) -> wlan_policy.WlanPolicy:
        """Returns a wlan_policy affordance object.

        Returns:
            wlan_policy.WlanPolicy object
        """
        return wlan_policy.WlanPolicy(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            fuchsia_device_close=self,
            location=self.location,
            suspend_resume_affordance=self,
        )

    @properties.Affordance
    def wlan_policy_ap(self) -> wlan_policy_ap.WlanPolicyAp:
        """Returns a wlan_policy_ap affordance object.

        Returns:
            wlan_policy_ap.WlanPolicyAp object
        """
        return wlan_policy_ap.WlanPolicyAp(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            fuchsia_device_close=self,
            suspend_resume_affordance=self,
        )

    @properties.Affordance
    def wlan_core(self) -> wlan_core.WlanCore:
        """Returns a wlan affordance object.

        Returns:
            wlan.WlanCore object
        """
        return wlan_core.WlanCore(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            fuchsia_device_close=self,
            suspend_resume_affordance=self,
        )

    @properties.Affordance
    def battery(self) -> battery_manager.Battery:
        """Returns a battery affordance object.

        Returns:
            battery_manager.Battery object
        """
        return battery_manager.Battery(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            fuchsia_device_close=self,
        )

    @properties.Affordance
    def battery_manager(self) -> battery_manager.Battery:
        """Returns a battery_manager affordance object.

        Returns:
            battery_manager.Battery object
        """
        return self.battery

    @properties.Affordance
    def fake_battery(self) -> fake_battery_affordance.FakeBattery:
        """Returns a fake_battery affordance object.

        Returns:
            fake_battery.FakeBattery object
        """
        return fake_battery_affordance.FakeBattery(
            device_name=self.device_name,
            ffx=self.ffx,
        )

    @properties.Affordance
    def power_source(self) -> hardware_power.PowerSource:
        """Returns a power_source affordance object.

        Returns:
            hardware_power.PowerSource object
        """
        return hardware_power.PowerSource(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            fuchsia_device_close=self,
        )

    @properties.Affordance
    def netstack(self) -> netstack.Netstack:
        """Returns a netstack affordance object.

        Returns:
            netstack.Netstack object
        """
        return netstack.Netstack(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            suspend_resume_affordance=self,
        )

    @properties.Affordance
    def thread(self) -> thread.Thread:
        """Returns a thread affordance object.

        Returns:
            thread.Thread object
        """
        return thread.Thread(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            fuchsia_device_close=self,
        )

    @properties.Affordance
    def location(self) -> location.Location:
        """Returns a location affordance object.

        Returns:
            location.Location object
        """
        return location.Location(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            suspend_resume_affordance=self,
        )

    @properties.Affordance
    def media(self) -> media_affordance.Media:
        """Returns a media affordance object.

        Returns:
            media.Media object
        """
        return media_affordance.Media(
            device_name=self.device_name,
            fuchsia_controller=self.fuchsia_controller,
            ffx_transport=self.ffx,
        )

    @properties.Affordance
    def hello_world(self) -> hello_world_affordance.HelloWorld:
        """Returns a HelloWorld affordance object.

        Returns:
            hello_world.HelloWorld object
        """
        return hello_world_affordance.HelloWorld(
            device_name=self.device_name,
            ffx=self.ffx,
        )

    @properties.Affordance
    def hello_fuchsia(self) -> hello_fuchsia_affordance.HelloFuchsia:
        """Returns a HelloFuchsia affordance object.

        Returns:
            hello_fuchsia_using_fc.HelloFuchsiaUsingFc |
            hello_fuchsia_using_ffx.HelloFuchsiaUsingFfx object
        """
        impl = self._get_hello_fuchsia_affordance_implementation()
        if impl == hello_fuchsia_types.Implementation.FFX:
            return hello_fuchsia_using_ffx.HelloFuchsiaUsingFfx(
                device_name=self.device_name,
                ffx=self.ffx,
            )
        return hello_fuchsia_using_fc.HelloFuchsiaUsingFc(
            device_name=self.device_name,
            fuchsia_controller=self.fuchsia_controller,
        )

    @properties.Affordance
    def usb(self) -> usb_affordance.USB:
        """Returns a USB affordance object.

        Returns:
            usb.USB object
        """
        serial: serial_transport.Serial | None = (
            self.serial if self._device_info.serial_socket is not None else None
        )
        power_hub = usb_power_hub_interface.UsbPowerHub.from_config(
            device_config=self._config,
        )
        return usb_affordance.USB(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            device_address_change=self,
            health_check=self,
            serial=serial,
            power_hub=power_hub,
        )

    # List all the public methods
    async def close(self) -> None:
        """Clean up method."""
        for fn in self._on_device_close_fns:
            _LOGGER.info("Calling %s", fn.__qualname__)
            res = fn()
            if inspect.isawaitable(res):
                await res

    def health_check(self) -> None:
        """Ensure device is healthy.

        Raises:
            errors.HealthCheckError
        """
        # `health_check()` can be re-entered when an outer health check leg
        # triggers recovery (for example: `health_check()` ->
        # `adb.check_connection()` -> `adb.run()` -> `_recover_adb_server()` ->
        # `usb.after_connect()` -> `health_check()`). Save any in-progress outer
        # leg state on the call stack and restore it in `finally` so the nested
        # `health_check()` does not overwrite the outer call's leg tracking or
        # error attribution.
        prev_leg_statuses: list[str] = self._health_check_leg_statuses
        prev_current_leg: str | None = self._current_health_check_leg
        self._health_check_leg_statuses = []
        self._current_health_check_leg = None
        try:
            with common.time_limit(
                timeout=_HEALTH_CHECK_TIMEOUT_SECS,
                exception_message=f"Timeout occurred during the health check of '{self._device_info.name}'",
            ):
                _LOGGER.info(
                    "Starting the health check on %s...",
                    self.device_name,
                )

                self._check_connections(timeout=_CHECK_CONNECTIONS_TIMEOUT_SECS)

                leg_summary: str = " ".join(self._health_check_leg_statuses)
                _LOGGER.info(
                    "Completed the health check successfully on %s (%s)...",
                    self.device_name,
                    leg_summary,
                )
        except (
            errors.HoneydewTimeoutError,
            errors.TransportConnectionError,
        ) as err:
            try:
                self.usb.get_diagnostic_info()
            except errors.NotSupportedError:
                pass
            except usb_errors.UsbError as usb_err:
                _LOGGER.warning("%s", usb_err)
            err_msg: str = self._format_health_check_error(err)
            _LOGGER.warning("%s", err_msg)
            raise errors.HealthCheckError(err_msg) from err
        finally:
            if prev_current_leg is not None:
                self._health_check_leg_statuses = prev_leg_statuses
                self._current_health_check_leg = prev_current_leg

    def _format_health_check_error(self, err: Exception) -> str:
        """Formats the error message for a failed health check."""
        failed_leg: str = self._current_health_check_leg or "unknown"
        leg_summary: str = " ".join(self._health_check_leg_statuses)
        leg_suffix: str = f" | {leg_summary}" if leg_summary else ""
        return (
            f"health check failed on '{self._device_info.name}': "
            f"[leg={failed_leg}{leg_suffix}] {type(err).__name__}: {err}"
        )

    @contextlib.contextmanager
    def _health_check_leg(self, leg_name: str) -> Iterator[None]:
        """Context manager that runs a single transport health check leg, recording its status and duration.

        To accurately measure elapsed time and attribute any initialization
        errors to the right leg, this is implemented as a context manager so
        lazy `@properties.Transport` access (e.g. `self.ffx`,
        `self.fuchsia_controller`, `self.adb`) happens inside the timed `try`
        block rather than at function argument evaluation time.
        """
        self._current_health_check_leg = leg_name
        start_time: float = time.monotonic()
        try:
            yield
        except errors.NotEnabledError:
            self._current_health_check_leg = None
            self._health_check_leg_statuses.append(f"{leg_name}=skipped")
            raise
        except (
            errors.HoneydewTimeoutError,
            errors.TransportConnectionError,
        ):
            elapsed: float = time.monotonic() - start_time
            self._health_check_leg_statuses.append(
                f"{leg_name}=FAIL({elapsed:.1f}s)"
            )
            raise
        elapsed = time.monotonic() - start_time
        self._current_health_check_leg = None
        self._health_check_leg_statuses.append(f"{leg_name}=ok({elapsed:.1f}s)")

    def _check_connections(self, timeout: int) -> None:
        """Checks health of all device transports."""
        # Note - FFX is invoked first to verify connectivity
        with self._health_check_leg("ffx"):
            self.ffx.check_connection(timeout=timeout)

        with self._health_check_leg("fc"):
            self.fuchsia_controller.check_connection(timeout=timeout)

    def get_inspect_data(
        self,
        selectors: list[str] | None = None,
        monikers: list[str] | None = None,
    ) -> fuchsia_inspect.InspectDataCollection:
        """Return the inspect data associated with the given selectors and
        monikers.

        Args:
            selectors: selectors to be queried.
            monikers: component monikers.

        Note: If both `selectors` and `monikers` lists are empty, inspect data
        for the whole system will be returned.

        Returns:
            Inspect data collection

        Raises:
            InspectError: Failed to return inspect data.
        """
        selectors_and_monikers: list[str] = []
        if selectors:
            selectors_and_monikers += selectors
        if monikers:
            for moniker in monikers:
                selectors_and_monikers.append(moniker.replace(":", r"\:"))

        cmd: list[str] = [
            "--machine",
            "json",
            "inspect",
            "show",
        ] + selectors_and_monikers

        try:
            message: str = (
                f"Collecting the inspect data from {self.device_name}"
            )
            if selectors:
                message = f"{message}, with selectors={selectors}"
            if monikers:
                message = f"{message}, with monikers={monikers}"
            _LOGGER.info(message)
            inspect_data_json_str: str = self.ffx.run(
                cmd=cmd,
                log_output=False,
            )
            _LOGGER.info(
                "Collected the inspect data from %s.", self.device_name
            )

            inspect_data_json_obj: list[dict[str, Any]] = json.loads(
                inspect_data_json_str
            )
            return fuchsia_inspect.InspectDataCollection.from_list(
                inspect_data_json_obj
            )
        except (
            ffx_errors.FfxCommandError,
            errors.DeviceNotConnectedError,
            fuchsia_inspect.InspectDataError,
        ) as err:
            raise errors.InspectError(
                f"Failed to collect the inspect data from {self.device_name}"
            ) from err

    async def log_message_to_device(
        self, message: str, level: custom_types.LEVEL
    ) -> None:
        """Log message to fuchsia device at specified level.

        Args:
            message: Message that need to logged.
            level: Log message level.

        Raises:
            FuchsiaControllerError: On communications failure.
        """
        timestamp: str = datetime.now().strftime("%Y-%m-%d-%I-%M-%S-%p")
        message = f"[Host Time: {timestamp}] - {message}"
        await self._send_log_command(
            tag="lacewing", message=message, level=level
        )

    async def on_device_boot(self) -> None:
        """Take actions after the device is rebooted.

        Raises:
            FuchsiaControllerError: On communications failure.
        """
        # Create a new Fuchsia controller context for new device connection.
        self.fuchsia_controller.create_context()

        # Ensure device is healthy
        self.health_check()

        for fn in self._on_device_boot_fns:
            _LOGGER.info("Calling %s", fn.__qualname__)
            res = fn()
            if inspect.isawaitable(res):
                await res

    @property
    def power_switch(self) -> power_switch_interface.PowerSwitch | None:
        """Power switch controlling this device, if one has been set."""
        return self._power_switch

    def set_power_switch(
        self,
        power_switch: power_switch_interface.PowerSwitch,
    ) -> None:
        """Set power switch for device.

        Note: This is normally populated automatically during `__init__()` via
        `PowerSwitch.from_config()` and does not need to be called manually.
        Call this only to override the auto-configured implementation or inject a
        custom/test instance at runtime.

        Args:
            power_switch: Implementation of PowerSwitch interface.
        """
        self._power_switch = power_switch

    async def power_cycle(self) -> None:
        """Power cycle (power off, wait for delay, power on) the device.

        Requires a configured power switch (populated automatically from device
        config or set via `set_power_switch()`).

        Raises:
            NotSupportedError: If power switch is not set.
            FuchsiaControllerError: On communications failure.
        """
        if self._power_switch is None:
            raise errors.NotSupportedError(
                f"Cannot power cycle '{self.device_name}': Power switch is not configured. "
                "Either call `set_power_switch()` or ensure power switch details are provided in the testbed config."
            )

        _LOGGER.info("Power cycling %s...", self.device_name)

        try:
            await self.log_message_to_device(
                message=f"Powering cycling {self.device_name}...",
                level=custom_types.LEVEL.INFO,
            )
        except errors.FIDL_ERRORS + (RuntimeError, errors.HoneydewError):
            # power_cycle can be used as a recovery mechanism when device is
            # unhealthy. So any calls to device prior to power_cycle can
            # fail in such cases and thus ignore them.
            # `RuntimeError` is raised by the native Fuchsia-Controller
            # bindings for internal errors, e.g. from
            # `Context.connect_remote_control_proxy()`. See `_CONTROLLER_ERRORS`
            # in //src/testing/end_to_end/honeydew/honeydew/transports/fuchsia_controller/fuchsia_controller.py.
            pass

        _LOGGER.info("Powering off %s...", self.device_name)
        self.ffx.notify_intentional_disconnect()
        self._power_switch.power_off()
        self.wait_for_offline()

        _LOGGER.info("Powering on %s...", self.device_name)
        self._power_switch.power_on()
        await self.wait_for_online()

        await self.on_device_boot()

        await self.log_message_to_device(
            message=f"Successfully power cycled {self.device_name}...",
            level=custom_types.LEVEL.INFO,
        )

    # TODO(b/582408121): Move to system_power_state_controller affordance and
    # create system_power_state_controller_using_usb for current suspend-resume
    # implementation in FuchsiaDevice.
    def register_on_device_suspend_fn(
        self,
        fn: Callable[[], None] | Callable[[], Awaitable[None]],
    ) -> None:
        """Register a function to be called when device is suspended.

        Args:
            fn: Function to be called when device is suspended.
        """
        self._on_device_suspend_fns.append(fn)

    # TODO(b/582408121): Move to system_power_state_controller affordance and
    # create system_power_state_controller_using_usb for current suspend-resume
    # implementation in FuchsiaDevice.
    def register_on_device_resume_fn(
        self,
        fn: Callable[[], None] | Callable[[], Awaitable[None]],
    ) -> None:
        """Register a function to be called when device is resumed.

        Args:
            fn: Function to be called when device is resumed.
        """
        self._on_device_resume_fns.append(fn)

    # TODO(b/582408121): Move to system_power_state_controller affordance and
    # create system_power_state_controller_using_usb for current suspend-resume
    # implementation in FuchsiaDevice.
    async def suspend(self) -> None:
        """Suspend the device by disconnecting USB power.

        Requires a configured USB power hub (populated automatically on
        `FuchsiaDevice.usb` from device config or set via
        `usb.set_power_hub()`). This will run all registered
        on_device_suspend_fns before disconnecting USB power. Note that this does
        not guarantee the device actually suspends, just that it will have the
        opportunity to.

        Raises:
            NotSupportedError: If USB power hub not set.
        """
        for fn in self._on_device_suspend_fns:
            res = fn()
            if inspect.isawaitable(res):
                await res

        self._pre_suspend_boot_id = await self.boot_id()
        self.usb.disconnect()

    # TODO(b/582408121): Move to system_power_state_controller affordance and
    # create system_power_state_controller_using_usb for current suspend-resume
    # implementation in FuchsiaDevice.
    async def resume(self) -> None:
        """Resume the device by reconnecting USB power.

        Requires a configured USB power hub (populated automatically on
        `FuchsiaDevice.usb` from device config or set via
        `usb.set_power_hub()`). This will run all registered
        on_device_resume_fns after reconnecting USB power.

        Raises:
            NotSupportedError: If USB power hub not set.
            FuchsiaDeviceError: If unexpected reboot detected.
        """
        self.usb.connect()

        post_resume_boot_id = await self.boot_id()
        if self._pre_suspend_boot_id != post_resume_boot_id:
            raise errors.FuchsiaDeviceError(
                f"Unexpected reboot detected for {self.device_name}. Boot ID {self._pre_suspend_boot_id} != {post_resume_boot_id}"
            )

        for fn in self._on_device_resume_fns:
            res = fn()
            if inspect.isawaitable(res):
                await res

    async def reboot(self) -> None:
        """Soft reboot the device.

        Raises:
            FuchsiaControllerError: On communications failure.
            FuchsiaDeviceError: If device fails to go offline.
        """
        _LOGGER.info("Lacewing is rebooting %s...", self.device_name)
        await self.log_message_to_device(
            message=f"Rebooting {self.device_name}...",
            level=custom_types.LEVEL.INFO,
        )

        self.ffx.notify_intentional_disconnect()
        await self._send_reboot_command()
        self.wait_for_offline()
        await self.wait_for_online()
        await self.on_device_boot()

        await self.log_message_to_device(
            message=f"Successfully rebooted {self.device_name}...",
            level=custom_types.LEVEL.INFO,
        )

    def register_for_on_device_boot(
        self, fn: Callable[[], None] | Callable[[], Awaitable[None]]
    ) -> None:
        """Register a function that will be called in `on_device_boot()`.

        Args:
            fn: Function that need to be called after FuchsiaDevice boot up.
        """
        self._on_device_boot_fns.append(fn)

    def register_for_on_device_close(
        self, fn: Callable[[], None] | Callable[[], Awaitable[None]]
    ) -> None:
        """Register a function that will be called during device clean up in `close()`.

        Args:
            fn: Function that need to be called during FuchsiaDevice cleanup.
        """
        self._on_device_close_fns.append(fn)

    # TODO(b/582408121): Kept synchronous so `USB.after_connect()` can call
    # `resolve_device_address()` synchronously (required while `Adb` is
    # synchronous and calls `USB` methods from `Adb.__init__()`). All registered
    # callbacks (`FFX.update_target_address` and
    # `FuchsiaController.update_target_address`) are synchronous. If `Adb` and
    # `USB` are later converted to async, revisit whether this should also
    # support async callbacks.
    def resolve_device_address(self) -> None:
        """Resolves the target address of Fuchsia device."""
        # Step #1 - Get the target address of the Fuchsia device.
        if self._device_info.address is not None:
            _LOGGER.info(
                "'%s' is the target address of '%s', prior to resolving device address",
                self._device_info.address,
                self.device_name,
            )
        try:
            target_addr: custom_types.TargetAddr = self.ffx.get_target_address(
                refresh=True
            )

            self._device_info = dataclasses.replace(
                self._device_info,
                address=target_addr,
            )
            _LOGGER.info(
                "'%s' is the target address of '%s', after resolving device address",
                self._device_info.address,
                self.device_name,
            )

            # Step #2 - Call all of the callback functions that were registered for device address change.
            for fn in self._on_device_address_change_fns:
                _LOGGER.info(
                    "Calling %s with arg %s",
                    fn.__qualname__,
                    target_addr,
                )
                fn(target_addr)
        except (errors.HoneydewError, ValueError) as err:
            raise errors.FuchsiaDeviceError(
                f"Failed to resolve address for '{self.device_name}'"
            ) from err

        # Step #3 - Ensure device is healthy
        self.health_check()

    def register_for_on_device_address_change(
        self,
        fn: Callable[[custom_types.TargetAddr], None],
    ) -> None:
        """Register a function that will be called when a device address is changed.

        Args:
            fn: Function that need to be called when a device address is changed.
        """
        self._on_device_address_change_fns.append(fn)

    async def snapshot(
        self,
        directory: str,
        snapshot_file: str | None = None,
    ) -> str:
        """Captures the snapshot of the device.

        Args:
            directory: Absolute path on the host where snapshot file will be
                saved. If this directory does not exist, this method will create
                it.

            snapshot_file: Name of the output snapshot file.
                If not provided, API will create a name using
                "Snapshot_{device_name}_{'%Y-%m-%d-%I-%M-%S-%p'}" format.

        Returns:
            Absolute path of the snapshot file.

        Raises:
            FuchsiaControllerError: On communications failure.
        """
        _LOGGER.info("Collecting snapshot on %s...", self.device_name)
        # Take the snapshot before creating the directory or file, as
        # _send_snapshot_command may raise an exception.
        snapshot_bytes: bytes = await self._send_snapshot_command()

        directory = os.path.abspath(directory)
        try:
            os.makedirs(directory)
        except FileExistsError:
            pass

        if not snapshot_file:
            timestamp: str = datetime.now().strftime("%Y-%m-%d-%I-%M-%S-%p")
            snapshot_file = f"Snapshot_{self.device_name}_{timestamp}.zip"
        snapshot_file_path: str = os.path.join(directory, snapshot_file)

        with open(snapshot_file_path, "wb") as snapshot_binary_zip:
            snapshot_binary_zip.write(snapshot_bytes)

        _LOGGER.info("Snapshot file has been saved @ '%s'", snapshot_file_path)
        return snapshot_file_path

    def wait_for_offline(self, timeout: float | None = None) -> None:
        """Wait for Fuchsia device to go offline.

        Args:
            timeout: Maximum time in seconds to wait for the device to go
                offline. If None (default), waits indefinitely.

        Raises:
            errors.FuchsiaDeviceError: If device is not offline.
        """
        _LOGGER.info("Waiting for %s to go offline...", self.device_name)
        try:
            self.ffx.wait_for_rcs_disconnection(timeout=timeout)
            _LOGGER.info("%s is offline.", self.device_name)
        except (
            errors.DeviceNotConnectedError,
            ffx_errors.FfxCommandError,
            ffx_errors.FfxTimeoutError,
        ) as err:
            raise errors.FuchsiaDeviceError(
                f"'{self.device_name}' failed to go offline."
            ) from err

    async def wait_for_online(self) -> None:
        """Wait for Fuchsia device to go online.

        Raises:
            errors.FuchsiaDeviceError: If device is not online.
        """
        _LOGGER.info("Waiting for %s to go online...", self.device_name)
        try:
            if self.is_static_address:
                # If the device was specified as an address,
                # just continue to use that address. This is the
                # norm when run by builders in Infra, because they
                # specify a static IP address so the address is
                # expected not to change.
                self.ffx.wait_for_rcs_connection(include_target_name=False)
            else:
                # If the device was not specified as an address, it may change
                # addresses, so first wait for it by name, then update the
                # address. This is the appropriate way when run locally or over
                # USB, where the device may have a new address after reboots.
                # For USB targets, retry if the device goes offline between
                # `wait_for_rcs_connection` and `resolve_device_address` (e.g.
                # if `wait_for_rcs_connection` observed a dying pre-reboot
                # instance during composite USB teardown).
                max_attempts = (
                    _RESOLVE_DEVICE_ADDRESS_MAX_ATTEMPTS
                    if isinstance(
                        self._device_info.address, custom_types.UsbAddress
                    )
                    else 1
                )
                for attempt in range(1, max_attempts + 1):
                    self.ffx.wait_for_rcs_connection(include_target_name=True)
                    try:
                        self.resolve_device_address()
                        break
                    except errors.FuchsiaDeviceError as err:
                        if attempt == max_attempts:
                            raise errors.FuchsiaDeviceError(
                                f"'{self.device_name}' failed to go online."
                            ) from err
                        _LOGGER.warning(
                            "Failed to resolve address for %s on attempt %d/%d "
                            "(device may still be shutting down or "
                            "re-enumerating); retrying: %s",
                            self.device_name,
                            attempt,
                            max_attempts,
                            err,
                        )
            _LOGGER.info("%s is online.", self.device_name)
        except (
            errors.DeviceNotConnectedError,
            ffx_errors.FfxCommandError,
        ) as err:
            raise errors.FuchsiaDeviceError(
                f"'{self.device_name}' failed to go online."
            ) from err

    def is_starnix_device(self) -> bool:
        """Check if the device under test is a starnix device.

        Some operation maybe heavy on starnix device, allow caller to find if running on starnix
        device.

        Raises:
            FuchsiaDeviceError: failed to check the device.
        """

        try:
            _LOGGER.info("%s is a starnix device", self.starnix)
            return True
        except errors.NotSupportedError:
            return False
        except starnix_errors.StarnixError as err:
            raise errors.FuchsiaDeviceError(err)

    # List all private properties
    async def _build_info(self) -> f_buildinfo.BuildInfo:
        """Returns the build information of the device.

        Returns:
            Build info dict.

        Raises:
            FuchsiaControllerError: On FIDL communication failure.
        """
        try:
            buildinfo_provider_proxy = f_buildinfo.ProviderClient(
                self.fuchsia_controller.connect_device_proxy(
                    _FC_PROXIES["BuildInfo"]
                )
            )
            build_info_resp = await buildinfo_provider_proxy.get_build_info()
            return build_info_resp.build_info
        except fcp.FcTransportStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller FIDL Error"
            ) from status

    @properties.persistent_method
    async def _device_info_from_fidl(self) -> f_hwinfo.DeviceInfo:
        """Returns the device information of the device.

        Returns:
            Device info dict.

        Raises:
            FuchsiaControllerError: On FIDL communication failure.
        """
        try:
            hwinfo_device_proxy = f_hwinfo.DeviceClient(
                self.fuchsia_controller.connect_device_proxy(
                    _FC_PROXIES["DeviceInfo"]
                )
            )
            device_info_resp = await hwinfo_device_proxy.get_info()
            return device_info_resp.info
        except fcp.FcTransportStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller FIDL Error"
            ) from status

    @properties.persistent_method
    async def _product_info(self) -> f_hwinfo.ProductInfo:
        """Returns the product information of the device.

        Returns:
            Product info dict.

        Raises:
            FuchsiaControllerError: On FIDL communication failure.
        """
        try:
            hwinfo_product_proxy = f_hwinfo.ProductClient(
                self.fuchsia_controller.connect_device_proxy(
                    _FC_PROXIES["ProductInfo"]
                )
            )
            product_info_resp = await hwinfo_product_proxy.get_info()
            return product_info_resp.info
        except fcp.FcTransportStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller FIDL Error"
            ) from status

    async def boot_id(self) -> str:
        """Gets the boot id from a device.

        Returns:
            The boot id string.

        Raises:
            errors.FuchsiaDeviceError: On failure to get boot ID.
        """
        try:
            rcs_proxy = fd_remotecontrol.RemoteControlClient(
                self.fuchsia_controller.ctx.connect_remote_control_proxy()
            )
            resp = (await rcs_proxy.identify_host()).unwrap()

            if resp.response.boot_id is not None:
                return str(resp.response.boot_id)

            raise errors.FuchsiaDeviceError(
                f"Boot ID not populated in IdentifyHost response for {self.device_name}"
            )
        except errors.FIDL_ERRORS + (
            RuntimeError,
            AssertionError,
            errors.HoneydewError,
        ) as err:
            # `RuntimeError`: internal errors from the native Fuchsia-Controller
            # bindings, see `_CONTROLLER_ERRORS` in
            # //src/testing/end_to_end/honeydew/honeydew/transports/fuchsia_controller/fuchsia_controller.py.
            # `AssertionError`: raised by `unwrap()` on a FIDL result that holds
            # an error (//tools/fidl/fidlgen_python/codegen/union.tmpl).
            raise errors.FuchsiaDeviceError(
                f"Failed to get boot ID from {self.device_name}"
            ) from err

    async def _last_reboot_info(self) -> f_feedback.LastReboot:
        """Gets the last reboot reason from a device.

        Returns:
            The last reboot info dictionary.

        Raises:
            FuchsiaControllerError: On FIDL communication failure or on
              data transfer verification failure.
        """
        try:
            proxy = f_feedback.LastRebootInfoProviderClient(
                self.fuchsia_controller.connect_device_proxy(
                    _FC_PROXIES["LastRebootInfo"]
                )
            )
            resp = await proxy.get()
            return resp.last_reboot
        except fcp.FcTransportStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "_last_reboot_info() failed"
            ) from status

    # List all private methods
    async def _send_log_command(
        self, tag: str, message: str, level: custom_types.LEVEL
    ) -> None:
        """Send a device command to write to the syslog.

        Args:
            tag: Tag to apply to the message in the syslog.
            message: Message that need to logged.
            level: Log message level.

        Raises:
            FuchsiaControllerError: On FIDL communication failure.
        """
        _LOGGER.debug(
            "Attempting to log to device %s: %s, message: %s, level: %s",
            self.device_name,
            tag,
            message,
            level,
        )
        try:
            rcs_proxy = fd_remotecontrol.RemoteControlClient(
                self.fuchsia_controller.ctx.connect_remote_control_proxy()
            )
            await rcs_proxy.log_message(
                tag=tag, message=message, severity=_LOG_SEVERITIES[level]
            )
        except fcp.FcTransportStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller FIDL Error"
            ) from status

    async def _send_reboot_command(self) -> None:
        """Send a device command to trigger a soft reboot.

        Raises:
            FuchsiaControllerError: On FIDL communication failure.
        """
        try:
            power_proxy = fhp_statecontrol.AdminClient(
                self.fuchsia_controller.connect_device_proxy(
                    _FC_PROXIES["PowerAdmin"]
                )
            )
            (
                await power_proxy.shutdown(
                    options=fhp_statecontrol.ShutdownOptions(
                        action=fhp_statecontrol.ShutdownAction.REBOOT,
                        reasons=[
                            fhp_statecontrol.ShutdownReason.DEVELOPER_REQUEST
                        ],
                    ),
                )
            )
        except fcp.FcTransportStatus as status:
            # We can't reliably get a message back from the device when
            # requesting shutdown, so we have to check for whether we receive a
            # communication error. This can be represented by either
            # FC_ERR_FDOMAIN which denotes an error with writing/reading from
            # the underlying channel, or FC_ERR_TRANSPORT which is when the
            # whole transport layer fails to communicate with the device. There
            # is a race regarding which can happen first and it is
            # non-deterministic
            fc_transport_status: int | None = (
                status.args[0] if len(status.args) > 0 else None
            )
            if (
                fc_transport_status != fcp.FcTransportStatus.FC_ERR_FDOMAIN
                and fc_transport_status
                != fcp.FcTransportStatus.FC_ERR_TRANSPORT
            ):
                raise fc_errors.FuchsiaControllerError(
                    "Fuchsia Controller FIDL Error"
                ) from status

    async def _read_snapshot_from_channel(
        self, channel_client: fcp.Channel
    ) -> bytes:
        """Read snapshot data from client end of the transfer channel.

        Args:
            channel_client: Client end of the snapshot data channel.

        Raises:
            FuchsiaControllerError: On FIDL communication failure or on
              data transfer verification failure.

        Returns:
            Bytes containing snapshot data as a zip archive.
        """
        # Snapshot is sent over the channel as |fuchsia.io.File|.
        file_proxy = f_io.FileClient(channel_client)

        # Get file size for verification later.
        try:
            attr_resp: f_io.NodeAttributes2 = (
                await file_proxy.get_attributes(
                    query=f_io.NodeAttributesQuery.CONTENT_SIZE
                )
            ).unwrap()
        except (AssertionError, fcp.ZxStatus, fcp.FcTransportStatus) as e:
            # The above `fcp.ZxStatus` is for the possible error returned by
            # calling `unwrap()` on the result of the function call, NOT for
            # the underlying protocol.
            raise fc_errors.FuchsiaControllerError(
                "get_attributes() failed"
            ) from e
        if attr_resp.immutable_attributes.content_size is None:
            raise fc_errors.FuchsiaControllerError(
                "get_attributes() returned empty content size"
            )
        expected_size: int = attr_resp.immutable_attributes.content_size

        # Read until channel is empty.
        ret: bytearray = bytearray()
        try:
            while True:
                response = (await file_proxy.read(count=f_io.MAX_BUF)).unwrap()
                if not response.data:
                    break
                ret.extend(response.data)
        except (AssertionError, fcp.FcTransportStatus, fcp.ZxStatus) as e:
            raise fc_errors.FuchsiaControllerError("read() failed") from e

        # Verify transfer.
        if len(ret) != expected_size:
            raise fc_errors.FuchsiaControllerError(
                f"Expected {expected_size} bytes, but read {len(ret)} bytes"
            )

        return bytes(ret)

    async def _send_snapshot_command(self) -> bytes:
        """Send a device command to take a snapshot.

        Raises:
            FuchsiaControllerError: On FIDL communication failure or on
              data transfer verification failure.

        Returns:
            Bytes containing snapshot data as a zip archive.
        """

        (
            channel_server,
            channel_client,
        ) = self.fuchsia_controller.channel_create()
        params = f_feedback.GetSnapshotParameters(
            # Set timeout to 2 minutes in nanoseconds.
            collection_timeout_per_data=2 * 60 * 10**9,
            response_channel=channel_server.take(),
        )

        try:
            feedback_proxy = f_feedback.DataProviderClient(
                self.fuchsia_controller.connect_device_proxy(
                    _FC_PROXIES["Feedback"]
                )
            )
            # The data channel isn't populated until get_snapshot() returns so
            # there's no need to drain the channel in parallel.
            (await feedback_proxy.get_snapshot(params=params))
        except fcp.FcTransportStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "get_snapshot() failed"
            ) from status
        return await self._read_snapshot_from_channel(channel_client)

    def _get_tracing_affordance_implementation(
        self,
        should_exist: bool = False,
    ) -> tracing_types.Implementation:
        """Parses the tracing affordance config and returns which implementation to use."""
        if self._config is None:
            return tracing_types.Implementation.FFX

        tracing_affordance_implementation: str | None = common.read_from_dict(
            self._config,
            key_path=("affordances", "tracing", "implementation"),
            should_exist=should_exist,
        )
        if tracing_affordance_implementation is None:
            return tracing_types.Implementation.FFX

        try:
            return tracing_types.Implementation(
                tracing_affordance_implementation
            )
        except ValueError as err:
            raise errors.ConfigError(
                f"Invalid value passed in config['affordances']['tracing']['implementation']. "
                f"Valid values are: {list(map(str, tracing_types.Implementation))}"
            ) from err

    def _get_hello_fuchsia_affordance_implementation(
        self,
        should_exist: bool = False,
    ) -> hello_fuchsia_types.Implementation:
        """Parses the hello_fuchsia affordance config and returns which implementation to use."""
        if self._config is None:
            return hello_fuchsia_types.Implementation.FFX

        hello_fuchsia_affordance_implementation: str | None = (
            common.read_from_dict(
                self._config,
                key_path=("affordances", "hello_fuchsia", "implementation"),
                should_exist=should_exist,
            )
        )
        if hello_fuchsia_affordance_implementation is None:
            return hello_fuchsia_types.Implementation.FFX

        try:
            return hello_fuchsia_types.Implementation(
                hello_fuchsia_affordance_implementation
            )
        except ValueError as err:
            raise errors.ConfigError(
                f"Invalid value passed in config['affordances']['hello_fuchsia']['implementation']. "
                f"Valid values are: {list(map(str, hello_fuchsia_types.Implementation))}"
            ) from err
