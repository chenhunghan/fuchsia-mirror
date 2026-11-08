// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_ENERGY_MODEL_H_
#define ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_ENERGY_MODEL_H_

#include <lib/fit/function.h>
#include <lib/power-management/power-level-controller.h>
#include <lib/power-management/types.h>
#include <lib/zx/result.h>
#include <stdint.h>
#include <stdlib.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <array>
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <limits>
#include <optional>
#include <span>
#include <string_view>
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
#include <fbl/vector.h>

namespace power_management {

// forward declarations.
class EnergyModel;
class PowerDomain;
class PowerDomainSet;
class PowerLevel;
class PowerLevelTransition;

struct ProcessorPowerLevel {
  uint64_t options;
  uint64_t processing_rate;
  uint64_t power_coefficient_nw;
  ControlInterface control_interface;
  uint64_t control_argument;
  const char* diagnostic_name;
};

static_assert(sizeof(ProcessorPowerLevel) == 48);
static_assert(alignof(ProcessorPowerLevel) == 8);
static_assert(offsetof(ProcessorPowerLevel, options) == 0);
static_assert(offsetof(ProcessorPowerLevel, processing_rate) == 8);
static_assert(offsetof(ProcessorPowerLevel, power_coefficient_nw) == 16);
static_assert(offsetof(ProcessorPowerLevel, control_interface) == 24);
static_assert(offsetof(ProcessorPowerLevel, control_argument) == 32);
static_assert(offsetof(ProcessorPowerLevel, diagnostic_name) == 40);

struct ProcessorPowerLevelTransition {
  zx_duration_mono_t latency;
  uint64_t energy_nj;
  uint8_t from;
  uint8_t to;
};

static_assert(sizeof(ProcessorPowerLevelTransition) == 24);
static_assert(alignof(ProcessorPowerLevelTransition) == 8);
static_assert(offsetof(ProcessorPowerLevelTransition, latency) == 0);
static_assert(offsetof(ProcessorPowerLevelTransition, energy_nj) == 8);
static_assert(offsetof(ProcessorPowerLevelTransition, from) == 16);
static_assert(offsetof(ProcessorPowerLevelTransition, to) == 17);

constexpr uint64_t kPowerLevelOptionsDomainIndependent = 1ull << 0u;

}  // namespace power_management

#ifdef __clang__
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wreturn-type-c-linkage"
#endif

extern "C" {

power_management::ProcessingRate rust_power_level_to_processing_rate(uint64_t processing_rate,
                                                                     uint64_t max_processing_rate);
uint64_t rust_power_level_from_processing_rate(power_management::ProcessingRate processing_rate,
                                               uint64_t max_processing_rate);
void rust_power_level_init(power_management::PowerLevel* out_level, uint8_t level_index,
                           const power_management::ProcessorPowerLevel& level,
                           uint64_t max_processing_rate);

void rust_energy_model_init(power_management::EnergyModel* model);
void rust_energy_model_move(power_management::EnergyModel* dst, power_management::EnergyModel* src);
void rust_energy_model_move_assign(power_management::EnergyModel* dst,
                                   power_management::EnergyModel* src);
void rust_energy_model_destroy(power_management::EnergyModel* model);
zx_status_t rust_energy_model_create(
    const power_management::ProcessorPowerLevel* levels, size_t level_count,
    const power_management::ProcessorPowerLevelTransition* transitions, size_t transition_count,
    uint64_t max_processing_rate, power_management::EnergyModel& out_model);
uint64_t rust_energy_model_max_processing_rate_scale(const power_management::EnergyModel* model);
power_management::ProcessingRate rust_energy_model_to_processing_rate(
    const power_management::EnergyModel* model, uint64_t processing_rate);
uint64_t rust_energy_model_from_processing_rate(const power_management::EnergyModel* model,
                                                power_management::ProcessingRate processing_rate);
const power_management::PowerLevel* rust_energy_model_levels(
    const power_management::EnergyModel* model, size_t* out_len);
size_t rust_energy_model_idle_power_levels(const power_management::EnergyModel* model);
bool rust_energy_model_max_idle_power_level(const power_management::EnergyModel* model,
                                            uint8_t* out);
bool rust_energy_model_max_idle_power_coefficient_nw(const power_management::EnergyModel* model,
                                                     uint64_t* out);
bool rust_energy_model_max_idle_power_level_interface(const power_management::EnergyModel* model,
                                                      power_management::ControlInterface* out);
power_management::ProcessingRate rust_energy_model_max_processing_rate(
    const power_management::EnergyModel* model);
const power_management::PowerLevelTransition* rust_energy_model_transitions(
    const power_management::EnergyModel* model, size_t* out_len);
bool rust_energy_model_find_power_level(const power_management::EnergyModel* model,
                                        uint64_t interface_id, uint64_t control_argument,
                                        uint8_t* out_level);
const power_management::PowerLevel* rust_energy_model_find_active_power_level_for_rate(
    const power_management::EnergyModel* model, power_management::ProcessingRate processing_rate);

zx_status_t rust_power_domain_set_create(const fbl::RefPtr<power_management::PowerDomain>* domains,
                                         size_t domain_count,
                                         power_management::PowerDomainSet* out_set);
void rust_power_domain_set_create_for_test(const fbl::RefPtr<power_management::PowerDomain>& domain,
                                           power_management::PowerDomainSet* out_set);
power_management::PowerDomain* rust_power_domain_set_find_by_domain_id(
    const power_management::PowerDomainSet* set, uint32_t domain_id);
power_management::PowerDomain* rust_power_domain_set_find_by_cpu_num(
    const power_management::PowerDomainSet* set, uint32_t cpu_num);
uint64_t rust_power_domain_set_lookup_active_power_coefficient(
    const power_management::PowerDomainSet* set, uint32_t cpu_num,
    power_management::ProcessingRate processing_rate);
uint64_t rust_power_domain_set_lookup_power_cost(const power_management::PowerDomainSet* set,
                                                 uint32_t cpu_num,
                                                 power_management::ProcessingRate processing_rate);
bool rust_power_domain_set_lookup_power_level(const power_management::PowerDomainSet* set,
                                              uint32_t cpu_num,
                                              power_management::ProcessingRate processing_rate,
                                              uint8_t* out_level);
power_management::Utilization rust_power_domain_set_lookup_total_normalized_utilization(
    const power_management::PowerDomainSet* set, uint32_t cpu_num);
size_t rust_power_domain_set_count(const power_management::PowerDomainSet* set);
bool rust_power_domain_set_is_empty(const power_management::PowerDomainSet* set);

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
#ifdef UNITTESTS_ENABLED
// Creates a test `PowerDomain` with a no-op `PowerLevelController` and returns a raw pointer
// exported via `fbl::ExportToRawPtr` with an acquired reference count that must be adopted by the
// caller (e.g., via `RefPtr::try_from_raw` in Rust) to avoid leaking the test domain, or `nullptr`
// on allocation failure.
power_management::PowerDomain* cpp_power_domain_create_for_test(
    uint32_t domain_id, uint64_t cpu_mask, power_management::EnergyModel* model);
#endif
#endif

}  // extern "C"

#ifdef __clang__
#pragma clang diagnostic pop
#endif

namespace power_management {

// Kernel representation of `zx_processor_power_level_t` with useful accessors and option support.
class PowerLevel {
 public:
  enum Type : bool {
    // Entity is not eligible for active work.
    kIdle,

    // Entity is eligible for work, but the rate at which work is completed is determined by the
    // active power level.
    kActive,
  };

  // Scale for processing rates communicated to/from the userspace limits interface.
  static constexpr uint64_t kUserProcessingRateScale = 1000u;

  // Normalizes an integer processing rate relative to the maximum rate.
  static ProcessingRate ToProcessingRate(uint64_t processing_rate, uint64_t max_processing_rate) {
    return rust_power_level_to_processing_rate(processing_rate, max_processing_rate);
  }
  static uint64_t FromProcessingRate(ProcessingRate processing_rate, uint64_t max_processing_rate) {
    return rust_power_level_from_processing_rate(processing_rate, max_processing_rate);
  }

  constexpr PowerLevel() = default;
  explicit PowerLevel(uint8_t level_index, const ProcessorPowerLevel& level,
                      uint64_t max_processing_rate = 0) {
    rust_power_level_init(this, level_index, level, max_processing_rate);
  }

  // Power level type. Idle and Active power levels are orthogonal, that is, an entity may be idle
  // while keepings its active power level unchanged. This means that the actual power level of
  // an entity should be determined by the tuple <Idle Power Level*, Active Power Level>, where if
  // `Idle Power Level`is absent then that means that the entity is active and the active power
  // level should be used.
  //
  // This situation happens for example, when a CPU transitions from an active power level A
  // (which may be interpreted as a known OPP or P-State) into an idle state, such as suspension,
  // idle thread or even powering it off.
  constexpr Type type() const { return processing_rate_ == 0 ? Type::kIdle : kActive; }

  // Processing rate when this power level is active. This is key to determining the available
  // bandwidth of the entity.
  constexpr ProcessingRate processing_rate() const { return processing_rate_; }

  // Relative to the system power consumption, determines how much power is being consumed at this
  // level. This allows determining if this power level should be a candidate when operating under
  // a given energy budget.
  constexpr uint64_t power_coefficient_nw() const { return power_coefficient_nw_; }

  // Power cost of this power level, normalized by the rate of this power level.
  constexpr uint64_t power_cost_nw_per_rate() const { return power_cost_nw_per_rate_; }

  // ID of the interface handling transitions for TO this power level.
  constexpr ControlInterface control() const { return control_; }

  // Argument to be interpreted by the control interface in order to transition to this level.
  //
  // The control interface is only aware of this arguments, and power levels are identified by
  // this argument.
  constexpr uint64_t control_argument() const { return control_argument_; }

  // This level may be transitioned in a per cpu basis, without affecting other entities in the
  // same power domain.
  constexpr bool TargetsCpus() const {
    return (options_ & kPowerLevelOptionsDomainIndependent) != 0;
  }

  // This level may be transitioned in a per power domain basis, that is, all other entities in
  // the power domain will be transitioned together.
  //
  // This means that underlying hardware elements are share and it is not possible to transition a
  // single member of the power domain.
  constexpr bool TargetsPowerDomain() const {
    return (options_ & kPowerLevelOptionsDomainIndependent) == 0;
  }

  // Name used to identify this power level, for diagnostic purposes.
  constexpr std::string_view name() const { return {name_.data(), name_len_}; }

  // Power Level as understood from the original model perspective.
  constexpr uint8_t level() const { return level_; }

 private:
  // Options.
  uint64_t options_{};

  // Control interface used to transition to this level.
  ControlInterface control_{};

  // Argument to be provided to the control interface.
  uint64_t control_argument_{0};

  // Processing rate.
  ProcessingRate processing_rate_{0};

  // Power coefficient in nanowatts.
  uint64_t power_coefficient_nw_{0};

  // Power cost. Memoized value of power_coefficient_nw_ / processing_rate_;
  uint64_t power_cost_nw_per_rate_{0};

  std::array<char, ZX_MAX_NAME_LEN> name_{};
  size_t name_len_{0};

  // Level as described in the model shared with user.
  uint8_t level_{0};

  static void CheckLayout() {
    static_assert(sizeof(PowerLevel) == 96);
    static_assert(alignof(PowerLevel) == 8);
    static_assert(offsetof(PowerLevel, options_) == 0);
    static_assert(offsetof(PowerLevel, control_) == 8);
    static_assert(offsetof(PowerLevel, control_argument_) == 16);
    static_assert(offsetof(PowerLevel, processing_rate_) == 24);
    static_assert(offsetof(PowerLevel, power_coefficient_nw_) == 32);
    static_assert(offsetof(PowerLevel, power_cost_nw_per_rate_) == 40);
    static_assert(offsetof(PowerLevel, name_) == 48);
    static_assert(offsetof(PowerLevel, name_len_) == 80);
    static_assert(offsetof(PowerLevel, level_) == 88);
  }
};

// Represents an entry in a transition matrix, where the position in the matrix denotes
// the source and target power level. This constructs just denotes the properties of that
// cell.
class PowerLevelTransition {
 public:
  // Returns an invalid transition.
  static constexpr PowerLevelTransition Invalid() { return {}; }
  static constexpr PowerLevelTransition Zero() {
    return PowerLevelTransition(ProcessorPowerLevelTransition{.latency = 0, .energy_nj = 0});
  }

  constexpr PowerLevelTransition() = default;
  explicit constexpr PowerLevelTransition(const ProcessorPowerLevelTransition& transition)
      : latency_(transition.latency), energy_cost_nj_(transition.energy_nj) {}

  // Latency for transitioning from a given level to another.
  constexpr zx_duration_mono_t latency() const { return latency_; }

  // Energy cost in nano joules(nj) for transition from a given level to another.
  constexpr uint64_t energy_cost_nj() const { return energy_cost_nj_; }

  // Whether the transition is valid or not.
  explicit constexpr operator bool() const {
    return latency_ != Invalid().latency() && energy_cost_nj_ != Invalid().energy_cost_nj_;
  }

 private:
  // Time required for the transition to take effect. In some cases it may mean for the actual
  // voltage to stabilize.
  zx_duration_mono_t latency_ = ZX_TIME_INFINITE;

  // Amount of energy consumed to perform the transition.
  uint64_t energy_cost_nj_ = std::numeric_limits<uint64_t>::max();

  static void CheckLayout() {
    static_assert(sizeof(PowerLevelTransition) == 16);
    static_assert(alignof(PowerLevelTransition) == 8);
    static_assert(offsetof(PowerLevelTransition, latency_) == 0);
    static_assert(offsetof(PowerLevelTransition, energy_cost_nj_) == 8);
  }
};

// Represents a view of the `zx_processor_power_level_transition_t` array as
// a matrix. As per a view's concept, this view is only valid so long the original
// object remains valid and it's tied to its lifecycle.
//
// Additionally transition matrix are required to be squared matrixes, since
// they describe transition from every existent level to every other level.
struct TransitionMatrix {
 public:
  constexpr TransitionMatrix(const TransitionMatrix& other) = default;

  constexpr std::span<const PowerLevelTransition> operator[](size_t index) const {
    return transitions_.subspan(index * num_rows_, num_rows_);
  }

 private:
  friend EnergyModel;
  TransitionMatrix(std::span<const PowerLevelTransition> transitions, size_t num_rows)
      : transitions_(transitions), num_rows_(num_rows) {
    ZX_DEBUG_ASSERT(!transitions_.empty());
    ZX_DEBUG_ASSERT(num_rows_ != 0);
    ZX_DEBUG_ASSERT(transitions_.size() % num_rows_ == 0);
    ZX_DEBUG_ASSERT(transitions.size() / num_rows_ == num_rows_);
  }

  const std::span<const PowerLevelTransition> transitions_;
  const size_t num_rows_;
};

// EnergyModel describes the power consumption rates of the available active and
// idle states a processor may enter, which interfaces to use to effect state
// transitions, and properties of the state transitions, such as energy cost and
// latency. This information is used to make efficient scheduling and load
// balancing decisions that meet the power vs. performance tradeoffs specified
// by a product.
//
// An energy model is constant once initialized. Updating the active energy
// model requires replacing it with a new instance and incrementing a generation
// count to detect stale values derived from the previous energy model.
class EnergyModel {
 public:
  static constexpr size_t kStorageSize = 88;

  static zx::result<EnergyModel> Create(std::span<const ProcessorPowerLevel> levels,
                                        std::span<const ProcessorPowerLevelTransition> transitions,
                                        uint64_t max_processing_rate = 0) {
    EnergyModel model;
    const zx_status_t status =
        rust_energy_model_create(levels.data(), levels.size(), transitions.data(),
                                 transitions.size(), max_processing_rate, model);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(std::move(model));
  }

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

  // Scale used to normalize raw integer rates into fixed-point ProcessingRate.
  uint64_t max_processing_rate_scale() const {
    return rust_energy_model_max_processing_rate_scale(this);
  }
  ProcessingRate ToProcessingRate(uint64_t processing_rate) const {
    return rust_energy_model_to_processing_rate(this, processing_rate);
  }
  uint64_t FromProcessingRate(ProcessingRate processing_rate) const {
    return rust_energy_model_from_processing_rate(this, processing_rate);
  }

  // All power levels described in the model, sorted by processing power and energy consumption.
  //
  // (1) The processing rate of power level i is less or equal than the processing rate of power
  // level j, where i <= j.
  //
  // (2) The processing rate of power level i is equal to the processing rate of power level j and
  // the energy cost of power level i is less than or equal to the energy cost of power level j,
  // where i <= j.
  std::span<const PowerLevel> levels() const {
    size_t len = 0;
    const PowerLevel* data = rust_energy_model_levels(this, &len);
    return {data, len};
  }

  // Following the same rules as `levels()` but returns only the set of power levels whose type is
  // `PowerLevel::Type::kIdle`. This set may be empty.
  std::span<const PowerLevel> idle_levels() const {
    return levels().subspan(0, rust_energy_model_idle_power_levels(this));
  }

  // Returns the idle power level with the maximum power consumption.
  std::optional<uint8_t> max_idle_power_level() const {
    uint8_t level = 0;
    if (rust_energy_model_max_idle_power_level(this, &level)) {
      return level;
    }
    return std::nullopt;
  }

  // Returns the power coefficient of the idle power level with the maximum power consumption. This
  // idle power level typically corresponds to clock gating, such that the power consumption is
  // almost entirely leakage power loss.
  std::optional<uint64_t> max_idle_power_coefficient_nw() const {
    uint64_t coeff = 0;
    if (rust_energy_model_max_idle_power_coefficient_nw(this, &coeff)) {
      return coeff;
    }
    return std::nullopt;
  }

  // Returns the control interface of the idle power level with the maximum power consumption.
  std::optional<ControlInterface> max_idle_power_level_interface() const {
    ControlInterface iface{};
    if (rust_energy_model_max_idle_power_level_interface(this, &iface)) {
      return iface;
    }
    return std::nullopt;
  }

  // Returns the processing rate of the fastest power level.
  ProcessingRate max_processing_rate() const { return rust_energy_model_max_processing_rate(this); }

  // Following the same rules as `levels()` but returns only the set of power levels whose type is
  // `PowerLevel::Type::kActive`. This set may be empty.
  std::span<const PowerLevel> active_levels() const {
    return levels().subspan(rust_energy_model_idle_power_levels(this));
  }

  // Returns a transition matrix, where the entry <i,j> represents the transition costs for
  // transitioning from i to j.
  TransitionMatrix transitions() const {
    size_t len = 0;
    const PowerLevelTransition* data = rust_energy_model_transitions(this, &len);
    return TransitionMatrix({data, len}, levels().size());
  }

  std::optional<uint8_t> FindPowerLevel(ControlInterface interface_id,
                                        uint64_t control_argument) const {
    uint8_t level = 0;
    if (rust_energy_model_find_power_level(this, static_cast<uint64_t>(interface_id),
                                           control_argument, &level)) {
      return level;
    }
    return std::nullopt;
  }

  // Returns the lowest active power level that is compatible with (i.e. has a
  // processing rate greater or equal to) the given processing rate. However, to
  // simplify power calculations, the maximum active power level is returned if
  // the given rate exceeds the maximum processing rate for the power domain.
  const PowerLevel* FindActivePowerLevelForRate(ProcessingRate processing_rate) const {
    return rust_energy_model_find_active_power_level_for_rate(this, processing_rate);
  }

 private:
#ifdef _KERNEL
  [[maybe_unused]] OpaqueStorage<kStorageSize, 8> storage_{};
#else
  [[maybe_unused]] alignas(8) uint8_t storage_[kStorageSize]{};
#endif

  static void CheckLayout() {
    static_assert(sizeof(EnergyModel) == 88);
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
  // and innaccuracy is acceptable.
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
  // updated via fetch_add, which only is only supported for fundamental types.
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

  // Creates a PowerDomainSet from a span of PowerDomain references.
  // Validates that domain count is <= kMaxPowerDomains, all pointers are non-null,
  // each domain has a non-zero CPU mask, and no two domains share a domain ID or
  // have intersecting CPU masks.
  static zx::result<PowerDomainSet> Create(std::span<const fbl::RefPtr<PowerDomain>> domains) {
    PowerDomainSet set;
    const zx_status_t status = rust_power_domain_set_create(domains.data(), domains.size(), &set);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(std::move(set));
  }

  static zx::result<PowerDomainSet> Create(
      std::initializer_list<fbl::RefPtr<PowerDomain>> domains) {
    return Create(std::span<const fbl::RefPtr<PowerDomain>>{domains.begin(), domains.size()});
  }

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

  // Returns a borrowed pointer to the power domain for the given CPU id, or nullptr if there isn't
  // one. Returns a raw pointer to avoid unnecessary ref count changes in contexts where the set is
  // guaranteed not to change and maintain the lifetime of its power domain elements.
  PowerDomain* FindByCpuNum(uint32_t cpu_num) const {
    return rust_power_domain_set_find_by_cpu_num(this, cpu_num);
  }

  // Looks up the active power coefficient for the given CPU operating at the given processing rate.
  // Returns 0 if there is no power domain for the given CPU or if there is no active power level
  // that lower bounds the given processing rate.
  uint64_t LookupActivePowerCoefficient(uint32_t cpu_num, ProcessingRate processing_rate) const {
    return rust_power_domain_set_lookup_active_power_coefficient(this, cpu_num, processing_rate);
  }

  uint64_t LookupPowerCost(uint32_t cpu_num, ProcessingRate processing_rate) const {
    return rust_power_domain_set_lookup_power_cost(this, cpu_num, processing_rate);
  }

  std::optional<uint8_t> LookupPowerLevel(uint32_t cpu_num, ProcessingRate processing_rate) const {
    uint8_t level = 0;
    if (rust_power_domain_set_lookup_power_level(this, cpu_num, processing_rate, &level)) {
      return level;
    }
    return std::nullopt;
  }

  Utilization LookupTotalNormalizedUtilization(uint32_t cpu_num) const {
    return rust_power_domain_set_lookup_total_normalized_utilization(this, cpu_num);
  }

  // Visits each non-empty power domain element with the given callable.
  template <typename Visitor>
  void Visit(Visitor&& visitor) const {
    for (const auto& element : domains_) {
      if (element) {
        visitor(element);
      }
    }
  }

  // Swaps the elements of the given power domain sets.
  friend constexpr void swap(PowerDomainSet& a, PowerDomainSet& b) {
    std::swap(a.domains_, b.domains_);
  }

  // Returns a const reference to the underlying power domain array.
  constexpr const ArrayType& domains() const { return domains_; }

  // Returns the count of non-empty array elements.
  size_t count() const { return rust_power_domain_set_count(this); }

  // Returns true if all of the array elements are empty.
  bool is_empty() const { return rust_power_domain_set_is_empty(this); }

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
