// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/syscalls/hypervisor.h>
#include <zircon/syscalls/object.h>
#include <zircon/syscalls/port.h>
#include <zircon/types.h>

#include <arch/hypervisor.h>
#include <kernel/ffi.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_vcpu_create(Guest* guest, zx_vaddr_t entry, Vcpu** vcpu_out) {
  auto vcpu = Vcpu::Create(*guest, entry);
  if (vcpu.is_error()) {
    return vcpu.status_value();
  }
  *vcpu_out = (*vcpu).release();
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vcpu_destroy(Vcpu* vcpu) { delete vcpu; }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_vcpu_enter(Vcpu* vcpu, zx_port_packet_t* packet) {
  return vcpu->Enter(*packet).status_value();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vcpu_kick(Vcpu* vcpu) { vcpu->Kick(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_vcpu_interrupt(Vcpu* vcpu, uint32_t vector) {
  return vcpu->Interrupt(vector).status_value();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_vcpu_read_state(Vcpu* vcpu, zx_vcpu_state_t* vcpu_state) {
  return vcpu->ReadState(*vcpu_state).status_value();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_vcpu_write_state(Vcpu* vcpu, const zx_vcpu_state_t* vcpu_state) {
  return vcpu->WriteState(*vcpu_state).status_value();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_vcpu_write_io_state(Vcpu* vcpu, const zx_vcpu_io_t* io_state) {
  return vcpu->WriteState(*io_state).status_value();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vcpu_get_info(const Vcpu* vcpu, zx_info_vcpu_t* info_out) {
  *info_out = vcpu->GetInfo();
}

}  // extern "C"
