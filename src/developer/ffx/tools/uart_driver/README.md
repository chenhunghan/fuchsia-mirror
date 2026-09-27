# ffx-uart-driver

This directory contains the `ffx-uart-driver` host tool. It acts as a background
daemon that handles communication between `ffx` client tools (like `ffx-uart`)
and Fuchsia devices connected via UART.

The driver runs as a background process, manages multiplexing, provides
reliability protocols (like acknowledgments and retransmissions), and exposes
metrics via a control UNIX socket.

## Directory Structure and Files

* **BUILD.gn**: Defines the GN build rules for compiling the
  `ffx_uart_host_driver` library and the `ffx-uart-driver` binary.
* **src/main.rs**: The entry point for the `ffx-uart-driver`
  executable. It initializes the build version info and runs the driver.
* **src/lib.rs**: Contains the CLI driver execution framework.
  It handles argument parsing, background execution supervision, setting up log
  sinks, managing log rotation, binding the main UNIX socket, writing connection
  metadata, and invoking the core driver implementation.
* **impl/BUILD.gn**: Compiles the core driver logic into the
  `uart_driver_impl` library.
* **impl/src/adapters.rs**: Implements `FDomainTransport`, providing packetized
  message framing over asynchronous Tokio streams for the `fdomain-client` library.
* **impl/src/lib.rs**: The implementation core of the driver daemon. Handles
  socket creation, orphaned process detection and cleanup, routing messages
  between clients and the serial port, executing the ResendSP protocol framing
  and reliability state machines, querying target identity over FDomain, and
  serving metrics on a `.control` socket.
