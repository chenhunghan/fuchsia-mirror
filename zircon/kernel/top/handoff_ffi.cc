// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/asm.h>
#include <lib/boot-options/boot-options.h>
#include <lib/elfldltl/init-fini.h>
#include <lib/thread-stack/abi.h>
#include <stdint.h>
#include <zircon/assert.h>

#include <memory>
#include <span>

#include <arch/kernel_aspace.h>
#include <kernel/ffi.h>
#include <phys/handoff/boot-constants.h>
#include <phys/handoff/handoff.h>
#include <phys/handoff/zircon-abi-spec.h>

// NOLINTNEXTLINE(bugprone-reserved-identifier)
extern arch::AsmLabel __text_start;  // Defined by the linker script.

// Declared in `<phys/handoff/zircon-abi-spec.h>` as `extern constinit const`
// inside `extern "C"`, so the compiler knows it needs to emit a real
// definition here to satisfy link-time references (from `-e`). Defining it as
// `constexpr` here still allows constant folding inside the kernel, though its
// members are only read by physboot and not by the kernel anyway.
extern "C" constexpr ZirconAbiSpec kZirconAbiSpec = {
    .machine_stack = kMachineStack,
#if __has_feature(shadow_call_stack)
    .shadow_call_stack = kShadowCallStack,
#endif
#if __has_feature(safe_stack)
    .unsafe_stack = kUnsafeStack,
#endif

    .boot_constants{kBootConstants},

    .kernel_aspace_base = KERNEL_ASPACE_BASE,
    .kernel_aspace_size = KERNEL_ASPACE_SIZE,

#if LK_DEBUGLEVEL >= 2
    // Without debug support, this stays nullptr and physboot will panic if the
    // `kernel.debug.boot-spin` boot option is used.
    .debug_boot_spin_ready{gDebugBootSpinReady},
#endif

    // This is just handy for physboot to be able to print out.
    .text_start{__text_start[0]},
};

const BootOptions* BootOptions::Get() { return kBootConstants.boot_options.get(); }

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const uint8_t* cpp_boot_constants_kernel_version_ident(size_t* size) {
  std::string_view version = kBootConstants.kernel_version_ident.get();
  *size = version.size();
  return reinterpret_cast<const uint8_t*>(version.data());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const uint8_t* cpp_boot_constants_kernel_debug_ident(size_t* size) {
  std::string_view ident = kBootConstants.kernel_debug_ident.get();
  *size = ident.size();
  return reinterpret_cast<const uint8_t*>(ident.data());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_boot_constants_hw_reboot_reason() {
  return static_cast<uint32_t>(kBootConstants.hw_reboot_reason);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_boot_constants_bypass_debuglog() {
  return kBootConstants.bypass_debuglog;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const uint8_t* cpp_boot_constants_system_version_string(size_t* size) {
  std::string_view version = kBootConstants.system_version_string.get();
  *size = version.size();
  return reinterpret_cast<const uint8_t*>(version.data());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const uint8_t* cpp_boot_constants_elf_build_id(size_t* size) {
  std::span<const std::byte> build_id = kBootConstants.elf_build_id.get();
  *size = build_id.size();
  return reinterpret_cast<const uint8_t*>(build_id.data());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const uint8_t* cpp_phys_handoff_crashlog(size_t* size) {
  std::string_view crashlog = gPhysHandoff->crashlog.get();
  *size = crashlog.size();
  return reinterpret_cast<const uint8_t*>(crashlog.data());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_phys_vmo_log(const PhysVmo* vmo, const char* prefix) {
  vmo->Log(prefix);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_phys_handoff_call_init_array(const PhysHandoff* handoff) {
  elfldltl::InitFiniInfo<>{handoff->init_array.get()}.callable_init_no_legacy()();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const PhysVmo* cpp_phys_handoff_extra_vmos(const PhysHandoff* handoff,
                                                             size_t* count) {
  std::span<const PhysVmo> extra_vmos = handoff->extra_vmos.get();
  *count = extra_vmos.size();
  return extra_vmos.data();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint8_t* cpp_phys_handoff_nvram(size_t* size) {
  std::span<std::byte> nvram = gPhysHandoff->nvram.get();
  *size = nvram.size();
  return reinterpret_cast<uint8_t*>(nvram.data());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const zbi_topology_node_t* cpp_phys_handoff_cpu_topology(size_t* count) {
  std::span<const zbi_topology_node_t> topology = gPhysHandoff->cpu_topology.get();
  *count = topology.size();
  return topology.data();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const memalloc::Range* cpp_phys_handoff_memory(size_t* count) {
  std::span<const memalloc::Range> memory = gPhysHandoff->memory.get();
  *count = memory.size();
  return memory.data();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const uint8_t* cpp_phys_handoff_efi_memory_attributes(size_t* size) {
  ZX_DEBUG_ASSERT(gPhysHandoff != nullptr);
  std::span<const std::byte> efi_memory_attributes = gPhysHandoff->efi_memory_attributes.get();
  *size = efi_memory_attributes.size();
  return reinterpret_cast<const uint8_t*>(efi_memory_attributes.data());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const MappedMmioRange* cpp_phys_handoff_periph_ranges(size_t* count) {
  std::span<const MappedMmioRange> periph_ranges = gPhysHandoff->periph_ranges.get();
  *count = periph_ranges.size();
  return periph_ranges.data();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_mapped_mmio_range_paddr(const MappedMmioRange& range) {
  return range.paddr();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uintptr_t cpp_mapped_mmio_range_vaddr(const MappedMmioRange& range) {
  return range.vaddr();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE size_t cpp_mapped_mmio_range_size_bytes(const MappedMmioRange& range) {
  return range.size_bytes();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_phys_handoff_platform_id(zbi_platform_id_t* out) {
  std::optional<zbi_platform_id_t> platform_id = gPhysHandoff->platform_id.to_std();
  if (!platform_id) {
    return false;
  }
  *out = *platform_id;
  return true;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const PhysVmar* cpp_phys_handoff_vmars(const PhysHandoff& handoff,
                                                         size_t* count) {
  std::span<const PhysVmar> vmars = handoff.vmars.get();
  *count = vmars.size();
  return vmars.data();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const PhysVmar* cpp_phys_handoff_temporary_vmar(const PhysHandoff& handoff) {
  return handoff.temporary_vmar.get();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const PhysVmar* cpp_phys_handoff_heap_vmar(const PhysHandoff& handoff) {
  return handoff.heap_vmar.has_value() ? &*handoff.heap_vmar : nullptr;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const PhysMapping* cpp_phys_vmar_mappings(const PhysVmar& vmar, size_t* count) {
  std::span<const PhysMapping> mappings = vmar.mappings.get();
  *count = mappings.size();
  return mappings.data();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_phys_vmar_permissions(const PhysVmar& vmar,
                                                 PhysMapping::Permissions* out) {
  std::construct_at(out, vmar.permissions());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_phys_vmar_log(const PhysVmar& vmar, const char* prefix) {
  vmar.Log(prefix);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_phys_mapping_permissions_readable(
    const PhysMapping::Permissions& perms) {
  return perms.readable();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_phys_mapping_permissions_writable(
    const PhysMapping::Permissions& perms) {
  return perms.writable();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_phys_mapping_permissions_executable(
    const PhysMapping::Permissions& perms) {
  return perms.executable();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const crypto::EntropyPool* cpp_phys_handoff_entropy_pool() {
  return gPhysHandoff->entropy_pool.has_value() ? &*gPhysHandoff->entropy_pool : nullptr;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_phys_handoff_reset_entropy_pool() { gPhysHandoff->entropy_pool.reset(); }

}  // extern "C"
