// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_RUNTIME_INCLUDE_PHYS_RUNTIME_INFO_H_
#define ZIRCON_KERNEL_PHYS_RUNTIME_INCLUDE_PHYS_RUNTIME_INFO_H_

// Defines ArchPhysInfo for the current machine.
// Note: Use `#if defined(...)` / `#elif defined(...)` rather than C++23
// `#elifdef`, which reclient's C++ include scanner fails to evaluate (see
// https://fxbug.dev/581592625).
#if defined(__aarch64__)
#include <phys/runtime/arm64/info.h>
#elif defined(__riscv)
#include <phys/runtime/riscv64/info.h>
#elif defined(__x86_64__)
#include <phys/runtime/x86/info.h>
#else
#error "unsupported machine"
#endif

// The pointer is initialized in ArchSetUp(), so it can be used anywhere after
// earliest start-up.
extern ArchPhysInfo* gArchPhysInfo;

#endif  // ZIRCON_KERNEL_PHYS_RUNTIME_INCLUDE_PHYS_RUNTIME_INFO_H_
