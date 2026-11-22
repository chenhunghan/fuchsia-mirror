// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <lib/driver/logging/cpp/logger.h>

#include <cstddef>
#include <cstdint>

#ifdef __aarch64__
#include <bind/fuchsia/platform/cpp/bind.h>
#endif
#ifdef __riscv
#include <bind/fuchsia/google/platform/cpp/bind.h>
#endif

#include "virt.h"

namespace fpbus = fuchsia_hardware_platform_bus;
namespace board_virt {

zx::result<> Virt::RtcInit() {
#ifdef __aarch64__
  // From QEMU hw/arm/virt.c memmap[VIRT_RTC].
  constexpr const char* kRtcName = "pl031";
  constexpr uint32_t kRtcVid = bind_fuchsia_platform::BIND_PLATFORM_DEV_VID_GENERIC;
  constexpr uint32_t kRtcPid = bind_fuchsia_platform::BIND_PLATFORM_DEV_PID_GENERIC;
  constexpr uint32_t kRtcDid = bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_RTC_PL031;
  constexpr zx_paddr_t kRtcMmioBase = 0x0901'0000;
  constexpr size_t kRtcMmioSize = 0x1000;
#elifdef __riscv
  // From QEMU hw/riscv/virt.c memmap[VIRT_RTC].
  constexpr const char* kRtcName = "goldfish-rtc";
  constexpr uint32_t kRtcVid = bind_fuchsia_google_platform::BIND_PLATFORM_DEV_VID_GOOGLE;
  constexpr uint32_t kRtcPid = bind_fuchsia_google_platform::BIND_PLATFORM_DEV_PID_GOLDFISH;
  constexpr uint32_t kRtcDid = bind_fuchsia_google_platform::BIND_PLATFORM_DEV_DID_GOLDFISH_RTC;
  constexpr zx_paddr_t kRtcMmioBase = 0x0010'1000;
  constexpr size_t kRtcMmioSize = 0x1000;
#else
#error "Unsupported architecture"
#endif

  fpbus::Mmio rtc_mmio;
  rtc_mmio.base() = kRtcMmioBase;
  rtc_mmio.length() = kRtcMmioSize;

  fpbus::Node rtc_dev;
  rtc_dev.name() = kRtcName;
  rtc_dev.vid() = kRtcVid;
  rtc_dev.pid() = kRtcPid;
  rtc_dev.did() = kRtcDid;
  rtc_dev.mmio() = {{rtc_mmio}};

  fidl::Arena<> fidl_arena;
  fdf::Arena arena('RTC_');
  auto result = pbus_.buffer(arena)->NodeAdd(fidl::ToWire(fidl_arena, rtc_dev));
  if (!result.ok()) {
    fdf::error("NodeAdd {} request failed: {}", kRtcName, result.FormatDescription().data());
    return zx::error(result.status());
  }
  if (result->is_error()) {
    fdf::error("NodeAdd {} failed: {}", kRtcName, zx_status_get_string(result->error_value()));
    return zx::error(result->error_value());
  }

  return zx::ok();
}

}  // namespace board_virt
