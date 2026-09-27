---
name: suspend-resume-integration
description: >
  Add or migrate suspend and resume support in a Fuchsia DFv2 driver using
  power_managed_dispatchers_enabled (system_suspend/system_resume on
  fdf_component::Driver in Rust or SystemSuspend/SystemResume on
  fdf::DriverBase2 in C++) or the driver-owned fdf_power::SuspendableDriver /
  fdf_power::Suspendable pattern. Use when integrating system suspend/resume
  hooks, handling wake leases, or configuring
  power_managed_dispatchers.shard.cml.
---

# Implement Driver Suspend and Resume

> [!IMPORTANT]
> This guide covers two **mutually exclusive** ways to implement driver suspend
> and resume. **Only implement ONE of them in a driver -- never both:**
>
> 1. **Runtime-Managed Dispatchers (Default / Recommended)**: Implement
>    `system_suspend` / `system_resume` on `fdf_component::Driver` (Rust) or
>    `SystemSuspend` / `SystemResume` on `fdf::DriverBase2` (C++). Do **not**
>    use `fdf_power::SuspendableDriver` or `fdf_power::Suspendable`.
> 2. **Driver-Owned Suspend (`fdf_power`)**: Implement `suspend` / `resume` on
>    `fdf_power::SuspendableDriver` (Rust) or `Suspend` / `Resume` on
>    `fdf_power::Suspendable` (C++). Do **not** implement `system_suspend` /
>    `system_resume` (`SystemSuspend` / `SystemResume`) on the driver itself.
>
> Always use **Runtime-Managed Dispatchers** unless the driver must keep its
> dispatchers running while suspended.

## Dependencies

### Runtime-Managed Dispatchers (Recommended)

**GN**

```gn
# Rust drivers
deps = [
  "//sdk/lib/async/rust",
  "//sdk/lib/async/rust/fidl",
  "//sdk/lib/driver/component/rust",
  "//sdk/lib/driver/runtime/rust",
  "//sdk/rust/zx",
]

# C++ drivers
deps = [
  "//sdk/lib/driver/component/cpp",
]
```

**Bazel**

```bazel
# Rust drivers
deps = [
    "//sdk/lib/async/rust",
    "//sdk/lib/async/rust/fidl",
    "//sdk/lib/driver/component/rust",
    "//sdk/lib/driver/runtime/rust",
    "//sdk/rust/zx",
]

# C++ drivers
deps = [
    "@fuchsia_sdk//pkg/driver_component_cpp",
]
```

### Driver-Owned Suspend (`fdf_power`)

**GN**

```gn
# Rust drivers
deps = [
  "//sdk/lib/driver/component/rust",
  "//sdk/lib/driver/power/rust",
  "//sdk/rust/zx",
]

# C++ drivers
deps = [
  "//sdk/lib/driver/component/cpp",
  "//sdk/lib/driver/power/cpp",
]
```

**Bazel**

```bazel
# Rust drivers
deps = [
    "//sdk/lib/driver/component/rust",
    "//sdk/lib/driver/power/rust",
    "//sdk/rust/zx",
]

# C++ drivers
deps = [
    "@fuchsia_sdk//pkg/driver_component_cpp",
    "@fuchsia_sdk//pkg/driver_power_cpp",
]
```

## Choosing a Suspend Architecture

#### **If** the driver can pause normal dispatcher execution while suspended (Recommended for most drivers)

Enable runtime-managed dispatchers (`power_managed_dispatchers_enabled: "true"`)
by including
[`power_managed_dispatchers.shard.cml`](/sdk/lib/driver_component/power_managed_dispatchers.shard.cml).

With runtime-managed dispatchers enabled, the driver host and driver runtime
automatically coordinate dispatcher execution around power transitions:

1.  **Suspending**: When the driver's power element transitions to the suspended
    level, the driver runtime immediately transitions all power-managed
    dispatchers into the suspended state and waits only for **actively
    executing** callbacks to finish. Any tasks that were already queued on the
    dispatcher—as well as any new tasks arriving while active callbacks
    finish—are moved into the dispatcher's sleep queue and remain held until the
    dispatcher is resumed. Once all actively executing callbacks complete, the
    runtime invokes the driver's suspend hook (`system_suspend` in Rust or
    `SystemSuspend` in C++) on the **always-on view of the driver's default
    dispatcher** (`fdf_dispatcher_get_always_on_dispatcher`).
2.  **Resuming**: When the system resumes or a registered wake vector triggers,
    the runtime invokes the driver's resume hook (`system_resume` in Rust or
    `SystemResume` in C++) on the **always-on view of the driver's default
    dispatcher** **before** unpausing the power-managed dispatchers. This
    guarantees that hardware registers and clocks are restored before any queued
    dispatcher tasks execute.
3.  **Wake lease handoff & "First Domino" semantics**:
    - **Originating wakeup ("First Domino") vs. dependency resume**: When a
      registered driver runtime wake vector (such as an interrupt or a readable
      channel wait) fires while suspended, this driver is the "first domino" in
      the wakeup chain. The driver host automatically acquires a power lease
      from
      [`fuchsia.power.broker.Topology`](/sdk/fidl/fuchsia.power.broker/broker.fidl)
      on the driver's power element and passes the lease token
      (`Some(zx::EventPair)` in Rust or
      `std::optional<fuchsia_power_broker::LeaseToken>` in C++) to the resume
      hook. Conversely, if the driver is being resumed because a downstream
      driver depends on it (or during a system-wide resume), the driver host
      does not acquire a lease and passes `None` / `std::nullopt`.
    - **Deterministic correlation with the wake task**: The driver host awaits
      completion of `system_resume` / `SystemResume` before calling
      `driver_resume()` to unpause dispatchers. When unpausing, the driver
      runtime splices all triggered wake vector tasks (`wake_queue_`) to the
      **front** of the dispatcher's callback queue ahead of normal sleeping
      tasks. This guarantees that the wake vector task (such as the interrupt
      handler) executes immediately as the first task on the unpaused
      dispatcher. Drivers correlate the lease simply by storing the optional
      token in an instance field during `system_resume` and taking it at the
      start of the wake vector task.
    - **Dual-layer suspension prevention (Kernel vs. Power Broker)**: While an
      unacknowledged hardware wake interrupt holds a kernel `WakeEvent` that
      blocks `zx_system_suspend_enter`, it does **not** block Power Broker in
      user space. Power Broker only tracks active `LeaseToken` handles on the
      power element. Dropping `lease` inside `system_resume` closes the token,
      allowing Power Broker to immediately send `SetLevel(0)` to the driver
      host—which pauses dispatchers and invokes `system_suspend` before the
      interrupt handler can service hardware or forward data upstream.
      Therefore, drivers **must retain the lease token** across `system_resume`
      into the wake task, **move** `wake_lease` directly into any outgoing FIDL
      message (passing the power baton upstream) **before** calling
      `interrupt.ack()`, or—if no message is forwarded upstream—let `wake_lease`
      drop after `interrupt.ack()` once local handling completes. When woken as
      a dependency (`lease` is `None`), the driver instead receives a power
      baton inside the incoming FIDL request, holds that token until the request
      completes, and if calling another driver upstream, manually acquires a
      lease via `Topology.Lease` on its own power element to pass along.
4.  **Execution inside `system_suspend` / `system_resume` (`SystemSuspend` /
    `SystemResume`) & Always-On Dispatcher Rules**: During both `system_suspend`
    (`SystemSuspend`) and `system_resume` (`SystemResume`), all regular
    power-managed dispatchers are in the **suspended** state
    (`SuspendState::kSuspended`). Any non-always-on task or channel wait queued
    to a suspended dispatcher is diverted into its `sleep_queue_` and will not
    run until after `system_resume` (`SystemResume`) completes.
    - **C++ rules**: Even while executing inside `SystemSuspend` or
      `SystemResume` on the always-on dispatcher,
      `fdf_dispatcher_get_current_dispatcher()`,
      `fdf::Dispatcher::GetCurrent()`, `DriverBase2::driver_dispatcher()`, and
      `DriverBase2::dispatcher()` all return the **regular (suspended)**
      dispatcher, **not** the always-on dispatcher. Waiting on an async FIDL
      call (`fidl::WireClient` / `fdf::WireClient`) bound to `dispatcher()` or
      `driver_dispatcher()`, or waiting on `async::PostTask(dispatcher(), ...)`,
      before calling `SuspendCompleter` / `ResumeCompleter` causes a **permanent
      deadlock** because the response or task lands in `sleep_queue_`. Any async
      FIDL client or async task used inside `SystemSuspend` / `SystemResume`
      must be explicitly bound to the always-on dispatcher via
      `driver_dispatcher()->GetAlwaysOnDispatcher()->async_dispatcher()` (for
      `async_dispatcher_t*`) or
      `driver_dispatcher()->GetAlwaysOnDispatcher()->get()` (for
      `fdf_dispatcher_t*`), or use synchronous FIDL calls (`.sync()` /
      `fidl::WireSyncClient`) when the target server is not suspended.
    - **Rust rules**: The Rust `DriverServer` polls `system_suspend` and
      `system_resume` on a `fuchsia_async::LocalExecutor` backed by a dedicated
      always-on dispatcher, so standard **Zircon-transport (`zx::Channel`) async
      FIDL calls** and `fuchsia_async` timers/tasks are safe to `.await`.
      However, `DriverServer` sets `fdf::CurrentDispatcher` on that thread (and
      `context.root_dispatcher`) to the **regular (suspended)** dispatcher.
      Therefore, `.await`ing a default **Driver-Transport (`fdf::Channel`)**
      FIDL client (`DriverChannel<CurrentDispatcher>`) or a task spawned on
      `context.root_dispatcher` / `fdf::CurrentDispatcher` inside
      `system_suspend` or `system_resume` will **deadlock**. To use
      Driver-Transport FIDL or driver runtime tasks in `system_suspend` /
      `system_resume`, construct an explicit always-on `fdf::AsyncDispatcher`
      via
      `DriverDispatcherRef::from_async_dispatcher(context.root_dispatcher.as_async_dispatcher_ref()).always_on_dispatcher()`.
5.  **Stopping while suspended**: If the driver manager wants to stop a driver
    that is suspended, the driver will be resumed (`system_resume` /
    `SystemResume`) before it is stopped (`stop` / `Stop`).

**Manifest (`.cml`)**

Include
[`power_managed_dispatchers.shard.cml`](/sdk/lib/driver_component/power_managed_dispatchers.shard.cml)
in the component manifest, which sets
`program.power_managed_dispatchers_enabled: "true"` and routes the required
power protocols:

```json5
{
    include: [
        "//sdk/lib/driver_component/power_managed_dispatchers.shard.cml",
        "syslog/client.shard.cml",
    ],
    program: {
        runner: "driver",
        binary: "driver/my_driver.so",
        bind: "meta/bind/my_driver.bindbc",
    },
}
```

**Rust Implementation
([`fdf_component::Driver`](/sdk/lib/driver/component/rust/src/lib.rs))**

Implement `system_suspend` and `system_resume` directly on the
[`fdf_component::Driver`](/sdk/lib/driver/component/rust/src/lib.rs) trait:

```rust
use fdf::OnDispatcher;
use fdf_component::{Driver, DriverContext, DriverError, driver_register};
use fuchsia_sync::Mutex;
use futures::StreamExt;
use libasync::{DispatcherInterruptExt, OnInterrupt};
use std::sync::Arc;

pub struct MyDriver {
    wake_lease: Arc<Mutex<Option<zx::EventPair>>>,
}

impl Driver for MyDriver {
    const NAME: &str = "my_driver";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        // Initialize hardware, obtain irq, and bind services on power-managed dispatchers.
        # let irq = zx::Interrupt::from(zx::Handle::invalid());
        let wake_lease = Arc::new(Mutex::new(None));

        // Bind the interrupt and spawn its handler on the power-managed root_dispatcher
        // using libasync::OnInterrupt (NOT fuchsia_async::OnInterrupt / Task).
        let mut irq_stream = context.root_dispatcher.on_interrupt(irq);
        let lease_clone = wake_lease.clone();
        context
            .root_dispatcher
            .spawn(async move {
                while let Some(Ok(_timestamp)) = irq_stream.next().await {
                    Self::handle_interrupt(&irq_stream, &lease_clone).await;
                }
            })
            .unwrap();

        Ok(Self { wake_lease })
    }

    async fn stop(&self) {
        // If suspended when stopping, the runtime resumes the driver before calling stop().
    }

    async fn system_suspend(&self) -> Result<(), DriverError> {
        // Power-managed dispatchers are paused and actively executing callbacks have finished.
        // Note: Zircon-transport (`zx::Channel`) FIDL calls on `fuchsia_async` are safe to
        // `.await` here, whereas tasks/DriverChannels on `context.root_dispatcher` or
        // `fdf::CurrentDispatcher` will deadlock unless bound to `always_on_dispatcher()`.
        Ok(())
    }

    async fn system_resume(&self, lease: Option<zx::EventPair>) -> Result<(), DriverError> {
        // 1. Restore hardware state before power-managed dispatchers are unpaused.
        // 2. Always store the optional lease (`Some` if woken by this driver's wake vector,
        //    `None` if woken as a dependency).
        *self.wake_lease.lock() = lease;
        Ok(())
    }
}

impl MyDriver {
    // Runs first on the unpaused power-managed dispatcher when woken by a wake vector.
    async fn handle_interrupt(
        irq_stream: &OnInterrupt,
        wake_lease: &Mutex<Option<zx::EventPair>>,
    ) {
        // 1. Take the wake lease stored during system_resume (if any).
        let lease = wake_lease.lock().take();

        // 2. Service hardware while power element and clocks are guaranteed active.
        // 3. If forwarding an event upstream, MOVE `lease` (power baton) directly
        //    into the outgoing FIDL message BEFORE acknowledging the interrupt.

        // 4. Acknowledge the hardware interrupt.
        let _ = irq_stream.ack();

        // 5. If `lease` was not moved into an outgoing FIDL call, it drops here.
    }
}

driver_register!(MyDriver);
```

**Rust Dispatcher Execution: `libasync` / `fdf` vs. `fuchsia_async`**

> [!WARNING]
> In Rust drivers, `DriverServer` runs a `fuchsia_async::LocalExecutor` on a
> dedicated **always-on** dispatcher thread backed by its own private
> `zx::Port`. Any task, timer, interrupt, signal wait, or FIDL channel
> scheduled through `fuchsia_async` (or through a library built on
> `fuchsia_async`) **bypasses** the driver runtime's power-managed dispatcher--it
> will **not** pause when the driver suspends and will **not** register as a
> driver runtime wake vector.

To ensure normal driver work pauses during suspend and participates in wake
vector tracking, schedule all operational work on `context.root_dispatcher` or
[`fdf::CurrentDispatcher`](/sdk/lib/driver/runtime/rust/core/src/dispatcher.rs)
using [`libasync`](/sdk/lib/async/rust/src/lib.rs) (partially re-exported by
[`fdf`](/sdk/lib/driver/runtime/rust/src/lib.rs)) and
[`libasync_fidl`](/sdk/lib/async/rust/fidl/src/lib.rs):

- **Tasks**: Use `fdf::OnDispatcher::spawn` / `compute` or
  `fdf::OnDriverDispatcher::spawn_local` / `compute_local` on
  `context.root_dispatcher` or `fdf::CurrentDispatcher` instead of
  `fuchsia_async::Task` or `fuchsia_async::Scope`.
- **Interrupts**: Use `libasync::DispatcherInterruptExt::on_interrupt`
  (`libasync::OnInterrupt` in [`libasync`](/sdk/lib/async/rust/src/lib.rs))
  instead of `fuchsia_async::OnInterrupt`. `libasync::OnInterrupt` binds via
  `async_bind_irq` on the driver dispatcher so the runtime tracks it as a wake
  vector.
- **Timers**: Use `fdf::DispatcherTimerExt::after_deadline`
  ([`libasync::AfterDeadline`](/sdk/lib/async/rust/src/after_deadline.rs))
  instead of `fuchsia_async::Timer` or `fuchsia_async::OnTimeout`.
- **Signal waits**: Use `libasync::DispatcherSignalExt::on_signals`
  ([`libasync::OnSignals`](/sdk/lib/async/rust/src/on_signals.rs)) instead of
  `fuchsia_async::OnSignals`.
- **Zircon-transport FIDL (`zx::Channel`)**: Use `fidl_next` with
  `libasync_fidl::AsyncChannel<fdf::CurrentDispatcher>` and
  `context.incoming.connect_protocol_libasync_next()` instead of old `fidl`
  proxies or `connect_protocol_next()` (which bind `zx::Channel` to
  `fuchsia_async`).
- **Shared libraries**: Existing Rust libraries that internally use
  `fuchsia_async` (`Task`, `Timer`, `OnInterrupt`, `OnSignals`, or
  `fuchsia_async`-bound FIDL proxies) will continue running while the driver is
  suspended. To use such a library on a power-managed dispatcher, port or
  abstract it to use [`//sdk/lib/async/rust`](/sdk/lib/async/rust/BUILD.gn) and
  [`//sdk/lib/async/rust/fidl`](/sdk/lib/async/rust/fidl/BUILD.gn) (see
  [`/src/ui/lib/input_pipeline/src/dispatcher.rs`](/src/ui/lib/input_pipeline/src/dispatcher.rs)
  for an in-tree example). Note that `//sdk/lib/async/rust` has a `visibility`
  allowlist in its `BUILD.gn` that may need updating when adding new library
  dependents.

**C++ Implementation
([`fdf::DriverBase2`](/sdk/lib/driver/component/cpp/driver_base2.h))**

Override `SystemSuspend` and `SystemResume` on
[`fdf::DriverBase2`](/sdk/lib/driver/component/cpp/driver_base2.h):

```cpp
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/component/cpp/resume_completer.h>
#include <lib/driver/component/cpp/suspend_completer.h>

class MyDriver : public fdf::DriverBase2 {
 public:
  MyDriver() : fdf::DriverBase2("my_driver") {}

  zx::result<> Start(fdf::DriverContext context) override {
    // Any async FIDL client used inside SystemSuspend/SystemResume must be bound to
    // driver_dispatcher()->GetAlwaysOnDispatcher()->async_dispatcher(), NOT dispatcher().
    return zx::ok();
  }

  void SystemSuspend(fdf::SuspendCompleter completer) override {
    // Power-managed dispatchers are paused and actively executing callbacks have finished.
    // Note: dispatcher() and fdf::Dispatcher::GetCurrent() return the SUSPENDED regular
    // dispatcher here. Only wait on async work bound to GetAlwaysOnDispatcher().
    completer(zx::ok());
  }

  void SystemResume(std::optional<fuchsia_power_broker::LeaseToken> pe_lease,
                    fdf::ResumeCompleter completer) override {
    // Restore hardware state before dispatchers are unpaused.
    // Store pe_lease (`has_value()` if woken by a wake vector, `std::nullopt` if a dependency).
    wake_lease_ = std::move(pe_lease);
    completer(zx::ok());
  }

 private:
  void HandleInterrupt(async_dispatcher_t* dispatcher, async::IrqBase* irq,
                       zx_status_t status, const zx_packet_interrupt_t* packet) {
    // 1. Take the wake lease stored during SystemResume.
    std::optional<fuchsia_power_broker::LeaseToken> lease = std::move(wake_lease_);

    // 2. Service hardware and MOVE `lease` directly into any outgoing FIDL message
    //    (passing the power baton upstream) BEFORE acknowledging the interrupt.
    irq_.ack();
    // 3. If `lease` was not moved into an outgoing FIDL message, it drops here.
  }

  zx::interrupt irq_;
  std::optional<fuchsia_power_broker::LeaseToken> wake_lease_;
};

FUCHSIA_DRIVER_EXPORT2(MyDriver);
```

---

#### **Otherwise** if the driver requires custom request queue draining/rejection while suspended or structured config toggling

Use the Driver-Owned suspend pattern via
[`fdf_power::SuspendableDriver`](/sdk/lib/driver/power/rust/src/lib.rs) (in
Rust) or [`fdf_power::Suspendable`](/sdk/lib/driver/power/cpp/suspend.h) (in
C++) with `suspend_enabled: "true"` in `.cml`. In this mode, the runtime does
**not** pause dispatchers during suspend; the driver remains active on its
dispatchers and is responsible for rejecting or queueing incoming requests while
suspended, and can dynamically toggle suspend support via the
`fuchsia.power.SuspendEnabled` structured configuration capability.

**Manifest (`.cml`)**

```json5
{
    include: [ "syslog/client.shard.cml" ],
    program: {
        runner: "driver",
        binary: "driver/my_driver.so",
        bind: "meta/bind/my_driver.bindbc",
        suspend_enabled: "true",
    },
    use: [
        {
            config: "fuchsia.power.SuspendEnabled",
            key: "suspend_enabled",
            type: "bool",
            availability: "optional",
            default: false,
        },
        {
            protocol: [
                "fuchsia.power.broker.Topology",
                "fuchsia.power.system.ActivityGovernor",
                "fuchsia.power.system.CpuElementManager",
            ],
            availability: "optional",
        },
    ],
}
```

**Rust Implementation
([`fdf_power::SuspendableDriver`](/sdk/lib/driver/power/rust/src/lib.rs))**

Implement
[`fdf_power::SuspendableDriver`](/sdk/lib/driver/power/rust/src/lib.rs) and wrap
the driver type with
[`fdf_power::Suspendable`](/sdk/lib/driver/power/rust/src/lib.rs) in
`driver_register!`:

```rust
use fdf_component::{Driver, DriverContext, DriverError, driver_register};
use fdf_power::{Suspendable, SuspendableDriver};
use my_driver_config::Config;

pub struct MyDriver {
    config: Config,
}

impl Driver for MyDriver {
    const NAME: &str = "my_driver";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        let config = context.take_config::<Config>()?;
        Ok(Self { config })
    }

    async fn stop(&self) {}
}

impl SuspendableDriver for MyDriver {
    async fn suspend(&self) {
        // Drain or pause internal command queues and suspend hardware.
    }

    async fn resume(&self) {
        // Restore hardware and resume internal command queues.
    }

    fn suspend_enabled(&self) -> bool {
        self.config.suspend_enabled
    }
}

driver_register!(Suspendable<MyDriver>);
```

**C++ Implementation
([`fdf_power::Suspendable`](/sdk/lib/driver/power/cpp/suspend.h))**

Inherit from [`fdf_power::Suspendable`](/sdk/lib/driver/power/cpp/suspend.h),
take the power element runner from
[`fdf::DriverContext`](/sdk/lib/driver/component/cpp/driver_context.h), and call
`InitializeSuspend`:

```cpp
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/power/cpp/suspend.h>

#include "src/devices/my_driver/my_driver_config.h"

class MyDriver : public fdf::DriverBase2, public fdf_power::Suspendable<MyDriver> {
 public:
  MyDriver() : fdf::DriverBase2("my_driver") {}

  zx::result<> Start(fdf::DriverContext context) override {
    config_ = context.take_config<my_driver_config::Config>();
    incoming_ = std::shared_ptr<fdf::Namespace>(context.take_incoming());
    // Take power resources passed by the driver framework.
    power_element_runner_ = context.take_power_element_runner();

    if (config_.suspend_enabled()) {
      zx::result<> result = InitializeSuspend(dispatcher(), *incoming_, name());
      if (result.is_error()) {
        return result.take_error();
      }
    }
    return zx::ok();
  }

  void Suspend(fdf_power::SuspendCompleter completer) override {
    // Drain internal queues, put hardware in low-power state, then complete:
    completer();
  }

  void Resume(fdf_power::ResumeCompleter completer) override {
    // Restore hardware state, resume internal queues, then complete:
    completer();
  }

  bool SuspendEnabled() override { return config_.suspend_enabled(); }

  std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>> take_power_element_runner() {
    return std::move(power_element_runner_);
  }

 private:
  my_driver_config::Config config_;
  std::shared_ptr<fdf::Namespace> incoming_;
  std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>> power_element_runner_;
};

FUCHSIA_DRIVER_EXPORT2(MyDriver);
```

**Structured Configuration (`BUILD.gn`)**

When using structured configuration (`fuchsia.power.SuspendEnabled`), supply
values for the driver component in `BUILD.gn`:

```gn
import("//build/components.gni")

fuchsia_component_manifest("manifest") {
  component_name = "my_driver"
  manifest = "meta/my_driver.cml"
}

fuchsia_structured_config_values("sc_values") {
  cm_label = ":manifest"
  values = {
    suspend_enabled = true
  }
}

fuchsia_driver_package("package") {
  package_name = "my_driver"
  driver_components = [ ":component" ]
  deps = [ ":sc_values" ]
}
```

## Common Pitfalls

- **Dropping the wake lease inside `system_resume` / `SystemResume` (even for
  hardware wake interrupts)**: Although an unacknowledged hardware wake
  interrupt blocks the Zircon kernel from entering system suspend via its kernel
  `WakeEvent`, it does **not** block Power Broker in user space. Dropping
  `lease` / `pe_lease` immediately inside `system_resume` closes the
  `LeaseToken` handle, causing Power Broker to immediately issue `SetLevel(0)`
  on the driver's power element and causing the driver host to re-pause
  dispatchers and invoke `system_suspend` before the wake task can service
  hardware or pass data upstream. Always store the lease token in driver state
  and take it inside the wake task.
- **Calling `interrupt.ack()` before moving the power baton into an outgoing
  FIDL message**: Acknowledging the interrupt clears the kernel's `WakeEvent`.
  If the driver acknowledges the interrupt before **moving** `wake_lease` (or a
  newly acquired `Topology.Lease` when woken as a dependency) into the outgoing
  FIDL message to an upstream consumer, a race window opens where the system can
  suspend before the consumer receives the baton. Always move the baton into the
  outgoing FIDL call before calling `ack()`.
- **Deadlocking `SystemSuspend` / `SystemResume` (or `system_suspend` /
  `system_resume`) by waiting on a suspended dispatcher**: During both suspend
  and resume hooks, regular power-managed dispatchers are suspended and divert
  all non-always-on callbacks into `sleep_queue_`. In C++, `dispatcher()`,
  `driver_dispatcher()`, and `fdf_dispatcher_get_current_dispatcher()`
  (`fdf::Dispatcher::GetCurrent()`) still return the **regular (suspended)**
  dispatcher even while executing inside `SystemSuspend` or `SystemResume`—so
  waiting on an async FIDL client or `async::PostTask` bound to `dispatcher()`
  before invoking `SuspendCompleter` / `ResumeCompleter` permanently deadlocks.
  Bind async clients/tasks needed during suspend or resume to
  `driver_dispatcher()->GetAlwaysOnDispatcher()->async_dispatcher()` (or use
  `.sync()` calls). In Rust, Zircon-transport (`zx::Channel`) FIDL calls on
  `fuchsia_async` are safe to `.await`, but default Driver-Transport
  (`DriverChannel<CurrentDispatcher>`) calls and tasks spawned on
  `context.root_dispatcher` or `fdf::CurrentDispatcher` will deadlock unless
  bound to `always_on_dispatcher()`.
- **Assuming `lease` is always `Some` in `system_resume` / `SystemResume`**:
  `lease` is only populated when the driver is the "first domino" woken by its
  own registered runtime wake vector. When the driver is resumed as a dependency
  because a downstream driver requires it (or during a system-wide resume),
  `lease` is `None` / `std::nullopt`—any required power baton will arrive inside
  the subsequent FIDL request from the downstream caller.
- **Implementing both `Driver` (`system_suspend`/`system_resume` or
  `SystemSuspend`/`SystemResume`) and `fdf_power` (`SuspendableDriver` or
  `fdf_power::Suspendable`) hooks, or mixing `fdf_power` with
  `power_managed_dispatchers_enabled: "true"`**: In Rust,
  `Suspendable<MyDriver>` does not forward `system_suspend`/`system_resume` to
  `MyDriver`; meanwhile, when `power_managed_dispatchers_enabled: "true"` is set
  in `.cml`, the driver host takes ownership of the `ElementRunner` channel
  (`PowerConfiguration::RuntimeControlled`) and does not pass
  `power_element_args.runner` to the driver. Do not combine
  `fdf_power::Suspendable` / `fdf_power::SuspendableDriver` with
  `system_suspend`/`SystemSuspend` or `power_managed_dispatchers_enabled:
  "true"`.
- **Registering `MyDriver` instead of `Suspendable<MyDriver>` in Rust when using
  `SuspendableDriver`**: For the Driver-Owned pattern,
  `driver_register!(MyDriver)` will not wire up the power element runner. Always
  invoke `driver_register!(Suspendable<MyDriver>)`.
- **Omitting
  [`power_managed_dispatchers.shard.cml`](/sdk/lib/driver_component/power_managed_dispatchers.shard.cml)
  when implementing `system_suspend` / `SystemSuspend`**: Without
  `power_managed_dispatchers_enabled: "true"` and the
  `fuchsia.power.broker.Topology` capability route provided by the shard, the
  driver host will not invoke `system_suspend` / `SystemSuspend` or acquire wake
  leases.
- **Using `fuchsia_async` primitives or `fuchsia_async`-based libraries for
  normal driver work in Rust**: Because `DriverServer` runs
  `fuchsia_async::LocalExecutor` on an always-on dispatcher with a private
  `zx::Port`, any tasks (`fuchsia_async::Task` / `Scope`), interrupts
  (`fuchsia_async::OnInterrupt`), timers (`fuchsia_async::Timer`), signal waits
  (`fuchsia_async::OnSignals`), or Zircon channels bound to `fuchsia_async`
  bypass the power-managed driver dispatcher--they keep running while the driver
  is suspended and do not register as runtime wake vectors. Use `fdf` /
  `libasync` (`OnDispatcher`, `OnDriverDispatcher`, `libasync::OnInterrupt`,
  `AfterDeadline`, `libasync::OnSignals`, and `libasync_fidl::AsyncChannel`) on
  `context.root_dispatcher` / `fdf::CurrentDispatcher`, and port any shared
  libraries to `libasync` (`//sdk/lib/async/rust`).

## Further Reading

- [Power Managed Dispatchers CML
  Shard](/sdk/lib/driver_component/power_managed_dispatchers.shard.cml)
- [Rust `fdf_component::Driver`
  Trait](/sdk/lib/driver/component/rust/src/lib.rs)
- [Rust Driver Runtime (`fdf`) Crate](/sdk/lib/driver/runtime/rust/src/lib.rs)
- [Rust `libasync` Crate](/sdk/lib/async/rust/src/lib.rs)
- [Rust `libasync_fidl` Crate](/sdk/lib/async/rust/fidl/src/lib.rs)
- [C++ `fdf::DriverBase2` Class](/sdk/lib/driver/component/cpp/driver_base2.h)
- [Rust `fdf_power::SuspendableDriver`
  Trait](/sdk/lib/driver/power/rust/src/lib.rs)
- [C++ `fdf_power::Suspendable` Mixin](/sdk/lib/driver/power/cpp/suspend.h)
- [Fuchsia Power Framework
  Documentation](/docs/concepts/power/power_framework.md)
