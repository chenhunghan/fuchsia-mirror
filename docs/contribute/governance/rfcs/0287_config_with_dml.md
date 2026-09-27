<!-- mdformat off(templates not supported) -->
{% set rfcid = "RFC-0287" %}
{% include "docs/contribute/governance/rfcs/_common/_rfc_header.md" %}
# {{ rfc.name }}: {{ rfc.title }}
{# Fuchsia RFCs use templates to display various fields from _rfcs.yaml. View
the #}
{# fully rendered RFCs at https://fuchsia.dev/fuchsia-
src/contribute/governance/rfcs #}
<!-- SET the `rfcid` VAR ABOVE. DO NOT EDIT ANYTHING ELSE ABOVE THIS LINE. -->

<!-- mdformat on -->

## Problem Statement

### What problem does this proposal solve?

The current Fuchsia Driver Framework requires developers to
coordinate configuration information such as compatible strings, device
properties, interfaces and capability routes across multiple files. This
highly fragmented model results in a "coordination tax" that severely slows
down hardware bring-up and driver development.

Specifically, a driver's configuration can currently come from up to six
distinct locations, several of which contain duplicated information, and
require the driver author to keep multiple files synchronized:

1. The Bind Rules (.bind): Defines the criteria that the Driver Manager's
   binding program uses to match the driver to a device node, but also contains
   important labelling information.
2. The Component Manifest (.cml): Declares the capabilities the driver needs to
   use, offer, or expose, which must correspond to the offers and exposes from
   its parent.
3. The Build System (BUILD.gn): Specifies the physical targets, dependencies,
   and packaging rules for both the driver and its binding identities.
4. The Board Driver (C++ or devicetree): Manually instantiates the child nodes,
   registers physical properties (MMIOs, IRQs), and binds them to matching
   properties.
5. The Driver Source Code: Programmatically fetches resources and connects to
   services proffered by the Driver and Component Framework inside the driver's
   `Start()` block, relying on hardcoded indices or string identifiers that
   must match the string names and indices declared in the board driver and the
   manifest.
6. FIDL: Metadata needed for the drivers configuration and operation is often
   encoded as a FIDL struct.

### The Consequences of Duplicated Metadata

* Many of these files are not validated against each other during the build or
  assembly phases, resulting in increased potential for synchronization drift.
  A typo in a resource name or an index mismatch usually compiles successfully,
  only to fail silently at runtime, resulting in the driver failing to load, or
  failing to access a resource some time during operation.
* Bringing up a new SoC or board experiences a higher degree of friction, as it
  cannot be done purely through hardware definitions. It requires writing and
  modifying board-driver code to manually match, translate, and route physical
  resources to specific driver instances.
* It is often unclear why a driver did not bind, as the bind system is
  difficult to configure and debug.  Since the bind conditions are arbitrary
  and there are many different patterns employed to match drivers, it can be
  unclear what information is needed.
* Devicetree visitors: Although many fuchsia boards currently use devicetrees,
  which contain all of the board specific configuration data, the devicetree
  still needs to be interpreted by a "visitor" program, to parse at boot time
  the devicetree data into the format that fuchsia requires. While visitors are
  necessary to bridge the gap between Linux Devicetree schemas and Fuchsia's
  capability model, maintaining and executing this translation layer in C++ at
  runtime adds operational overhead and makes the resulting topology harder to
  statically inspect and validate.
* Barrier to Automation and Tooling: The multi-file fragmentation of the driver
  configuration files presents a challenge to both human and AI driver authors,
  for code generation and debugging. The driver author must successfully modify
  and synchronize C++ code, configuration JSON (.cml), and domain-specific
  bind syntax (.bind) simultaneously.

### What problem(s) is this proposal not trying to solve?

This proposal does not replace runtime dynamic discovery and binding for
hot-pluggable buses (such as USB and PCIe) where device presence and matching
properties can only be determined at runtime.

While Driver DML applies universally across the tree, Board DML only targets
static hardware topologies. We do not replace runtime ACPI discovery on x86
platforms; x86 boards will continue to rely on the ACPI board driver to discover
hardware and construct device nodes at boot.

Zircon's `phys` boot shim is unchanged; it continues to consume
bootloader-supplied Devicetrees for early kernel boot and memory reservations.

This proposal does not eliminate the need for developers to write the core
functional hardware register/logical code of their drivers.

Finally, this proposal does not dictate Driver Framework Metadata schema
definitions, aggregation, or deserialization. That architecture is being
addressed in a parallel design document.

## Background

### The Core Needs of a Driver

At its core, a driver's relationship with the operating system can be divided
into a few distinct requirements.

**Incoming Resources:**

* From the kernel: MMIO registers, IRQs, BTIs
* From resource controllers:  Clocks, GPIOs, PWMs
* From other drivers: many different domain specific protocols
* Implicit resources:  storage and cpu
* The driver must be able to assume that all resources have been correctly
  routed, and all configuration data is correct. (The driver cannot be
  responsible for verifying that it is connected to the correct instance of a
  service).

**Advertising & Serving Outputs:** Logical capabilities (FIDL protocols,
services) exposed to downstream clients. Outputs may need to specify a general
destination, but usually not a specific connection.

**Internal Configuration Data:** Non-system resource parameters that allow a
generic driver to target a specific hardware configuration (e.g., hardware
version, hardware enable flags, physical device characteristics).

**Lifecycle & Power Management:** a defined set of interfaces and api contracts
that cover: start, reload, low power states (suspend / resume) and shutdown.
The driver must have an agreement from the framework that if the driver is
started or resumed, all of its required resources and configurations are
available, or will be available when queried.

**The ideal system:**

* Specifies each bit of information once.
* Centralizes configuration into logical locations.
* Validates any configuration issues at build or assembly time
* Is modular enough to allow drivers to be reused by multiple boards.

### Driver vs System Configuration

If modularity was not a requirement, we could keep all configuration data in
one location, at the board level. However, to maintain modularity we must split
driver configuration into two locations:

1. **Driver Specific** - indicate what a driver needs to operate in terms of
the core needs.  For example: a display driver might need to interface with:
    * 2 clocks: (`clock-pixel` and `clock-dsi`),
    * 2 mmio ranges: (`dsi` and `mdp`) and 1 interrupt (`vsync`),
    * 1 GPIO pin (`gpio-lcd-reset`), and
    * output a `fuchsia.hardware.display.DisplayEngine` service.

2. **Board Specific** - give the board specific information about how a driver
is connected, where its capabilities come from, and what board specific
configuration data is provided.  For example: That display driver's:
    * Two clock services are routed from the board's clock controllers, where
      `clock-pixel` is provided by `main_pll_controller` and `clock-dsi` is
      provided by `dsi_pll`.
    * The mmio ranges and interrupt are served by the `pdev` driver, with `dsi`
      corresponding to an offset of `0x51280` and size `0xfa0` (4000 bytes),
      `mdp` corresponding to an offset of `0x56000` and size `0x12c0` (4800
      bytes), and the `vsync` interrupt mapped to IRQ 35.
    * The `gpio-lcd-reset` pin is routed from `gpio-controller-20` on pin 0.

Note that the board specific information has an implicit dependency on the
driver specific configuration. This split is unavoidable if drivers are to
remain modular.

### Why Now?

For most of Fuchsia's development, drivers have assumed a completely dynamic
model. This assumption is important for general purpose operating systems that
need to discover hardware at runtime. However, it has become clear that
Fuchsia can safely abandon this flexibility for the foreseeable future. By
embracing the static hardware configurations of the boards we plan to support,
we can *greatly simplify* the way drivers are discovered and configured, which
in turn allows driver creation to be much more ergonomic.  Of course, there are
still situations where dynamism is unavoidable.  See [Non-Static
Considerations](#non-static-considerations) for examples of these situations,
and how we plan to address them.

## Summary

This proposal introduces the consolidation of all the configuration related to
a board and its drivers into a single configuration language called the Driver
Markup Language (DML).  DML files are split into 2 categories:

 * Driver DML files: these cover the configuration to the driver, and
   incorporate information from what is currently the .bind and .cml files.
 * Board DML files:  these cover the configuration of the entire board, and are
   designed to provide similar information to what the devicetree does today.

This consolidation will allow the following architectural goals:

* All driver and board configuration, as well as driver topology will be done
  during assembly.
* Binding will be done statically, informed by the Board DML, with dynamic
  binding isolated to specific hot-pluggable subsystems (e.g., PCIe, USB).
* Framework related driver build configuration and driver base configuration
  boilerplate is generated from the Driver DML files.
* Driver specific configuration data, aggregated metadata, resource identity
  and dependency resolution will happen based on the Board DML, rather than
  parent-child relationships between drivers.

There will be several benefits of this change:

* A static configuration that is known and validated at assembly time greatly
  simplifies board bring-up.
* Improved Static Debuggability: Because DML continues to generate standard
  .cml manifests for cmc, existing Component Framework tooling (like cmc
  validation, scrutiny, and ffx component) can validate static capability
  routing between driver components, while assembly-time DML validators and ffx
  driver handle hardware-specific constraints (MMIOs, IRQs, pins) and runtime
  diagnostics
* Easier to understand configuration: By eliminating devicetree visitors and
  consolidating configuration into two sets of files, driver configurations
  will become much easier to reason about, both by humans and AI.

## Stakeholders

_Facilitator:_ Abdulla Kamar (@abdulla)

_Reviewers:_

* Claire Gonyeo (@cgonyeo)  - Component Framework
* Aidan Wolter (@awolter)  - Software Assembly
* Suraj Malhotra (@surajmalhotra) - Driver Framework
* Puneetha Ramachandra (@puneetha) - Platform Drivers
* Chris Anderson (@cja) - Platform Drivers
* Sarah Chan (@spqchan) - Driver Framework
* Justin Mattson (@jmatt) - Driver Framework

_Socialization:_ This proposal has been socialized within the Driver Framework
and Driver Teams to align on architectural goals and staging phases.

## Requirements

To address the core problems, any acceptable solution must meet the following
architectural requirements:

* Declarative Board Configuration: Bringing up a new product or board must be
  achieved purely through declarative configurations (Board DML), without
  writing custom coded board-drivers or devicetree visitors.
* Single Source of Truth: A driver's incoming and outgoing dependencies must be
  declared in a single, schema-validated configuration file.
* Static Validation: Capability routes, physical resource assignments, and
  configurations must be verified at compile/assembly time rather than runtime.
* Zero-Boilerplate Runtime Plumbing: All runtime plumbing for diagnostics
  (logging, inspect), resource mapping, and IPC connections must be made
  available by the framework, rather than requiring hand-coded boilerplate in
  driver code.
* Intuitive: It should be easy for humans and AI to understand how all the
  drivers are connected, and what capabilities and configurations are routed.
* Debugability: It should be easy to diagnose if and why a driver would lack
  access to the resources it needs to start.

## Design

Note: All configuration examples in this document are representative and
intended for illustrative purposes. They do not necessarily represent the final
syntax or schema.

### 1. The Driver Markup Language (DML) vs. CML

Drivers are considered components, and are thus required to specify a component
manifest (CML), which enumerated the types of capabilities that a component
needs.  Unfortunately, CML does not provide all the information that a driver
requires to fully resolve its connections.  Currently the driver framework uses
multiple other sources of information to supplement the CML.  DML is designed
to be roughly a superset of the CML schema, adding in a few important aspects.
Using a separate but similar configuration language allows the transition to
proceed quickly, as we can add in temporary features to DML to accommodate the
needs of various drivers.  Once the full migration is achieved, we will seek to
merge the added features of DML into CML wherever appropriate, ideally
achieving full integration, and thus merging DML back into CML completely.

#### Instance Identity.

Drivers frequently require multiple distinct instances of a service. For
example, a display controller driver may require two separate clock lines (one
for pixel timing, one for an dsi timing pipeline) alongside an I2C bus. All are
provided by core drivers through generic FIDL services.

Standard CML relies on the binding of composite nodes to route the correct
capability instance when multiple of a single capability are routed to a
driver. DML instead allows a `use` block to specify a local short name alias.

#### Driver DML

Authors define a single `.dml` file per driver specifying their platform
dependencies. The `dmlc` compiler generates the required component manifest,
that is given to the component framework, as well as topological bind matchers,
and language-specific scaffolding from the driver DML at compile time.

The Driver DML merges information that is currently expressed through a
combination of the `.bind` and `.cml` files into a unified `.dml` file.

This consolidation allows the Driver DML to specify both identity and
capability requirements when routing multiple services of the identical type.
Whereas connecting to two `clock.Service` instances requires a CML capability
declaration coupled with manual node matching and custom string properties in
the Bind file, With DML, authors explicitly declare local capability identities
directly.

**CML (Before)**
```json5
// my-display-driver.cml
{
    capabilities: [
        { service: "fuchsia.hardware.display.DisplayEngine" }
    ],
    use: [
        // Broad declarations for all instances of the service type
        { service: "fuchsia.hardware.platform.device.Service" },
        { service: "fuchsia.hardware.clock.Service" },
        { service: "fuchsia.hardware.gpio.Service" },
    ],
    expose: [
        { service: "fuchsia.hardware.display.DisplayEngine", from: "self" }
    ]
}
```

**Bind (Before)**

```json5
// my-display-driver.bind
using fuchsia.platform;
using fuchsia.amlogic.platform;
using fuchsia.amlogic.platform.clock;
using fuchsia.gpio;
using fuchsia.clock;
primary parent "pdev" {
  fuchsia.BIND_PROTOCOL == fuchsia.platform.BIND_PROTOCOL.DEVICE;
  fuchsia.BIND_PLATFORM_DEV_VID == fuchsia.amlogic.platform.BIND_PLATFORM_DEV_VID.AMLOGIC;
  fuchsia.BIND_PLATFORM_DEV_PID == fuchsia.amlogic.platform.BIND_PLATFORM_DEV_PID.A311D;
  fuchsia.BIND_PLATFORM_DEV_DID == fuchsia.amlogic.platform.BIND_PLATFORM_DEV_DID.DISPLAY;
}
parent "gpio-lcd-reset" {
  fuchsia.BIND_PROTOCOL == fuchsia.gpio.BIND_PROTOCOL.DEVICE;
  fuchsia.BIND_GPIO_PIN == fuchsia.amlogic.platform.BIND_GPIO_PIN.LCD_RESET;
}
parent "clock-pixel" {
  fuchsia.BIND_PROTOCOL == fuchsia.clock.BIND_PROTOCOL.DEVICE;
  fuchsia.BIND_CLOCK_ID == fuchsia.amlogic.platform.clock.BIND_CLOCK_ID.CLK_PIXEL;
}
parent "clock-dsi" {
  fuchsia.BIND_PROTOCOL == fuchsia.clock.BIND_PROTOCOL.DEVICE;
  fuchsia.BIND_CLOCK_ID == fuchsia.amlogic.platform.clock.BIND_CLOCK_ID.CLK_DSI;
}
```

**DML (After)**
```json5
// my-display-driver.dml
{
    name: "my-display-driver",
    capabilities: [
        { service: "fuchsia.hardware.display.DisplayEngine" }
    ],
    use: [
        {
            service: "fuchsia.hardware.platform.device.Service",
            name: "pdev",
            mmio: [ "dsi", "mdp" ],
            interrupts: [ "vsync" ],
        },
        { service: "fuchsia.hardware.gpio.Service", name: "gpio-lcd-reset" },
        { service: "fuchsia.hardware.clock.Service", name: "clock-pixel" },
        { service: "fuchsia.hardware.clock.Service", name: "clock-dsi" },
    ],
    expose: [
        { service: "fuchsia.hardware.display.DisplayEngine", name: "display_engine" }
    ]
}
```

In the driver DML, capability requirements include a `name` field, which
specifies exactly which role each service connection fulfills locally.
Further, by specifying a unique `name`, the DML compiler
([`dmlc`](#the-compiler-dmlc)) can automatically generate strongly-typed
`clock_pixel_` and `clock_dsi_` accessors that can be used in driver code.

To keep these names unique and avoid collisions, `dmlc` will validate at
assembly-time that no two `use` declarations within the same driver share the
same `name` alias.  Because these aliases are locally scoped to each individual
driver component, driver authors only need to ensure uniqueness within their
own `.dml` file, without needing to coordinate with other drivers globally.

#### Driver Specific Configuration

Driver specific configuration parsing is a significant source of runtime panics
and driver boilerplate. Standard CML has no mechanism for defining the internal
structure of arbitrary payload bytes passed between components, and currently
relies on FIDL structs to define complex configurations.

In DML, drivers will be able to nest configuration schemas directly under a
defined capability. By declaring the schema explicitly in the contract, `dmlc`
unlocks the ability to validate that the data encoded in the board DML and the
driver which receives the driver specific configuration are aligned at
build/assembly time.

(Exact configuration schema and serialization format TBD, pending parallel
designs).

#### Transitional Schemas

The current DML schema includes several transitional shims required to
interface with the current Driver Framework. For example, bind properties are
temporarily embedded alongside capability `use` blocks to bypass discrete
`.bind` files, and the 'primary parent' concept is retained strictly to satisfy
legacy composite node initialization. These constructs are temporary; they will
be completely phased out as we shift toward native Component Framework routing
and phasing out the divide between composite and non-composite nodes.

Because DML is actively rolling out across the tree, this RFC avoids locking
down an exhaustive schema specification. The exact structural contract will
continue to evolve as subsequent migrations expose new edge cases.

### 2. The Board DML

Board configuration currently relies on either Linux style devicetrees or C++
board drivers. Board drivers dictate manual child node instantiation and
structural boilerplate; devicetrees supply generic DTSs requiring a "visitor"
parsing layer to translate properties into Fuchsia-specific framework
constructs.

The Board DML replaces both paradigms with a strictly declarative,
assembly-time validated topology. While sharing JSON5 syntax with driver DMLs,
the Board DML provides the complimentary component: where drivers mostly have
*use* arrays to declare requirements, the Board DML lays out *children* and
*offer* blocks to route capabilities point-to-point between drivers.

Board DML files support including shards, which allows base SoC configurations
to be shared and reused cleanly across various board variations. However, as
Board DMLs represent a Fuchsia-specific configuration, it is assumed that the
conversion of Linux devicetrees to Board DMLs will be an important time to
validate the hardware during the bringup of a new Fuchsia device.

The declarative capability routing inherently implies hardware startup
sequencing and a dependency graph. Further, Board DML configurations will be
able to specify if a child component should delay until a dependency is
satisfied, or enable parallel driver startups using lazy dependency resolution.
To maintain dependency ordering, the Driver Manager will retain the
runner role, and orchestrate driver startup sequencing.

#### In the Board DML:

Instantiating a driver requires a corresponding entry Board DML file. The
author declares the driver as a child within the board topology, routes the
hardware-specific implementations to it, using the names in the `offers` array.
This guarantees that the correct physical hardware clock is routed to the
corresponding logical need inside the driver:

```json5
// my-board.dml
{
    children: [
        { name: "pdev", url: "fuchsia-pkg://fuchsia.com/platform-bus#meta/platform-bus.cm" },
        { name: "gpio-controller-20", url: "fuchsia-pkg://fuchsia.com/aml-gpio#meta/aml-gpio.cm" },
        { name: "main_pll_controller", url: "fuchsia-pkg://fuchsia.com/aml-clk#meta/aml-clk.cm" },
        { name: "dsi_pll", url: "fuchsia-pkg://fuchsia.com/aml-dsi-pll#meta/aml-dsi-pll.cm" },
        { name: "display", url: "fuchsia-pkg://fuchsia.com/my-display-driver#meta/my-display-driver.cm" }
    ],
    offers: [
        // Route core memory-mapped registers and IRQs
        {
            service: "fuchsia.hardware.platform.device.Service",
            from: "#pdev:display",
            to: "#display:pdev",
            constraints: {
                mmio: [
                    { name: "dsi", address: 0x51280, size: 0xfa0 },
                    { name: "mdp", address: 0x56000, size: 0x12c0 }
                ],
                interrupts: [
                    { name: "vsync", number: 35, mode: "EdgeHigh" }
                ]
            }
        },
        // Route pin 0 to the display for the LCD reset line
        {
            service: "fuchsia.hardware.gpio.Service",
            from: "#gpio-controller-20:lcd_reset",
            to: "#display:gpio-lcd-reset",
            constraints: { pin: 0 }
        },
        // Route the high-frequency PLL to the display's pixel clock
        {
            service: "fuchsia.hardware.clock.Service",
            from: "#main_pll_controller:clock-pixel",
            to: "#display:clock-pixel"
        },
        // Route the dedicated serial interface clock to the display's DSI PHY
        {
            service: "fuchsia.hardware.clock.Service",
            from: "#dsi_pll:clock-dsi",
            to: "#display:clock-dsi"
        }
    ]
}
```

The Board DML uses the local name alias from the driver along with the driver's
name to explicitly state routes between specific instances of services across
driver topology.

### The Compiler (dmlc) and Tooling Integration

The DML toolchain executes during driver build and product assembly, and will
operate on both Board DMLs and Driver DMLs.  Driver DMLs will be parsed and
embedded into `meta.far` to be used during assembly, whereas Board DMLs will be
packaged into the `BoardConfig`, replacing the existing `devicetree` field.

During Build:

* Component Manifest Synthesis: Compiles the driver DML to produce the driver
  Component Manifest (.cm).
* Driver Scaffold Generation: Outputs driver specific code which handles all
  the boilerplate involved in connecting to resources specified by the Driver
  DML, greatly simplifying driver setup code.  Exact structural patterns for
  different language bindings (e.g., C++ object inheritance vs Rust
  composition) will be finalized in a subsequent RFC.

During Assembly:

* A Rust library will be provided (`assembly_dml`) that will be linked into the
  assembly binary.
* The Board and Driver DMLs will be cross-validated to ensure that all
  connections are valid, and the driver's required capabilities are provided.
* The current component framework validators (such as hardware resource boundary
  enforcement and structured configuration validation) will be expanded to
  apply to the newly static drivers.
* The static node graph will be generated, in a json5 format that will be
  ingested by the Driver Manager.

## Implementation

The rollout is divided into five key operational phases to ensure platform
stability:

### Phase 1: CML and Bind Auto-Generation

* The dmlc parses driver DMLs to produce driver CMLs and .bind files.
* The dmlc also compiles the board DMLs to produce a fidl input that the board
  driver will ingest when Fuchsia starts.  the Board DMLs remain totally
  isolated from the driver DMLs.

Driver DML files will be introduced purely as a developer utility without
altering the underlying driver framework execution engine. Driver DML files
will be checked in for all drivers, and will be used to generate .cml and .bind
files which are used by the existing driver framework code. The generated .cml
and .bind files will be checked in, with an "AUTOGENERATED" notice, so as to
not impede any active driver development. .cml and .bind files will be
automatically regenerated at build time and compared against goldens, to catch
any changes introduced by alterations of a dml file or the dml compiler itself.
Any difference will result in a compiler error.
This will allow drivers to be migrated to use Driver DML with zero disruption
to driver development, and allow the driver framework team to establish the
syntax schema and gather developer feedback.

### Phase 2: Produce Board DMLs for All Boards

Systematically migrate existing devicetrees and board drivers to the
configuration-driven model using VIM3 as the testbed.
 * Hide actual conversion to Board DML behind configuration flag
 * No validation will take place between Board DML and Driver DML yet.
 * A Metadata format will be proposed in a separate design doc, and FIDL
   metadata will be converted to in-DML format.

### Phase 3: Validate and Switch to Board DML

An assembly validator will be added to validate Board DML against Driver DML,
to catch missing or mismatched capabilities during product assembly. At the same
time, a separate Board DML validator will ensure that switching to Board DMLs
has no effect on runtime driver topology or configuration.
Once validated, Devicetree-based boards will switch to Board DML, and their
corresponding C++ board drivers and Devicetree visitor pipelines will be
deleted.
The x86 ACPI board driver remains unchanged in this phase; it will continue to
discover devices and publish nodes at runtime.
Note that Zircon's physboot shim, which consumes the bootloader-supplied
devicetree for early kernel/physical memory initialization, remains unchanged.
Bridging dynamic bootloader-supplied runtime parameters (such as board revision
straps) into the static Board DML topology is noted under Non-Static
Considerations and Unknowns.

### Phase 4: Direct Native Compilation & Bedrock Integration

The Driver Framework will be updated consume compiled versions of the Board DML
and Driver DML files directly.

* The `dmlc` will compile Driver DML and Board DML into a native formatthat it
  will use for static binding.  It will still generate `.cml` files that will
  be consumed by the `cmc` compiler.
* The Driver Manager will act to bind drivers according to a static topology.
* Dynamic Binding will be separated from the driver index, and isolated into a
  separate binding program, which can be used by bus nodes.
* Static binding will be separated from the resource resolution as a
  substructure of the driver manager.
* The autogenerated `.bind` and `.cml` files will be removed, and the `dmlc`
   will only emit a temporary cml file, for use by the cmc compiler.



### Phase 5: Additional enhancements

#### Driver Code Generation

The `dmlc` tool generates language-specific scaffolding code (e.g., C++ headers
or Rust modules) that is compiled directly into the driver's binary. This
generated scaffolding automates the boilerplate of resource acquisition (such as
mapping MMIOs, connecting to FIDL services, or registering interrupts) based on
the Driver DML file, but does not own the driver's lifecycle or replace standard
Driver Framework ABI.

Driver authors interact with this scaffolding via strongly-typed accessors
inside their driver's initialization or `Start()` functions (the extension
point). Use of this generated scaffolding will be optional but strongly
encouraged. Complete adoption is expected to be gradual.

#### Integration with Component Framework

The differences between DML and CML files can be resolved over time, especially
if the additions that DML files contain prove to be useful. As more DML
features are adopted by the component framework, more of the static binding
logic can be offloaded from the driver manager by using standard component
framework primitives.

We expect to "upstream" several DML concepts into standard CML and Component
Framework primitives, such as:

* **Filtering support for service capability instances:** Providing a standard
  mechanism for a consumer to distinguish between multiple instances of the
  same service protocol routed from the same provider based on metadata or
  identifiers.
* **Nested Configuration/Metadata Schemas:** Allowing components to define
  rich, structured configuration or payload schemas directly in their
  manifests.

### Non-Static Considerations

Not all aspects of driver instantiation can be known statically at assembly.
There are a few important use cases that need to be explicitly accounted for to
allow the rest of the system to transition to a static topology assumption.

#### Statically Known Drivers on a Dynamic Bus:

Certain USB devices and a number of PCI devices must technically be
"discovered" via a bus protocol, but remain a static component in our
understanding of the product - we can guarantee that they will always be
discovered.  Such devices we can treat just like bus devices who cannot be
dynamically discovered, like devices on an I2C Bus.  The only difference is
that the actual address of the device may change but when using named
connections, the bus driver will have sufficient information to route
connections to the proper device.

This use case covers all devices that require sideband resources from the board;
for example, an SDIO Wi-Fi chip communicates over standard SDIO data pins, but
depends on an out-of-band GPIO interrupt pin from the SoC, a power-enable line
and external reference clocks.
Because the physical wiring is fixed on the board, we treat these as statically
known devices in Board DML, and they do not require global dynamic composite
matching.

#### Board Overlays and Component Variations

We support boards whose hardware configuration can be one of a known set of
options. For example, we might have two different display panels for a board,
and discover which one is present at boot using strap pins or I2C reads.  In
this situation, the board configuration can still be static, just *which*
configuration we are loading may change.  We propose enumerating all possible
board configurations that may be loaded on a device, and validating all of them
at assembly as usual.  The syntax for how to support component variations will
be determined when those boards are migrated, but here is how it may look:

 * All candidate variations (e.g., display_panel_vendor_a and
   display_panel_vendor_b) are declared in the Board DML (via
   conditional/variant groups) so that dmlc and Assembly validate every
   possible variant route at build time and package all candidate drivers into
   the same system image.
 * At boot time, either the topology selector (informed by board-revision/strap
   pins) activates the matching variant subgraph in Driver Manager, or a single
   driver binds and selects the active hardware configuration based on the
   probed strap/I2C ID.

#### Fully Dynamically Discovered Drivers
For truly hotpluggable devices, like
most usb devices, as well as some wlan devices, there are a limited set of
drivers that must be bound dynamically.  Fortunately, these devices have the
following characteristics:

 * They exist on a bus with no other resource requirements, and so have only
   one source of capabilities: the bus driver
 * They have a standard way of identifying themselves

 These characteristics allow us to create a simpler binding program that is
 restricted to the bus driver.  The bus driver is provided with a limited
 driver index, and can dynamically add child nodes that will immediately bind
 to a specified driver, essentially restricting the dynamic binding to isolated
 nodes on the driver graph.

As of yet, there are no plans to support fully dynamic frameworks like ACPI.

## Performance

DML introduces zero runtime overhead. Once all the phases are complete, static
compile-time mapping eliminates dynamic search during boot.

## Ergonomics

### The Board "Roughing Out" Workflow

Instead of writing complex C++ board drivers to wire pins, power rails, and
clocks, engineers "rough out" the board instantly. They map standard hardware
reference schemas (.dts) into a single Board DML file, binding resources
directly by the logical names declared in the driver's DML.

### Reduced Framework Plumbing

By leaning on generated code, developers can avoid manually fetching services,
parsing MMIOs, or registering outgoing directories in `Start()`. The `dmlc`
pipeline generates much of this standard framework plumbing, streamlining a
driver's startup flow.

## Backwards Compatibility

* Coexistence Phase: Legacy composite drivers and bind rules will coexist
  alongside the new DML drivers.

## Security considerations

* Automated Sandboxing: Additional validation of .dml files helps prevent human
  configuration errors. `dmlc` enforces minimum-viable sandboxing
  automatically, compiling Bedrock routing dictionaries directly from declared
  inputs and outputs.

## Privacy considerations

This proposal does not handle user personal data.

## Testing

* Toolchain Verification: `dmlc` will be validated with unit tests ensuring
  correct parsing of schema properties and exact generation of .cm files and
  generated scaffolding.
* System Integration Testing: Board validation tests will ensure that the node
  topology, properties and capability graphs remain constant through
  infrastructure changes.
* Golden Testing: We will incorporate composite node specification and
  configuration goldens based testing to prevent unintentional regressions in
  the generated board bindings.

## Documentation

* Developer Guide: Author a "Getting Started with DML" tutorial showing
  developers how to bring up their first driver from scratch using `fx driver
  create`.
* Reference Specs: Add reference specs for the DML YAML schema and Board DML
  property mapping rules.
* Migration Guide: Author a guide specifically on how to migrate existing
  drivers, bindings, and board configurations to the DML model.

## Drawbacks, alternatives, and unknowns

**Drawbacks**:
* **Added build complexity:** Added complexity to the build system's
  compilation and code-generation stages.
* **Degraded Board Bring-up Porting:** Board bring-up today relies heavily on
  vendor-supplied `.dts` files. Even with Fuchsia-specific binding differences,
  Standard Devicetrees allow straightforward diffing and porting against vendor
  collateral. Because Board DML introduces a completely separate schema, it
  significantly increases the manual verify/translate burden for board bring-up
  engineers upfront.


## Alternatives Considered

### Add to existing CML manifests

We considered building instance naming and typed data schemas directly into
CML. However, iterating on schema definitions inside the core Component
Framework is slow and risky while prototyping. The migration off of bind also
involves adding a number of temporary options, which would introduce a lot of
churn into the cml compiler.  Instead, DML provides a transitional sandbox. We
fully plan to evaluate all successful DML features for upstream inclusion into
CML, with the long-term goal of replacing most of Driver DML.

### Expanding Devicetree with Fuchsia-specific schema

Converting all boards to standard devicetrees and changing bind rules to
compatibility strings would simplify binding, but it drops the service
topological information required for a driver to receive the correct instance.
While the current Devicetree rollout successfully proved the value of
declarative board configuration, adapting Linux devicetrees into
Fuchsia-compatible capabilities still requires a fair bit of effort using
bespoke visitor programs. DML compacts this bring-up process. By re-formatting
devicetrees into a Fuchsia-native format upfront, we directly map hardware
resources to Component Framework capabilities; this immediately validates
driver compatibility at compile time without mutating standard `.dts` files
into a Fuchsia-only dialect.

## Unknowns

* **Schema Evolution and Versioning:** How Driver DML and Board DML handle
  schema evolution and backwards compatibility across API levels is currently
  unspecified.
* **Mismatch Failure Modes:** When a Driver DML and Board DML do not align
  (e.g., missing capability alias or metadata schema mismatch), we expect this
  to result in a hard failure during `dmlc` compilation to avoid silent runtime
  errors. The exact error surfacing and mismatch handling requires further
  design.
* Defining the optimal validation boundaries between the board compilation
  phase and the final System Assembly phase.

## Prior art and references

* RFC-0192: Devicetrees on Fuchsia.
  *  Note that this RFC overrides the necessity of device trees for user space
     drivers.
* Linux Devicetree Specification: Using devicetree nodes for declarative
  hardware representation.
