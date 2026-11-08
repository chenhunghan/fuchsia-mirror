# PCI

The PCI bus driver provides management of the PCI bus in applicable systems and allows device drivers to be written for and bind to PCI devices in the system.

# Architecture

## Source

### Location

* [`//src/devices/pci/drivers/pci`](https://cs.opensource.google/fuchsia/fuchsia/+/main:src/devices/pci/drivers/pci/)

### Organization

Bus level operations are handled in `bus.cc`. Device-specific operations and the `fuchsia.hardware.pci/Device` FIDL protocol implementation are in `device.cc` and `device_*.cc`. The interface between a Device and the Bus is the `BusDeviceInterface` specified in `bus_device_interface.h`. The `fuchsia.hardware.pci/Debug` protocol (served through `fuchsia.hardware.pci/DebugService`) used by `lspci` is implemented in `device_service.cc`.

### Build Targets

* [`//src/devices/pci/drivers/pci:bus-pci`](https://cs.opensource.google/fuchsia/fuchsia/+/main:src/devices/pci/drivers/pci/BUILD.gn;l=107) for the driver.

## FIDL

### Dependent Protocols

The PCI bus driver binds to [`fuchsia.hardware.pci.root/RootService`](https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/fidl/fuchsia.hardware.pci.root/pciroot.fidl) (and uses `fuchsia.hardware.pci.root/ConfigService` when offered) to receive platform-level information for ACPI or devicetree devices, interrupts, interrupt routing, address space allocation, and bus configuration. On boards whose platform bus driver still serves the legacy banjo [`fuchsia.hardware.pciroot/Pciroot`](https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/banjo/fuchsia.hardware.pciroot/pciroot.fidl) protocol, `pciroot-banjo-adapter` bridges it to `fuchsia.hardware.pci.root`.

### Implemented Protocols

* [`//fuchsia/sdk/fidl/fuchsia.hardware.pci/pci.fidl`](https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/fidl/fuchsia.hardware.pci/pci.fidl)
  * `fuchsia.hardware.pci/Device`: Provides the interface to develop PCI device drivers. Each PCI function's child node is offered one `fuchsia.hardware.pci/Service` instance serving it.
* [`//fuchsia/sdk/fidl/fuchsia.hardware.pci/debug.fidl`](https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/fidl/fuchsia.hardware.pci/debug.fidl)
  * `fuchsia.hardware.pci/Debug`: Exports bus and device state for use with `lspci`, served through `fuchsia.hardware.pci/DebugService`

## Libraries

* [`//src/devices/pci/lib/device-protocol-pci:device-protocol-pci`](https://cs.opensource.google/fuchsia/fuchsia/+/main:src/devices/pci/lib/device-protocol-pci/BUILD.gn;l=7)
  * Provides helper methods `MapMmio` and `ConfigureInterruptMode` for use with drivers, as well as convenience methods for constructing a client end for `fuchsia.hardware.pci/Device` calls.
* [`//src/devices/pci/lib/pci:pci`](https://cs.opensource.google/fuchsia/fuchsia/+/main:src/devices/pci/lib/pci/BUILD.gn;l=9)
  * Provides base implementations for the `PciRootHost` interface which provides a way for a platform to implement a backing driver that can service multiple `fuchsia.hardware.pciroot/Pciroot` instances to support multiple PCI buses in the system.
* [`//src/devices/pci/testing:pci-protocol-fake`](https://cs.opensource.google/fuchsia/fuchsia/+/main:src/devices/pci/testing/BUILD.gn;l=14)
  * Provides a fake `fuchsia.hardware.pci/Device` implementation for use in unit testing, allowing users to create fake PCI devices that match the configuration of their real hardware.

# Testing

See [`//src/devices/pci/drivers/pci/test/README.md`](drivers/pci/test/README.md) for detailed documentation of the PCI test suite.

## Tests

* [`//src/devices/pci/drivers/pci:tests`](https://cs.opensource.google/fuchsia/fuchsia/+/main:src/devices/pci/drivers/pci/BUILD.gn;l=122)
  * `pci-driver-test`: Rust integration test suite in `drivers/pci/test/driver/driver_tests.rs` running against `fake-bus-pci` (build target `pci_fake`) inside `DriverTestRealm`.
  * `pci-unit-test`: C++ unit test suite in `drivers/pci/test/unit/` testing allocation, bus, config, device, and MSI-X against fakes.
* [`//src/devices/pci:tests`](https://cs.opensource.google/fuchsia/fuchsia/+/main:src/devices/pci/BUILD.gn;l=14)
  * `device-protocol-pci-test`
  * `pci-protocol-fake-test`
  * `pci-roothost-test`
  * `pciroot-banjo-adapter-test`

## Fakes

Fakes exist for most major classes in the PCI bus driver and can be found in [`//src/devices/pci/drivers/pci/test/fakes/`](https://cs.opensource.google/fuchsia/fuchsia/+/main:src/devices/pci/drivers/pci/test/fakes/)

# Debugging

## Driver nodes

Each PCI function is published as a child node of the bus node, named `<bus>_<device>_<function>` in hex (for example `00_1f_3`). Use `ffx driver list-devices -v` to see the nodes, their bind properties, and the drivers bound to them.

## Tools

* [`//src/devices/pci/bin/lspci:lspci`](https://cs.opensource.google/fuchsia/fuchsia/+/main:src/devices/pci/bin/lspci/BUILD.gn;l=36)
  * Provides `lspci`, a tool to inspect and dump the state of PCI buses and devices in the system.
