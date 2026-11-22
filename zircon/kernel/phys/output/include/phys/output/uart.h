// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_OUTPUT_INCLUDE_PHYS_OUTPUT_UART_H_
#define ZIRCON_KERNEL_PHYS_OUTPUT_INCLUDE_PHYS_OUTPUT_UART_H_

#include <lib/uart/all.h>

using UartDriver = uart::all::KernelDriver<uart::BasicIoProvider, uart::UnsynchronizedPolicy,
                                           uart::NullIrqProvider>;

// Gets the UART driver.  The reference returned is always the same and can be
// kept across SetUartDriver() calls.  FILE{&GetUartDriver()} writes to it.
UartDriver& GetUartDriver();

// Reconfigures and initializes the UART driver, returning GetUartDriver().
// To use it as the console: `gPhysStdout.set_console(FILE{&SetUartDriver(config)})`.
UartDriver& SetUartDriver(const uart::all::Config<>& uart_config);

#endif  // ZIRCON_KERNEL_PHYS_OUTPUT_INCLUDE_PHYS_OUTPUT_UART_H_
