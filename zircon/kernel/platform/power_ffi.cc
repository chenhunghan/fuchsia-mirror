// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <platform.h>
#include <zircon/boot/crash-reason.h>

#include <arch/regs.h>
#include <dev/power.h>
#include <kernel/ffi.h>
#include <platform/halt_token.h>

static_assert(sizeof(PlatformAllowDomainPowerDown) == sizeof(uint8_t));
static_assert(alignof(PlatformAllowDomainPowerDown) == alignof(uint8_t));
static_assert(static_cast<uint8_t>(PlatformAllowDomainPowerDown::No) == 0);
static_assert(static_cast<uint8_t>(PlatformAllowDomainPowerDown::Yes) == 1);

static_assert(sizeof(platform_halt_action) == sizeof(uint32_t));
static_assert(alignof(platform_halt_action) == alignof(uint32_t));
static_assert(HALT_ACTION_HALT == 0);
static_assert(HALT_ACTION_REBOOT == 1);
static_assert(HALT_ACTION_REBOOT_BOOTLOADER == 2);
static_assert(HALT_ACTION_REBOOT_RECOVERY == 3);
static_assert(HALT_ACTION_SHUTDOWN == 4);

static_assert(sizeof(zircon_crash_reason_t) == sizeof(uint32_t));
static_assert(alignof(zircon_crash_reason_t) == alignof(uint32_t));
static_assert(static_cast<uint32_t>(ZirconCrashReason::Invalid) == 0);
static_assert(static_cast<uint32_t>(ZirconCrashReason::Unknown) == 1);
static_assert(static_cast<uint32_t>(ZirconCrashReason::NoCrash) == 2);
static_assert(static_cast<uint32_t>(ZirconCrashReason::Oom) == 3);
static_assert(static_cast<uint32_t>(ZirconCrashReason::Panic) == 4);
static_assert(static_cast<uint32_t>(ZirconCrashReason::SoftwareWatchdog) == 5);
static_assert(static_cast<uint32_t>(ZirconCrashReason::UserspaceRootJobTermination) == 6);

// Storage for the global halt token singleton
HaltToken HaltToken::g_instance;

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_platform_specific_halt(platform_halt_action suggested_action,
                                                  zircon_crash_reason_t reason,
                                                  bool halt_on_panic) {
  platform_specific_halt(suggested_action, reason, halt_on_panic);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_platform_panic_start(bool halt_other_cpus) {
  platform_panic_start(halt_other_cpus ? PanicStartHaltOtherCpus::Yes
                                       : PanicStartHaltOtherCpus::No);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_platform_halt_cpu() { platform_halt_cpu(); }

#ifdef __aarch64__

static_assert(sizeof(PlatformCpuStateResult) == 8);
static_assert(alignof(PlatformCpuStateResult) == 4);
static_assert(sizeof(power_cpu_state) == sizeof(uint32_t));
static_assert(static_cast<uint32_t>(power_cpu_state::ON) == 0);
static_assert(static_cast<uint32_t>(power_cpu_state::OFF) == 1);
static_assert(static_cast<uint32_t>(power_cpu_state::ON_PENDING) == 2);
static_assert(static_cast<uint32_t>(power_cpu_state::STARTED) == 3);
static_assert(static_cast<uint32_t>(power_cpu_state::STOPPED) == 4);
static_assert(static_cast<uint32_t>(power_cpu_state::START_PENDING) == 5);
static_assert(static_cast<uint32_t>(power_cpu_state::STOP_PENDING) == 6);
static_assert(static_cast<uint32_t>(power_cpu_state::SUSPENDED) == 7);
static_assert(static_cast<uint32_t>(power_cpu_state::SUSPEND_PENDING) == 8);
static_assert(static_cast<uint32_t>(power_cpu_state::RESUME_PENDING) == 9);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_platform_supports_suspend_cpu() {
  return platform_supports_suspend_cpu();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_platform_suspend_cpu(bool allow_domain) {
  return platform_suspend_cpu(allow_domain ? PlatformAllowDomainPowerDown::Yes
                                           : PlatformAllowDomainPowerDown::No);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_platform_supports_suspend_system() {
  return platform_supports_suspend_system();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_platform_suspend_system() { return platform_suspend_system(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE PlatformCpuStateResult cpp_platform_get_cpu_state(cpu_num_t cpu_id) {
  const zx::result<power_cpu_state> state = platform_get_cpu_state(cpu_id);
  return {
      .status = state.status_value(),
      .state = static_cast<uint32_t>(state.value_or(power_cpu_state::OFF)),
  };
}

#endif  // __aarch64__
}  // extern "C"
