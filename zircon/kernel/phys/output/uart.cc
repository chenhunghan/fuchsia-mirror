// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <phys/output/uart.h>

#include <ktl/enforce.h>

UartDriver& GetUartDriver() {
  // This is function-local so it can be non-constinit.
  static UartDriver uart;
  return uart;
}

UartDriver& SetUartDriver(const uart::all::Config<>& uart_config) {
  UartDriver& uart = GetUartDriver();
  uart = uart::all::MakeDriver(uart_config);
  uart.Visit([](auto& driver) { driver.Init(); });
  return uart;
}
