// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_PLATFORM_H_
#define ZIRCON_KERNEL_INCLUDE_PLATFORM_H_

#include <lib/zx/result.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <dev/power.h>
#include <kernel/cpu.h>

#define BOOT_CPU_ID 0

// Returns true iff this platform supports |platform_suspend_cpu|.
extern "C" bool rust_platform_supports_suspend_cpu();
inline bool platform_supports_suspend_cpu() { return rust_platform_supports_suspend_cpu(); }

// Specifies whether this suspend operation is limited to the calling CPU or may
// power down the encapsulating power domain.
enum class PlatformAllowDomainPowerDown : bool { No, Yes };

// Suspend the calling CPU.
//
// On success, the CPU will enter an implementation-defined suspend state.
//
// If |allow_domain| is No, then the operation will only target the calling CPU.
// However, if Yes is specified, then, depending on the implementation, the
// power domain encapsulating the calling CPU may also be suspended and powered
// down.
//
// Prior to calling this function, it's critical to set up some kind of
// interrupt that will wake the CPU and resume execution.  Upon successful
// suspend and resume, this call returns ZX_OK.
//
// Prior to calling, interrupts must be disabled, preemption must be disabled,
// and the caller must be pinned to the calling CPU.
//
// The calling thread must be a kernel-only thread.  Because this routine saves
// only the minimum required register state, it is an error to call this
// function on a thread that has a "user half" (ThreadDispatcher).
//
// Possible error results include (but are not limited to),
//
// ZX_ERR_NOT_SUPPORTED if unsupported on this platform.
//
// ZX_ERR_ACCESS_DENIED or ZX_ERR_INVALID_ARGS if the suspend power state is
// incompatible with the current power state of other CPUs.  This is a somewhat
// normal error and must be handled by callers.
//
// TODO(https://fxbug.dev/417556024): Consider adding a platform-specific or
// custom result type to differentiates between success-but-did-not-power-down
// and powered-down.
//
// TODO(https://fxbug.dev/417558115): Consider checking the state of the
// secondary CPUs in order to determine which power state value to use so that
// we can eliminate the need to return ZX_ERR_ACCESS_DENIED or
// ZX_ERR_INVALID_ARGS in "normal" situations.
//
// TODO(https://fxbug.dev/417558115): Currently, the caller is responsible for
// determining if pausing/resuming the monotonic clock is necessary and then
// actually doing it.  More thought needs to be given to where that logic should
// live.  Right now, it's done in |IdlePowerThread::UpdateMonotonicClock|,
// however, it might be better to move some of that logic down into the platform
// layer (perhaps within |platform_suspend_cpu|).
extern "C" zx_status_t rust_platform_suspend_cpu(PlatformAllowDomainPowerDown allow_domain);
inline zx_status_t platform_suspend_cpu(PlatformAllowDomainPowerDown allow_domain) {
  return rust_platform_suspend_cpu(allow_domain);
}

// Returns true iff this platform supports |platform_suspend_system|.
extern "C" bool rust_platform_supports_suspend_system();
inline bool platform_supports_suspend_system() { return rust_platform_supports_suspend_system(); }

// Suspend the entire system from the boot CPU after all secondary CPUs have
// been powered off.
//
// Prior to calling this function, it's critical to set up some kind of
// interrupt that will wake the boot CPU and resume execution.
//
// Prior to calling, all secondary CPUs must be powered off, interrupts must be
// disabled, preemption must be disabled, and the caller must be running on the
// boot CPU.
//
// The calling thread must be a kernel-only thread.  Because this routine saves
// only the minimum required register state, it is an error to call this
// function on a thread that has a "user half" (ThreadDispatcher).
//
// Returns ZX_OK on success.  Note that this does not necessarily mean that the
// system actually suspended (e.g. it may return immediately if an interrupt is
// pending).
//
// Returns ZX_ERR_NOT_SUPPORTED if unsupported on this platform.
//
// Returns ZX_ERR_BAD_STATE, without suspending, if any secondary CPU is online
// or the platform cannot confirm that every secondary CPU has powered off.
//
// May return other errors if the platform rejects the request.
extern "C" zx_status_t rust_platform_suspend_system();
inline zx_status_t platform_suspend_system() { return rust_platform_suspend_system(); }

#endif  // ZIRCON_KERNEL_INCLUDE_PLATFORM_H_
