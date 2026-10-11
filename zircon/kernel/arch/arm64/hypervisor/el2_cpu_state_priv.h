// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_HYPERVISOR_EL2_CPU_STATE_PRIV_H_
#define ZIRCON_KERNEL_ARCH_ARM64_HYPERVISOR_EL2_CPU_STATE_PRIV_H_

#include <lib/arch/arm64/system.h>
#include <lib/id_allocator.h>

#include <kernel/cpu.h>
#include <kernel/mp.h>
#include <ktl/unique_ptr.h>

// Maintains the EL2 state for each CPU.
class El2CpuState {
 public:
  static zx::result<ktl::unique_ptr<El2CpuState>> Create();
  ~El2CpuState();

  // Allocate/free a VMID.
  zx::result<uint16_t> AllocVmid();
  zx::result<> FreeVmid(uint16_t id);

 private:
  arch::ArmVtcrEl2 vtcr_;

  cpu_mask_t cpu_mask_ = 0;
  id_allocator::IdAllocator<uint16_t, UINT16_MAX> vmid_allocator_;

  El2CpuState() = default;

  static zx::result<> OnTask(void* context, cpu_num_t cpu_num);
};

// Allocate and free virtual machine IDs.
zx::result<uint16_t> alloc_vmid();
zx::result<> free_vmid(uint16_t id);

#endif  // ZIRCON_KERNEL_ARCH_ARM64_HYPERVISOR_EL2_CPU_STATE_PRIV_H_
