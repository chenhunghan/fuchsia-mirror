// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_HYPERVISOR_PV_PRIV_H_
#define ZIRCON_KERNEL_ARCH_X86_HYPERVISOR_PV_PRIV_H_

#include <lib/zx/result.h>
#include <zircon/types.h>

#include <hypervisor/aspace.h>

struct PvClockState;

// Implemented in Rust in arch/x86/hypervisor/pv.rs; exported from pv_ffi.rs.
extern "C" {
zx_status_t rust_pv_clock_update_boot_time(const hypervisor::GuestPhysicalAspace* gpa,
                                           zx_vaddr_t guest_paddr);
zx_status_t rust_pv_clock_reset_clock(PvClockState* pv_clock,
                                      const hypervisor::GuestPhysicalAspace* gpa,
                                      zx_vaddr_t guest_paddr);
void rust_pv_clock_stop_clock(PvClockState* pv_clock);
void rust_pv_clock_update_system_time(PvClockState* pv_clock);
zx_status_t rust_pv_clock_populate_offset(const hypervisor::GuestPhysicalAspace* gpa,
                                          zx_vaddr_t guest_paddr);
}  // extern "C"

// Updates guest boot time.
inline zx::result<> pv_clock_update_boot_time(const hypervisor::GuestPhysicalAspace* gpa,
                                              zx_vaddr_t guest_paddr) {
  return zx::make_result(rust_pv_clock_update_boot_time(gpa, guest_paddr));
}

// Remembers guest physical address for KVM clock system time structure and enables updates
// to guest system time.
inline zx::result<> pv_clock_reset_clock(PvClockState* pv_clock,
                                         const hypervisor::GuestPhysicalAspace* gpa,
                                         zx_vaddr_t guest_paddr) {
  return zx::make_result(rust_pv_clock_reset_clock(pv_clock, gpa, guest_paddr));
}

// Disables updates to guest system time.
inline void pv_clock_stop_clock(PvClockState* pv_clock) { rust_pv_clock_stop_clock(pv_clock); }

// Updates guest system time. If updates disabled does nothing.
inline void pv_clock_update_system_time(PvClockState* pv_clock) {
  rust_pv_clock_update_system_time(pv_clock);
}

// Populates mapping between TSC and wall time per guest request. guest_paddr contains
// physical address of PvClockOffset structure (see pv.rs) where the result should be stored.
inline zx::result<> pv_clock_populate_offset(const hypervisor::GuestPhysicalAspace* gpa,
                                             zx_vaddr_t guest_paddr) {
  return zx::make_result(rust_pv_clock_populate_offset(gpa, guest_paddr));
}

#endif  // ZIRCON_KERNEL_ARCH_X86_HYPERVISOR_PV_PRIV_H_
