// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_PLATFORM_DEBUG_H_
#define ZIRCON_KERNEL_INCLUDE_PLATFORM_DEBUG_H_

#include <stdarg.h>
#include <stdbool.h>
#include <sys/types.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

__BEGIN_CDECLS

// Writes |len| bytes of |str| to the debug serial port, blocking as needed
// until the output has been handed off to the UART.  Must be called from a
// context which is allowed to block.
//
// Concurrent calls must be serialized by the caller.  The UART driver wakes
// only one blocked writer per TX interrupt, so multiple concurrent blocking
// writers can leave one of them blocked forever.  Most code should use
// `dlog_serial_write`, which provides this serialization, instead.
//
// If the debug serial port has been suspended (see `platform_serial_suspend`
// and `zx_debug_suspend`), this function writes nothing and returns
// ZX_ERR_SHOULD_WAIT.  The caller may then wait for the port to be resumed using
// `platform_serial_wait_for_resume` and try again.  Callers should not hold
// locks which other writers may need while waiting.  When the system is halting
// (see `platform_serial_discard_writes_while_suspended`), output written while
// suspended is silently discarded and ZX_OK is returned.
//
// Returns ZX_OK if the output was written (or discarded).
zx_status_t rust_platform_dputs_thread(const char* str, size_t len);
static inline zx_status_t platform_dputs_thread(const char* str, size_t len) {
  return rust_platform_dputs_thread(str, len);
}

// Writes |len| bytes of |str| to the debug serial port without blocking.  Safe
// to call from contexts which may not block (e.g. with interrupts disabled).
//
// If the debug serial port has been suspended, the output is silently dropped.
void rust_platform_dputs_irq(const char* str, size_t len);
static inline void platform_dputs_irq(const char* str, size_t len) {
  rust_platform_dputs_irq(str, len);
}

// Returns 1 if a character was retrieved, 0 if no character was retrieved,
// and negative on error.
//
// If |wait| is true, blocks until a character is available.  While the debug
// serial port is suspended, characters already received will still be
// returned, but no new characters will arrive until the port is resumed, so a
// waiting caller may block indefinitely.
int rust_platform_dgetc(char* c, bool wait);
static inline int platform_dgetc(char* c, bool wait) { return rust_platform_dgetc(c, wait); }

// The "p" variants should be available even if the system has panicked.  They
// never block on a suspended debug serial port.

// Polls for a character.  Returns 0 on success, negative value on error.
//
// If the debug serial port is suspended, returns a negative value (no data).
int rust_platform_pgetc(char* c);
static inline int platform_pgetc(char* c) { return rust_platform_pgetc(c); }

// Writes a character to the debug serial port.
//
// If the debug serial port is suspended, the character is silently dropped.
void rust_platform_pputc(char c);
static inline void platform_pputc(char c) { rust_platform_pputc(c); }

struct UartDriverInfo;
bool cpp_uart_enabled(void);
void cpp_uart_set_driver(const void* serial);
size_t cpp_uart_unparse(char* buf, size_t len);
void cpp_uart_get_driver_info(struct UartDriverInfo* out);
void cpp_uart_init_interrupt(uint32_t uart_irq);
void cpp_uart_interrupt(void);
void cpp_uart_enable_rx_interrupt(void);
bool cpp_uart_tx_drained(void);
void cpp_uart_prepare_for_suspend(void);
void cpp_uart_wakeup_from_suspend(void);
bool cpp_uart_read(uint8_t* out_c);
void cpp_uart_write(const char* str, size_t len, bool blocking);
void cpp_uart_pputc(char c);

__END_CDECLS

#endif  // ZIRCON_KERNEL_INCLUDE_PLATFORM_DEBUG_H_
