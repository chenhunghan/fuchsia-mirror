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

static_assert(sizeof(PlatformAllowDomainPowerDown) == sizeof(uint8_t));
static_assert(alignof(PlatformAllowDomainPowerDown) == alignof(uint8_t));
static_assert(static_cast<uint8_t>(PlatformAllowDomainPowerDown::No) == 0);
static_assert(static_cast<uint8_t>(PlatformAllowDomainPowerDown::Yes) == 1);

static_assert(sizeof(zircon_crash_reason_t) == sizeof(uint32_t));
static_assert(alignof(zircon_crash_reason_t) == alignof(uint32_t));
static_assert(static_cast<uint32_t>(ZirconCrashReason::Invalid) == 0);
static_assert(static_cast<uint32_t>(ZirconCrashReason::Unknown) == 1);
static_assert(static_cast<uint32_t>(ZirconCrashReason::NoCrash) == 2);
static_assert(static_cast<uint32_t>(ZirconCrashReason::Oom) == 3);
static_assert(static_cast<uint32_t>(ZirconCrashReason::Panic) == 4);
static_assert(static_cast<uint32_t>(ZirconCrashReason::SoftwareWatchdog) == 5);
static_assert(static_cast<uint32_t>(ZirconCrashReason::UserspaceRootJobTermination) == 6);

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
