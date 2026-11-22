// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_PLATFORM_DEBUG_H_
#define ZIRCON_KERNEL_INCLUDE_PLATFORM_DEBUG_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>

__BEGIN_CDECLS

struct UartDriverInfo;
bool cpp_uart_enabled(void);
void cpp_uart_set_driver(const void* serial);
size_t cpp_uart_unparse(char* buf, size_t len);
void cpp_uart_get_driver_info(struct UartDriverInfo* out);
void cpp_uart_init_interrupt(uint32_t uart_irq);
void cpp_uart_interrupt(void);
void cpp_uart_enable_rx_interrupt(void);
bool cpp_uart_tx_drained(bool lockless);
void cpp_uart_prepare_for_suspend(void);
void cpp_uart_wakeup_from_suspend(void);
bool cpp_uart_read(uint8_t* out_c);
void cpp_uart_write(const char* str, size_t len, bool blocking);
void cpp_uart_pputc(char c);

__END_CDECLS

#endif  // ZIRCON_KERNEL_INCLUDE_PLATFORM_DEBUG_H_
