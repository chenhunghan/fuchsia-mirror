// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_TEST_HELPER_H_
#define ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_TEST_HELPER_H_

#include <lib/power-management/energy-model.h>
#include <zircon/types.h>

#include <cstddef>
#include <cstdint>
#include <optional>
#include <utility>

#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>

constexpr uint8_t kDefaultActivePowerLevels = 7;
constexpr uint8_t kDefaultIdleStates = 3;

constexpr uint64_t ActivePowerNwForLevel(size_t i) { return (i + 1) * 100; }
constexpr uint64_t StaticPowerNwForLevel(size_t i) { return (i + 1) * 10; }

// Makes a synthetic energy model with `active_levels` active OPPs and `idle_states` idle states
// (`0` = clock-gated WFI, `1..idle_states` = power-gated PSCI states).
inline power_management::EnergyModel MakeFakeEnergyModel(size_t active_levels,
                                                         size_t idle_states = kDefaultIdleStates) {
  return power_management::EnergyModel::CreateForTest(idle_states, active_levels);
}

template <typename... Cpus>
cpu_mask_t MakeCpuMask(Cpus... cpus) {
  cpu_mask_t mask = 0;
  auto set_bit = [&mask](size_t num_cpu) {
    mask |= cpu_num_to_mask(static_cast<uint32_t>(num_cpu));
    return true;
  };
  (set_bit(cpus) && ...);
  return mask;
}

struct FakePowerLevelController : public power_management::PowerLevelController {
  FakePowerLevelController()
      : PowerLevelController(power_management::ControlInterface::kCpuDriver) {}

  zx::result<uint32_t> Post(const power_management::PowerLevelUpdateRequest& pending) final {
    return zx::ok(0);
  }

  zx::result<uint64_t> GetCurrentPowerLevel(uint32_t domain_id) const final {
    if (current_power_level.has_value()) {
      return zx::ok(current_power_level.value());
    }
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  uint64_t id() const final { return 0; }

  std::optional<uint64_t> current_power_level;
};

inline fbl::RefPtr<power_management::PowerDomain> MakePowerDomain(
    uint32_t id, power_management::EnergyModel& model, cpu_mask_t cpus) {
  return fbl::MakeRefCounted<power_management::PowerDomain>(
      id, cpus, std::move(model), fbl::MakeRefCounted<FakePowerLevelController>());
}

template <typename... Cpus>
inline auto MakePowerDomainHelper(uint32_t id, Cpus... cpus) {
  auto model = MakeFakeEnergyModel(kDefaultActivePowerLevels);
  return MakePowerDomain(id, model, MakeCpuMask(cpus...));
}

template <typename... Cpus>
inline auto MakePowerDomainHelper(uint32_t id, power_management::EnergyModel& model, Cpus... cpus) {
  return MakePowerDomain(id, model, MakeCpuMask(cpus...));
}

#endif  // ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_TEST_HELPER_H_
