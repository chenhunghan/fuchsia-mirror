// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/power-management/energy-model.h>
#include <lib/power-management/power-level-controller.h>
#include <lib/zx/result.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <type_traits>

#include <fbl/alloc_checker.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>

#ifdef _KERNEL
#include <kernel/ffi.h>
#include <kernel/scheduler.h>
#include <ktl/utility.h>
#else
#ifndef FFI_ALWAYS_INLINE
#ifdef __clang__
#define FFI_ALWAYS_INLINE [[gnu::always_inline]]
#else
#define FFI_ALWAYS_INLINE
#endif
#endif
#endif

static_assert(sizeof(std::atomic<int64_t>) == sizeof(int64_t));
static_assert(alignof(std::atomic<int64_t>) == alignof(int64_t));
static_assert(std::atomic<int64_t>::is_always_lock_free);

static_assert(!std::is_polymorphic_v<power_management::PowerDomain>);

#if defined(_KERNEL) && defined(UNITTESTS_ENABLED)
namespace {

class TestPowerLevelController final : public power_management::PowerLevelController {
 public:
  TestPowerLevelController()
      : PowerLevelController(power_management::ControlInterface::kCpuDriver) {}

  zx::result<uint32_t> Post(const power_management::PowerLevelUpdateRequest& request) final {
    return zx::ok(0);
  }

  uint64_t id() const final { return 0; }
};

}  // namespace
#endif

extern "C" {

void cpp_power_domain_recycle(power_management::PowerDomain* ptr) {
  ZX_DEBUG_ASSERT(static_cast<const void*>(
                      static_cast<const fbl::RefCounted<power_management::PowerDomain>*>(ptr)) ==
                  static_cast<const void*>(ptr));
  delete ptr;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_power_domain_id(const power_management::PowerDomain* domain) {
  return domain->id();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_power_domain_cpus(const power_management::PowerDomain* domain) {
  return domain->cpus();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const power_management::EnergyModel* cpp_power_domain_model(
    const power_management::PowerDomain* domain) {
  return &domain->model();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const power_management::PowerLevelController* cpp_power_domain_controller(
    const power_management::PowerDomain* domain) {
  return domain->controller().get();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_power_domain_scheduler_control_enabled(
    const power_management::PowerDomain* domain) {
  return domain->scheduler_control_enabled();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_power_domain_set_scheduler_control_enabled(
    const power_management::PowerDomain* domain, bool enabled) {
  domain->SetSchedulerControlEnabled(enabled);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const std::atomic<int64_t>* cpp_power_domain_total_normalized_utilization_ptr(
    const power_management::PowerDomain* domain) {
  return &domain->total_normalized_utilization_;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_power_level_controller_is_serving(
    const power_management::PowerLevelController* controller) {
  return controller->is_serving();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE power_management::ControlInterface cpp_power_level_controller_control_interface(
    const power_management::PowerLevelController* controller) {
  return controller->control_interface();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_power_level_controller_get_current_power_level(
    const power_management::PowerLevelController* controller, uint32_t domain_id,
    uint64_t* out_level) {
  const zx::result<uint64_t> result = controller->GetCurrentPowerLevel(domain_id);
  if (result.is_ok()) {
    *out_level = result.value();
  }
  return result.status_value();
}

#ifdef _KERNEL

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE power_management::PowerDomain* cpp_power_domain_create(
    uint32_t domain_id, uint64_t cpu_mask, power_management::EnergyModel* model,
    power_management::PowerLevelController* controller) {
  fbl::RefPtr<power_management::PowerLevelController> ctrl = fbl::ImportFromRawPtr(controller);
  fbl::AllocChecker ac;
  fbl::RefPtr<power_management::PowerDomain> domain =
      fbl::MakeRefCountedChecked<power_management::PowerDomain>(
          &ac, domain_id, static_cast<cpu_mask_t>(cpu_mask), ktl::move(*model), ktl::move(ctrl));
  if (!ac.check()) {
    return nullptr;
  }
  return fbl::ExportToRawPtr(&domain);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_power_management_set_power_domain_set(
    power_management::PowerDomainSet* domain_set) {
  Scheduler::SetPowerDomainSet(ktl::move(*domain_set));
}

#ifdef UNITTESTS_ENABLED
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE power_management::PowerDomain* cpp_power_domain_create_for_test(
    uint32_t domain_id, uint64_t cpu_mask, power_management::EnergyModel* model) {
  fbl::AllocChecker ac;
  fbl::RefPtr controller = fbl::MakeRefCountedChecked<TestPowerLevelController>(&ac);
  if (!ac.check()) {
    return nullptr;
  }
  fbl::RefPtr domain = fbl::MakeRefCountedChecked<power_management::PowerDomain>(
      &ac, domain_id, static_cast<cpu_mask_t>(cpu_mask), ktl::move(*model), ktl::move(controller));
  if (!ac.check()) {
    return nullptr;
  }
  return fbl::ExportToRawPtr(&domain);
}
#endif
#endif

}  // extern "C"
