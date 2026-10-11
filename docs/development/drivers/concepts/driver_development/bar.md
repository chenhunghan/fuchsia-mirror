<!--
    (C) Copyright 2018 The Fuchsia Authors. All rights reserved.
    Use of this source code is governed by a BSD-style license that can be
    found in the LICENSE file.
-->

# Configuration

Hardware peripherals are attached to the CPU through a bus, such as the PCI bus.

During bootup, the BIOS (or equivalent platform startup software)
discovers all of the peripherals attached to the PCI bus.
Each peripheral is assigned resources (notably interrupt vectors,
and address ranges for configuration registers).

The impact of this is that the actual resources assigned to each peripheral may
be different across reboots.
When the operating system software starts up, it enumerates
the bus and starts drivers for all supported devices.
The drivers then call PCI functions in order to obtain configuration information about
their device(s) so that they can map registers and bind to interrupts.

## Base address register

The Base Address Register (**BAR**) is a configuration register that exists on each
PCI device.
It's where the BIOS stores information about the device, such as the assigned interrupt vector
and addresses of control registers.
Other, device specific information, is stored there as well.

Drivers connect to the `fuchsia.hardware.pci/Device` protocol from their incoming
namespace:

```cpp
#include <fidl/fuchsia.hardware.pci/cpp/wire.h>

// In Driver::Start(fdf::DriverContext context)
zx::result pci_client_end = context.incoming().Connect<fuchsia_hardware_pci::Service::Device>();
if (pci_client_end.is_error()) {
  return pci_client_end.take_error();
}
fidl::WireSyncClient<fuchsia_hardware_pci::Device> pci(std::move(pci_client_end.value()));
```

Call `GetBar(bar_id)` on the `fuchsia.hardware.pci/Device` client to retrieve the BAR
resource (where `bar_id` is the BAR register number, starting with `0`), and then call
**fdf::MmioBuffer::Create()** to map the BAR's VMO into the driver's address space:

```cpp
zx::result<fdf::MmioBuffer> MmioBuffer::Create(zx_off_t offset, size_t size, zx::vmo vmo,
                                               uint32_t cache_policy);
```

The `cache_policy` parameter determines the caching policy for access,
and can take on the following values:

`cache_policy` value                | Meaning
------------------------------------|---------------------
`ZX_CACHE_POLICY_CACHED`            | use hardware caching
`ZX_CACHE_POLICY_UNCACHED`          | disable caching
`ZX_CACHE_POLICY_UNCACHED_DEVICE`   | disable caching, and treat as device memory
`ZX_CACHE_POLICY_WRITE_COMBINING`   | uncached with write combining

Note that `ZX_CACHE_POLICY_UNCACHED_DEVICE` is architecture dependent
and may in fact be equivalent to `ZX_CACHE_POLICY_UNCACHED` on some architectures.

## Reading and writing memory

Once **fdf::MmioBuffer::Create()**
returns a valid buffer, you can access the BAR through the `fdf::MmioBuffer` interface, for example:

```cpp
#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <lib/driver/mmio/cpp/mmio-buffer.h>

fidl::WireResult bar_result = pci->GetBar(0);
if (!bar_result.ok()) {
  return zx::error(bar_result.status());
}
if (bar_result->is_error()) {
  return bar_result->take_error();
}

fuchsia_hardware_pci::wire::Bar& bar = bar_result->value()->result;
if (!bar.result.is_vmo()) {
  return zx::error(ZX_ERR_WRONG_TYPE);
}

zx::result<fdf::MmioBuffer> mmio = fdf::MmioBuffer::Create(
    0, bar.size, std::move(bar.result.vmo()), ZX_CACHE_POLICY_UNCACHED_DEVICE);
if (mmio.is_ok()) {
  mmio->Write32(0x1234, REGISTER_X);  // configure register X for deep sleep mode
}
```
