// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_DEBUGLOG_INCLUDE_LIB_DEBUGLOG_H_
#define ZIRCON_KERNEL_LIB_DEBUGLOG_INCLUDE_LIB_DEBUGLOG_H_

// Severity Levels
#define DEBUGLOG_TRACE (0x10)
#define DEBUGLOG_DEBUG (0x20)
#define DEBUGLOG_INFO (0x30)
#define DEBUGLOG_WARNING (0x40)
#define DEBUGLOG_ERROR (0x50)
#define DEBUGLOG_FATAL (0x60)

// Initialize the debuglog subsystem. Called once at extremely early boot.
extern "C" void dlog_init_early();

#endif  // ZIRCON_KERNEL_LIB_DEBUGLOG_INCLUDE_LIB_DEBUGLOG_H_
