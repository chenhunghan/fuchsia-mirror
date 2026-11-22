// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_H_

#include <align.h>
#include <arch.h>
#include <lib/ktrace.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/types.h>
#include <zircon/compiler.h>

#include <arch/kernel_aspace.h>
#include <arch/vm.h>
#include <ktl/span.h>
#include <vm/arch_vm_aspace.h>

#ifndef VM_TRACING_LEVEL
#define VM_TRACING_LEVEL 0
#endif

// Evaluates to true if tracing is enabled for the given level.
#define VM_KTRACE_LEVEL_ENABLED(level) ((VM_TRACING_LEVEL) >= (level))

#define VM_MAKE_UNIQUE_TOKEN3(a, b) a##b
#define VM_MAKE_UNIQUE_TOKEN2(a, b) VM_MAKE_UNIQUE_TOKEN3(a, b)
#define VM_MAKE_UNIQUE_TOKEN(a) VM_MAKE_UNIQUE_TOKEN2(a, __LINE__)

#define VM_KTRACE_DURATION(level, string, args...) \
  ktrace::Scope VM_MAKE_UNIQUE_TOKEN(_duration_) = \
      KTRACE_BEGIN_SCOPE_ENABLE(VM_KTRACE_LEVEL_ENABLED(level), "kernel:vm", string, ##args)

// In builds with verbose VM tracing enabled (VM_TRACING_LEVEL >= 1), we emit these events under the
// "kernel:vm" category at the specified VM tracing level. In standard/production builds
// (VM_TRACING_LEVEL = 0), we fallback to emitting under the lightweight "kernel:oom" category
// instead.
//
// The general expectation is that users will enable "kernel:oom" in cases where "kernel:vm" is
// too expensive. Using a compile-time selection here prevents duplicate overlapping slices in
// the trace visualizer when both categories are enabled at runtime, while still providing a
// single category for developers opting for more verbose VM tracing.
#if VM_TRACING_LEVEL >= 1
#define OOM_KTRACE_DURATION(level, string, args...) VM_KTRACE_DURATION(level, string, ##args)
#else
#define OOM_KTRACE_DURATION(level, string, args...) \
  ktrace::Scope VM_MAKE_UNIQUE_TOKEN(_duration_) = KTRACE_BEGIN_SCOPE("kernel:oom", string, ##args)
#endif

#define VM_KTRACE_DURATION_BEGIN(level, string, args...) \
  KTRACE_DURATION_BEGIN_ENABLE(VM_KTRACE_LEVEL_ENABLED(level), "kernel:vm", string, ##args)

#define VM_KTRACE_DURATION_END(level, string, args...) \
  KTRACE_DURATION_END_ENABLE(VM_KTRACE_LEVEL_ENABLED(level), "kernel:vm", string, ##args)

#define VM_KTRACE_FLOW_BEGIN(level, string, flow_id, args...) \
  KTRACE_FLOW_BEGIN_ENABLE(VM_KTRACE_LEVEL_ENABLED(level), "kernel:vm", string, flow_id, ##args)

#define VM_KTRACE_FLOW_END(level, string, flow_id, args...) \
  KTRACE_FLOW_END_ENABLE(VM_KTRACE_LEVEL_ENABLED(level), "kernel:vm", string, flow_id, ##args)

#define VM_KTRACE_INSTANT(level, string, args...) \
  KTRACE_INSTANT_ENABLE(VM_KTRACE_LEVEL_ENABLED(level), "kernel:vm", string, ##args)

// kernel address space
static_assert(KERNEL_ASPACE_BASE + (KERNEL_ASPACE_SIZE - 1) > KERNEL_ASPACE_BASE, "");

// user address space, defaults to below kernel space with a 16MB guard gap on either side
static_assert(USER_ASPACE_BASE + (USER_ASPACE_SIZE - 1) > USER_ASPACE_BASE, "");

// linker script provided start of the kernel virtual image
extern const char __executable_start[];

__BEGIN_CDECLS

// return a pointer to the zero page
__CONST vm_page_t* vm_get_zero_page();

// Whether the virtual heap is enabled.
__CONST bool vm_using_virtual_heap();

// Request the heap dimensions.
__CONST vaddr_t vm_get_kernel_heap_base();
__CONST size_t vm_get_kernel_heap_size();

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_H_
