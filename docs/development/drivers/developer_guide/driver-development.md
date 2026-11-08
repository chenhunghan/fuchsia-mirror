# Fuchsia driver development

Note: The workflows documented on this page are specific to the Fuchsia source
checkout (`fuchsia.git`) environment and assume the GN build system. For
SDK-based driver development, see
[SDK driver development](/docs/development/sdk/index.md). For a step-by-step
guide to writing your first driver, see
[Write a minimal DFv2 driver][write-a-minimal-dfv2-driver].

Fuchsia drivers are components dynamically loaded into [driver host][driver-host]
processes in user space. The process of loading and coordinating drivers is
managed by the [driver manager][driver-manager]. See the
[Driver framework][driver-framework] and [Drivers and nodes][drivers-and-nodes]
documentation for more information on driver hosts, driver manager, and the
device topology.

## Directory structure

Drivers may be found throughout the source tree under `drivers` subdirectories of
areas as specified in the [source code layout](/docs/development/source_code/layout.md)
document. Most Fuchsia drivers are located under [//src/devices/](/src/devices).
They are grouped based on the hardware FIDL protocols they implement, defined under
[//sdk/fidl/](/sdk/fidl/). For example, a USB ethernet driver goes in
[//src/connectivity/ethernet/drivers/](/src/connectivity/ethernet/drivers/)
because it implements an ethernet protocol, while drivers that implement the
USB controller stack are located in [//src/devices/usb/drivers/](/src/devices/usb/drivers/).

In the driver's `BUILD.gn`, four target types are typically defined:

1. `driver_bind_rules`: Compiles the `.bind` file into driver bind bytecode.
2. `fuchsia_cc_driver` (or `fuchsia_rust_driver`): Compiles the driver shared library.
3. `fuchsia_driver_component`: Wraps the driver shared library and bind rules into a
   driver component with its component manifest (`.cml`).
4. `fuchsia_driver_package`: Packages the driver component for inclusion in the system.

For detailed instructions and examples on configuring these GN targets, refer to
[Building drivers](/docs/development/drivers/best_practices/build.md).

## Creating a new driver

Creating a new driver can be done using the [Create Tool](/tools/create/README.md):

```posix-terminal
fx create driver --path <PATH> --lang cpp
```

This command creates the directory `<PATH>` containing template driver sources,
where the last segment of `<PATH>` becomes the driver name and GN target name.
After running this command, follow these steps:

1. Define bind rules in `meta/<NAME>.bind`.
2. Configure the component manifest in `meta/<NAME>.cml`.
3. Add driver metadata in `meta/<NAME>-info.json`, including a `short_description`
   and `areas` matching an entry in `//build/drivers/areas.txt`.
4. Implement the driver class in C++ (inheriting from `fdf::DriverBase2`) or Rust.
5. Include the `tests` target in the `<PATH>:tests` build target to ensure CQ coverage.

## Declaring a driver and bind rules

Drivers are matched to devices (nodes) using declarative bind rules placed in a
`.bind` file alongside the driver. The bind compiler compiles these rules into
bytecode that the driver index evaluates without needing to load the driver into
memory.

The following example illustrates bind rules for an AHCI PCI device:

```none
using fuchsia.pci;
using fuchsia.pci.massstorage;

fuchsia.BIND_PROTOCOL == fuchsia.pci.BIND_PROTOCOL.DEVICE;
fuchsia.BIND_PCI_CLASS == fuchsia.pci.BIND_PCI_CLASS.MASS_STORAGE;
fuchsia.BIND_PCI_SUBCLASS == fuchsia.pci.massstorage.BIND_PCI_SUBCLASS_SATA;
fuchsia.BIND_PCI_INTERFACE == 0x01;
fuchsia.BIND_COMPOSITE == 1;
```

In `BUILD.gn`, declare the bind target:

```gn
driver_bind_rules("bind") {
  rules = "meta/ahci.bind"
  bind_output = "ahci.bindbc"
  deps = [
    "//src/devices/bind/fuchsia.pci",
    "//src/devices/bind/fuchsia.pci.massstorage",
  ]
}
```

For more details on authoring bind rules, see [Driver binding][driver-binding] and
the [Bind rules tutorial][bind-rules-tutorial].

In C++, the driver class inherits from `fdf::DriverBase2` and exports itself using
the `FUCHSIA_DRIVER_EXPORT2` macro:

```cpp
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>

class AhciDriver : public fdf::DriverBase2 {
 public:
  AhciDriver() = default;

  zx::result<> Start(fdf::DriverContext context) override;
};

FUCHSIA_DRIVER_EXPORT2(AhciDriver);
```

When publishing child nodes that downstream drivers can bind to, specify node
properties using `fuchsia_driver_framework::NodeProperty2` and `fdf::MakeProperty2`:

```cpp
std::vector<fuchsia_driver_framework::NodeProperty2> properties = {
    fdf::MakeProperty2(bind_fuchsia::PCI_VID, device_info.vendor_id),
    fdf::MakeProperty2(bind_fuchsia::PCI_DID, device_info.device_id),
};

zx::result child = AddChild("sata-child", properties, offers);
```

## Driver lifecycle

### Start

The driver framework initializes a driver instance by calling its `Start()` hook.
Drivers receive an `fdf::DriverContext` providing access to their incoming namespace,
outgoing directory, node control, and dispatcher.

```cpp
zx::result<> AhciDriver::Start(fdf::DriverContext context) {
  // Access incoming services and initialize hardware.
  return zx::ok();
}
```

Drivers should make no assumptions about the state of hardware when `Start()` is
invoked; the device may not be in a clean reset state if a previous driver host
crashed or restarted.

Drivers should avoid blocking the driver dispatcher with long-running operations
during `Start()`. If a driver needs to perform asynchronous initialization (such
as loading firmware or waiting on hardware calibration), implement the asynchronous
`Start()` overload that takes an `fdf::StartCompleter`:

```cpp
void AhciDriver::Start(fdf::DriverContext context, fdf::StartCompleter completer) {
  // Perform asynchronous initialization on the dispatcher.
  // When complete:
  completer(zx::ok());
}
```

### PrepareStop and Stop

When a driver is being shut down or unbound, the framework invokes the teardown
hooks:

1. `PrepareStop(fdf::PrepareStopCompleter completer)`: The driver should stop
   servicing new client requests, cancel pending hardware transactions, and complete
   outstanding asynchronous tasks. Once finished, invoke `completer(zx::ok())`.
2. `Stop()`: Final cleanup before the driver instance is destroyed.

## Driver communication

All communication in Fuchsia occurs over capabilities routed through the component
framework, primarily [FIDL service capabilities][service-capabilities].

### Connecting to parent services

Drivers connect to FIDL protocols exposed by parent nodes or the platform using
their incoming namespace:

```cpp
zx::result pci = context.incoming().Connect<fuchsia_hardware_pci::Service::Device>();
if (pci.is_error()) {
  return pci.take_error();
}
```

### Advertising services to children and components

A driver can advertise FIDL services to child drivers or non-driver components by
adding service handlers to its outgoing directory and offering them when adding a child:

```cpp
// 1. Add service to outgoing directory
zx::result result = context.outgoing()->AddService<fuchsia_example::EchoService>(
    std::move(handler));

// 2. Offer service to child node
std::array offers = {
    fdf::MakeOffer2<fuchsia_example::EchoService>(),
};
zx::result child = AddChild("echo-child", properties, offers);
```

For a comprehensive guide, see [Driver communication][driver-communication] and
the [FIDL tutorial][fidl-tutorial].

### Colocation and transport

Drivers that require high-throughput, low-latency communication with parent or child
drivers can be co-located within the same driver host process by setting
`colocate: "true"` in their component manifest. Co-located drivers can use the
[Driver runtime][driver-runtime] transport (`fdf::Channel`) for in-process,
zero-copy communication without kernel context switches. Drivers in separate processes
communicate across Zircon channels.

## Device interrupts

Hardware interrupts are represented by Zircon [`zx::interrupt`][zx-interrupt]
kernel objects. Drivers request an interrupt capability from their parent bus driver
(such as the platform bus or PCI driver).

Drivers should listen to interrupts asynchronously on their synchronized driver
dispatcher using [`async::IrqMethod`][async-irq-method]:

```cpp
#include <lib/async/cpp/irq.h>

class MyDriver : public fdf::DriverBase2 {
 public:
  zx::result<> Start(fdf::DriverContext context) override {
    irq_method_.set_handler(
        [this](async_dispatcher_t* dispatcher, async::IrqMethod* irq,
               zx_status_t status, const zx_packet_interrupt_t* packet) {
          OnInterrupt(status);
        });
    return zx::make_result(irq_method_.Post(dispatcher(), interrupt_.get()));
  }

 private:
  void OnInterrupt(zx_status_t status) {
    // Acknowledge interrupt and process hardware events.
    interrupt_.ack();
  }

  zx::interrupt interrupt_;
  async::IrqMethod irq_method_{this};
};
```

For more details and Rust examples, see
[Handle interrupts in a driver][handle-interrupts].

## Driver rights and hardware resources

Drivers run in restricted user space processes with sandboxed namespaces. Drivers do
not have arbitrary access to the filesystem, raw memory, or root resources.

Privileged hardware resources (such as MMIO ranges, interrupts, and Bus Transaction
Initiators for DMA) are managed and partitioned by bus drivers such as the
[Platform Bus][platform-bus] or [PCI][pci-driver] driver. Drivers request access to
their assigned resources via FIDL protocols provided by their parent node.

## Power management

Drivers integrate with the Fuchsia Power Framework through Driver Framework power
elements. By enabling runtime-managed dispatchers (`power_managed_dispatchers_enabled: "true"`
in the driver manifest), the driver runtime coordinates power transitions:

- **Suspending**: When entering a low-power state, power-managed dispatchers are paused,
  and the driver's `SystemSuspend` hook is called on the always-on dispatcher to save
  hardware state.
- **Resuming**: When waking up, the driver's `SystemResume` hook restores hardware
  state before dispatchers resume processing requests.

For detailed guidelines, see [Device power management][device-power-management].

<!-- Reference links -->

[driver-framework]: /docs/concepts/drivers/driver_framework.md
[driver-manager]: /docs/concepts/drivers/driver_framework.md#driver_manager
[driver-host]: /docs/concepts/drivers/driver_framework.md#driver_host
[driver-runtime]: /docs/concepts/drivers/driver_framework.md#driver_runtime
[drivers-and-nodes]: /docs/concepts/drivers/drivers_and_nodes.md
[driver-binding]: /docs/concepts/drivers/driver_binding.md
[driver-communication]: /docs/concepts/drivers/driver_communication.md
[service-capabilities]: /docs/concepts/components/v2/capabilities/service.md
[write-a-minimal-dfv2-driver]: /docs/development/drivers/developer_guide/write-a-minimal-dfv2-driver.md
[bind-rules-tutorial]: /docs/development/drivers/tutorials/bind-rules-tutorial.md
[fidl-tutorial]: /docs/development/drivers/tutorials/fidl-tutorial.md
[handle-interrupts]: /docs/development/drivers/developer_guide/handle-interrupts-in-a-driver.md
[platform-bus]: /docs/development/drivers/concepts/device_driver_model/platform-bus.md
[device-power-management]: /docs/development/drivers/concepts/device_driver_model/device-power.md
[pci-driver]: /src/devices/pci/
[zx-interrupt]: /reference/kernel_objects/interrupts.md
[async-irq-method]: /sdk/lib/async/include/lib/async/cpp/irq.h
