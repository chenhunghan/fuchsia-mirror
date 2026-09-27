// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/boot-shim/reboot-reason.h"

#include <array>

namespace boot_shim {
namespace {

// See https://source.android.com/docs/core/architecture/bootloader/boot-reason
constexpr std::string_view kBootArgKey = "androidboot.bootreason";

// Maps an `androidboot.bootreason` value, compared as a single string, to a ZBI reboot reason.
struct RebootReasonMap {
  std::string_view reason;
  zbi_hw_reboot_reason_t value;
  // If true, match any value starting with `reason` (e.g. "reboot,uvlo" matches
  // "reboot,uvlo,pmic,sub"); otherwise the value must equal `reason` exactly.
  bool is_prefix = false;
};

constexpr auto kRebootReasons = std::to_array<RebootReasonMap>({

    // Generally indicates the hardware has its state reset and ramoops/crashlog should retain
    // persistent
    // content.
    {.reason = "warm", .value = ZBI_HW_REBOOT_REASON_WARM},
    {.reason = "reboot,warm", .value = ZBI_HW_REBOOT_REASON_WARM},

    // Generally indicates the memory and the devices retain some state, and the ramoops/crashlog
    // backing
    // store contains persistent content.
    {.reason = "hard", .value = ZBI_HW_REBOOT_REASON_WARM},

    // Generally indicates a full reset of all devices, including memory.
    {.reason = "cold", .value = ZBI_HW_REBOOT_REASON_COLD},
    {.reason = "reboot,cold", .value = ZBI_HW_REBOOT_REASON_COLD},

    {.reason = "watchdog", .value = ZBI_HW_REBOOT_REASON_WATCHDOG},
    {.reason = "reboot,uvlo", .value = ZBI_HW_REBOOT_REASON_BROWNOUT, .is_prefix = true},
    {.reason = "reboot,ocp", .value = ZBI_HW_REBOOT_REASON_BROWNOUT, .is_prefix = true},
    {.reason = "reboot,sys_ldo_ok,pmic", .value = ZBI_HW_REBOOT_REASON_BROWNOUT, .is_prefix = true},
    {.reason = "reboot,smpl_timeout,pmic",
     .value = ZBI_HW_REBOOT_REASON_BROWNOUT,
     .is_prefix = true},
    {.reason = "reboot,master_dc,reset", .value = ZBI_HW_REBOOT_REASON_BROWNOUT},
    {.reason = "reboot,longkey,s2", .value = ZBI_HW_REBOOT_REASON_USER_HARD_RESET},
});

}  // namespace

void RebootReasonItem::Init(const BootProperties& properties, const char* shim_name, FILE* log) {
  auto prop = properties.GetProperty(kBootArgKey);

  // No reboot reason.
  if (prop.is_error()) {
    fprintf(log, "%s: ERROR %.*s was missing, no reboot reason.\n", shim_name,
            static_cast<int>(kBootArgKey.size()), kBootArgKey.data());
    return;
  }

  std::string_view reboot_reason = prop.value();
  if (reboot_reason.empty()) {
    fprintf(log, "%s: ERROR %.*s was empty, no reboot reason.\n", shim_name,
            static_cast<int>(kBootArgKey.size()), kBootArgKey.data());
    return;
  }

  for (const auto& [reason, value, is_prefix] : kRebootReasons) {
    if (is_prefix ? reboot_reason.starts_with(reason) : reboot_reason == reason) {
      fprintf(log, "%s: INFO %.*s was <%.*s>.\n", shim_name, static_cast<int>(kBootArgKey.size()),
              kBootArgKey.data(), static_cast<int>(reboot_reason.size()), reboot_reason.data());
      set_payload(value);
      return;
    }
  }

  fprintf(log, "%s: ERROR %.*s was <%.*s>, no known reboot reason.\n", shim_name,
          static_cast<int>(kBootArgKey.size()), kBootArgKey.data(),
          static_cast<int>(reboot_reason.size()), reboot_reason.data());
}

}  // namespace boot_shim
