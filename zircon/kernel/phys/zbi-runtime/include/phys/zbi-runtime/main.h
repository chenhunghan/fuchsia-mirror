// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_ZBI_RUNTIME_INCLUDE_PHYS_ZBI_RUNTIME_MAIN_H_
#define ZIRCON_KERNEL_PHYS_ZBI_RUNTIME_INCLUDE_PHYS_ZBI_RUNTIME_MAIN_H_

#include <lib/arch/ticks.h>

#include <phys/runtime/early-boot.h>

// In ZBI executables, PhysMain is defined to set up the console on stdout and
// then hand off to ZbiMain.  So ZbiMain is the main entry point that a ZBI
// executable defines.  It can use printf (and stdout generally) freely.
[[noreturn]] void ZbiMain(void* zbi_ptr, EarlyBootZbi zbi, arch::EarlyTicks) PHYS_SINGLETHREAD;

#endif  // ZIRCON_KERNEL_PHYS_ZBI_RUNTIME_INCLUDE_PHYS_ZBI_RUNTIME_MAIN_H_
