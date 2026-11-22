// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_IOMMU_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_IOMMU_DISPATCHER_H_

#include <lib/object-constants.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>

class IommuDispatcher;

extern "C" {
zx_status_t cpp_iommu_dispatcher_create(
    const void* iommu, ffi::Uninitialized<KernelHandle<IommuDispatcher>>* handle_out);

void rust_iommu_dispatcher_state_init(void* state, const IommuDispatcher* disp, const void* iommu);
void rust_iommu_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_iommu_dispatcher_state_get_lock(const void* state);
}

class IommuDispatcher final : public Dispatcher {
 public:
  ~IommuDispatcher() final;

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_IOMMU; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final { return ZX_DEFAULT_IOMMU_RIGHTS & ZX_RIGHT_WAIT; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_iommu_dispatcher_create(
      const void* iommu, ffi::Uninitialized<KernelHandle<IommuDispatcher>>* handle_out);

  explicit IommuDispatcher(const void* iommu);
  DISALLOW_COPY_ASSIGN_AND_MOVE(IommuDispatcher);

  OpaqueStorage<kIommuDispatcherStateSize, kIommuDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_IOMMU_DISPATCHER_H_
