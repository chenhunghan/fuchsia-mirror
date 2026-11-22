// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_ENERGY_MODEL_H_
#define ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_ENERGY_MODEL_H_

#include <lib/power-management/power-level-controller.h>
#include <lib/power-management/types.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <array>
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <utility>

#ifdef _KERNEL
#include <kernel/cpu.h>
#include <kernel/ffi.h>
#else
using cpu_mask_t = uint64_t;
constexpr cpu_mask_t cpu_num_to_mask(uint64_t num) { return cpu_mask_t{1} << num; }
#endif

#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>

namespace power_management {

class EnergyModel;
class PowerDomain;
class PowerDomainSet;

}  // namespace power_management

#ifdef __clang__
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wreturn-type-c-linkage"
#endif

extern "C" {

void rust_energy_model_init(power_management::EnergyModel* model);
void rust_energy_model_move(power_management::EnergyModel* dst, power_management::EnergyModel* src);
void rust_energy_model_move_assign(power_management::EnergyModel* dst,
                                   power_management::EnergyModel* src);
void rust_energy_model_destroy(power_management::EnergyModel* model);
power_management::ProcessingRate rust_energy_model_max_processing_rate(
    const power_management::EnergyModel* model);
bool rust_energy_model_find_active_power_level(const power_management::EnergyModel* model,
                                               uint64_t interface_id, uint64_t control_argument,
                                               uint8_t* out_level);
bool rust_energy_model_find_active_power_level_for_rate(
    const power_management::EnergyModel* model, power_management::ProcessingRate processing_rate,
    uint8_t* out_level);

void rust_power_domain_set_create_for_test(const fbl::RefPtr<power_management::PowerDomain>& domain,
                                           power_management::PowerDomainSet* out_set);
power_management::PowerDomain* rust_power_domain_set_find_by_domain_id(
    const power_management::PowerDomainSet* set, uint32_t domain_id);
uint64_t rust_power_domain_set_lookup_power_cost(const power_management::PowerDomainSet* set,
                                                 uint32_t cpu_num,
                                                 power_management::ProcessingRate processing_rate);
power_management::Utilization rust_power_domain_set_lookup_total_normalized_utilization(
    const power_management::PowerDomainSet* set, uint32_t cpu_num);
size_t rust_power_domain_set_count(const power_management::PowerDomainSet* set);

void cpp_power_domain_recycle(power_management::PowerDomain* ptr);
uint32_t cpp_power_domain_id(const power_management::PowerDomain* domain);
uint64_t cpp_power_domain_cpus(const power_management::PowerDomain* domain);
const power_management::EnergyModel* cpp_power_domain_model(
    const power_management::PowerDomain* domain);
const power_management::PowerLevelController* cpp_power_domain_controller(
    const power_management::PowerDomain* domain);
bool cpp_power_domain_scheduler_control_enabled(const power_management::PowerDomain* domain);
void cpp_power_domain_set_scheduler_control_enabled(const power_management::PowerDomain* domain,
                                                    bool enabled);
const std::atomic<int64_t>* cpp_power_domain_total_normalized_utilization_ptr(
    const power_management::PowerDomain* domain);

#ifdef _KERNEL
power_management::PowerDomain* cpp_power_domain_create(
    uint32_t domain_id, uint64_t cpu_mask, power_management::EnergyModel* model,
    power_management::PowerLevelController* controller);
void cpp_power_management_set_power_domain_set(power_management::PowerDomainSet* domain_set);
#endif

#if !defined(_KERNEL) || defined(UNITTESTS_ENABLED)
void rust_energy_model_create_for_test(size_t idle_states, size_t active_levels,
                                       power_management::EnergyModel* out_model);
#endif

#ifndef _KERNEL
// Creates a test `PowerDomain` with a no-op `PowerLevelController` and returns a raw pointer
// exported via `fbl::ExportToRawPtr` with an acquired reference count that must be adopted by the
// caller (e.g., via `RefPtr::try_from_raw` in Rust) to avoid leaking the test domain, or `nullptr`
// on allocation failure.
power_management::PowerDomain* cpp_power_domain_create_for_test(
    uint32_t domain_id, uint64_t cpu_mask, power_management::EnergyModel* model);
#endif

}  // extern "C"

#ifdef __clang__
#pragma clang diagnostic pop
#endif

namespace power_management {

// EnergyModel describes the orthogonal active OPPs and idle states a processor
// power domain may enter, along with active-to-active transition costs.
//
// An energy model is immutable once initialized. Updating the active energy
// model requires replacing it with a new instance on a newly registered
// PowerDomain.
class EnergyModel {
 public:
  static constexpr size_t kStorageSize = 104;

#if !defined(_KERNEL) || defined(UNITTESTS_ENABLED)
  // Creates a synthetic energy model with `idle_states` idle states and `active_levels` active
  // operating points (`control_argument == level_index`) for C++ tests.
  static EnergyModel CreateForTest(size_t idle_states, size_t active_levels) {
    EnergyModel model;
    rust_energy_model_create_for_test(idle_states, active_levels, &model);
    return model;
  }
#endif

  EnergyModel() { rust_energy_model_init(this); }
  EnergyModel(const EnergyModel&) = delete;
  EnergyModel& operator=(const EnergyModel&) = delete;
  EnergyModel(EnergyModel&& other) noexcept { rust_energy_model_move(this, &other); }
  EnergyModel& operator=(EnergyModel&& other) noexcept {
    if (this != &other) {
      rust_energy_model_move_assign(this, &other);
    }
    return *this;
  }
  ~EnergyModel() { rust_energy_model_destroy(this); }

  // Returns the processing rate of the fastest active power level.
  ProcessingRate max_processing_rate() const { return rust_energy_model_max_processing_rate(this); }

  std::optional<uint8_t> FindActivePowerLevel(ControlInterface interface_id,
                                              uint64_t control_argument) const {
    uint8_t level = 0;
    if (rust_energy_model_find_active_power_level(this, static_cast<uint64_t>(interface_id),
                                                  control_argument, &level)) {
      return level;
    }
    return std::nullopt;
  }

  // Returns the 0-based index of the lowest active power level whose processing rate is greater
  // than or equal to `processing_rate`, clamping rates above the domain maximum to the fastest
  // active power level.
  std::optional<uint8_t> FindActivePowerLevelForRate(ProcessingRate processing_rate) const {
    uint8_t level = 0;
    if (rust_energy_model_find_active_power_level_for_rate(this, processing_rate, &level)) {
      return level;
    }
    return std::nullopt;
  }

 private:
#ifdef _KERNEL
  [[maybe_unused]] OpaqueStorage<kStorageSize, 8> storage_{};
#else
  [[maybe_unused]] alignas(8) uint8_t storage_[kStorageSize]{};
#endif

  static void CheckLayout() {
    static_assert(sizeof(EnergyModel) == 104);
    static_assert(alignof(EnergyModel) == 8);
  }
};

// PowerDomain establishes the relationship between a set of CPUs, the energy
// model that describes their characteristics, and the power level controller
// responsible for changing the active power levels for the set of CPUs.
//
// Instances of PowerDomain are safe for concurrent use.
class PowerDomain : public fbl::RefCounted<PowerDomain> {
 public:
  PowerDomain(uint32_t id, cpu_mask_t cpus, EnergyModel model,
              fbl::RefPtr<PowerLevelController> controller)
      : cpus_(cpus), id_(id), energy_model_(std::move(model)), controller_(std::move(controller)) {
    ZX_DEBUG_ASSERT(controller_ != nullptr);
  }

  // ID representing the relationship between a set of CPUs and a power model.
  constexpr uint32_t id() const { return id_; }

  // Set of CPUs associated with `model()`.
  constexpr cpu_mask_t cpus() const { return cpus_; }

  // Model describing the behavior of the power domain.
  constexpr const EnergyModel& model() const { return energy_model_; }

  // The total normalized utilization of the set of processors associated with this power domain.
  //
  // Uses relaxed semantics, since the value does not need to synchronize with other memory accesses
  // and inaccuracy is acceptable.
  Utilization total_normalized_utilization() const {
    return Utilization::FromRaw(total_normalized_utilization_.load(std::memory_order_relaxed));
  }

  // Handler for transitions where the target level's control interface is not kernel handled.
  const fbl::RefPtr<PowerLevelController>& controller() const { return controller_; }

  // Returns whether the kernel scheduler should send power level update requests to the controller.
  // This is used by tests to prevent the scheduler from sending power level change requests through
  // the fake control interface that could confuse the test. It does not prevent the kernel from
  // exercising the control interface, however, only whether the scheduler will interact with the
  // control interface to handle utilization changes.
  //
  // Uses relaxed semantics, since this variable will generally be synchronized by the lock
  // protecting each PowerState as it is associated with this PowerDomain. However, an atomic is
  // used to prevent formal data races if the value is read outside of external synchronization,
  // which can't be statically checked internally.
  bool scheduler_control_enabled() const {
    return scheduler_control_enabled_.load(std::memory_order_relaxed);
  }

  // Sets whether the kernel scheduler should send power level update requests through the control
  // interface. When set to false, the scheduler must not send requests to avoid confusing tests.
  void SetSchedulerControlEnabled(bool enabled) const {
    scheduler_control_enabled_.store(enabled, std::memory_order_relaxed);
  }

 private:
  friend class PowerState;
  friend const std::atomic<int64_t>* ::cpp_power_domain_total_normalized_utilization_ptr(
      const PowerDomain* domain);

  const cpu_mask_t cpus_;
  const uint32_t id_;
  const EnergyModel energy_model_;

  // Although this value is exposed publicly with the Utilization type, it needs to be atomically
  // updated via fetch_add, which only is supported for fundamental types.
  mutable std::atomic<int64_t> total_normalized_utilization_{0};

  const fbl::RefPtr<PowerLevelController> controller_ = nullptr;

  mutable std::atomic<bool> scheduler_control_enabled_ = false;
};

// Maintains an array of ref pointers to the power domains in the system. This is used for both the
// power domain registry storage and processor-local caches of the power domain set. Processor-local
// caches avoid cross-processor and central lock contention when consulting a snapshot of the system
// power domains during sensitive scheduling operations.
class PowerDomainSet {
 public:
  static constexpr size_t kMaxPowerDomains = 4;

  using ArrayType = std::array<fbl::RefPtr<PowerDomain>, kMaxPowerDomains>;

  constexpr PowerDomainSet() = default;
  PowerDomainSet(const PowerDomainSet&) = default;
  PowerDomainSet& operator=(const PowerDomainSet&) = default;
  PowerDomainSet(PowerDomainSet&&) = default;
  PowerDomainSet& operator=(PowerDomainSet&&) = default;
  ~PowerDomainSet() = default;

  // Creates a PowerDomainSet with the given PowerDomain as its only entry for testing.
  static PowerDomainSet CreateForTest(const fbl::RefPtr<PowerDomain>& domain) {
    PowerDomainSet set;
    rust_power_domain_set_create_for_test(domain, &set);
    return set;
  }

  // Returns a borrowed pointer to the power domain with the given id, or nullptr if there isn't
  // one. Returns a raw pointer to avoid unnecessary ref count changes in contexts where the set is
  // guaranteed not to change and maintain the lifetime of its power domain elements.
  PowerDomain* FindByDomainId(uint32_t domain_id) const {
    return rust_power_domain_set_find_by_domain_id(this, domain_id);
  }

  uint64_t LookupPowerCost(uint32_t cpu_num, ProcessingRate processing_rate) const {
    return rust_power_domain_set_lookup_power_cost(this, cpu_num, processing_rate);
  }

  Utilization LookupTotalNormalizedUtilization(uint32_t cpu_num) const {
    return rust_power_domain_set_lookup_total_normalized_utilization(this, cpu_num);
  }

  // Returns the count of non-empty array elements.
  size_t count() const { return rust_power_domain_set_count(this); }

 private:
  ArrayType domains_;

  static void CheckLayout() {
    static_assert(sizeof(fbl::RefPtr<PowerDomain>) == 8);
    static_assert(alignof(fbl::RefPtr<PowerDomain>) == 8);
    static_assert(sizeof(PowerDomainSet) == 32);
    static_assert(alignof(PowerDomainSet) == 8);
    static_assert(offsetof(PowerDomainSet, domains_) == 0);
  }
};

}  // namespace power_management

#endif  // ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_ENERGY_MODEL_H_
