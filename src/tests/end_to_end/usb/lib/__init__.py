# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fuchsia USB End-to-End Test Common Library."""

from .dmc_virtual_usb_hub import (
    DmcVirtualUsbPowerHub,
    DmcVirtualUsbResetPowerHub,
)
from .dwc3_irq_metrics import (
    Dwc3IrqSnapshot,
    assert_no_dwc3_unhandled_irq_storm,
    collect_dwc3_irq_metrics,
)
from .fastboot_utils import (
    reboot_to_fastboot_mode,
    recover_to_fuchsia_mode,
)
from .irq_storm import (
    DispatcherSnapshot,
    IrqStormSnapshot,
    assert_no_irq_storm,
    take_irq_storm_snapshot,
)
from .linux_virtual_usb_hub import (
    DEFAULT_BUS_ID_LOOKUP_ATTEMPTS,
    DEFAULT_BUS_ID_LOOKUP_TIMEOUT_SEC,
    LinuxVirtualUsbAuthorizePowerHub,
    LinuxVirtualUsbResetPowerHub,
)
from .sysfs_usb import (
    HostDevice,
    find_host_device,
    find_usb_device_node,
    get_usb_device_configuration,
    get_usb_device_speed,
    get_usb_device_sysfs_path,
    wait_for_usb_device,
)
from .usb_cli_diag import (
    USB_CLI_END_TOKENS,
    find_diag_problems,
    find_stuck_endpoints,
    get_endpoint_bytes,
    get_endpoint_types,
    parse_dwc3_endpoints,
)
from .usb_config import (
    FUNCTION_INTERFACES,
    FUNCTION_PIDS,
    GOOGLE_USB_VID,
    KERNEL_LOG_RE,
    UsbConfigParseError,
    build_config_matrix,
    get_usb_config,
    parse_get_config,
    parse_usb_config_functions,
    read_usb_functions_over_serial,
    set_usb_config,
)
from .usb_power_hub_base_test import UsbPowerHubBaseTest
from .usb_reset import UsbResetCapable, reset_usb_link

__all__ = [
    "DEFAULT_BUS_ID_LOOKUP_ATTEMPTS",
    "DEFAULT_BUS_ID_LOOKUP_TIMEOUT_SEC",
    "DispatcherSnapshot",
    "DmcVirtualUsbPowerHub",
    "DmcVirtualUsbResetPowerHub",
    "Dwc3IrqSnapshot",
    "FUNCTION_INTERFACES",
    "FUNCTION_PIDS",
    "GOOGLE_USB_VID",
    "HostDevice",
    "IrqStormSnapshot",
    "KERNEL_LOG_RE",
    "LinuxVirtualUsbAuthorizePowerHub",
    "LinuxVirtualUsbResetPowerHub",
    "USB_CLI_END_TOKENS",
    "UsbConfigParseError",
    "UsbPowerHubBaseTest",
    "UsbResetCapable",
    "assert_no_dwc3_unhandled_irq_storm",
    "assert_no_irq_storm",
    "build_config_matrix",
    "collect_dwc3_irq_metrics",
    "find_diag_problems",
    "find_host_device",
    "find_stuck_endpoints",
    "find_usb_device_node",
    "get_endpoint_bytes",
    "get_endpoint_types",
    "get_usb_config",
    "get_usb_device_configuration",
    "get_usb_device_speed",
    "get_usb_device_sysfs_path",
    "parse_dwc3_endpoints",
    "parse_get_config",
    "parse_usb_config_functions",
    "read_usb_functions_over_serial",
    "reboot_to_fastboot_mode",
    "recover_to_fuchsia_mode",
    "reset_usb_link",
    "set_usb_config",
    "take_irq_storm_snapshot",
    "wait_for_usb_device",
]
