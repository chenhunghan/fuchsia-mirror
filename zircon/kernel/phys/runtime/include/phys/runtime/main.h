// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_RUNTIME_INCLUDE_PHYS_RUNTIME_MAIN_H_
#define ZIRCON_KERNEL_PHYS_RUNTIME_INCLUDE_PHYS_RUNTIME_MAIN_H_

#include <lib/arch/ticks.h>
#include <zircon/compiler.h>

#include "early-boot.h"

// This is private to each arch/$cpu/phys directory.  The pointer is
// initialized in ArchSetUp, so it can be used anywhere after earliest startup.
struct ArchPhysInfo;
extern ArchPhysInfo* gArchPhysInfo;

// There's never any such object but the static analysis API requires some
// C++ object to refer to.  The PHYS_SINGLETHREAD marker on any function
// asserts that it is only run in the single thread rooted at PhysMain.
//
// TODO(mcgrathr): If and when code developed for phys environments is also
// shared in multithreaded kernel or user environments then this should
// probably be replaced by a lock/guard template wrapper regime that uses
// specific static analysis annotations and in fancy environments is real
// locks but in single-threaded environments is compiled away stub lock
// types that only do the static analysis.  For now, we take pains to mark
// specific reusable-looking code with PHYS_SINGLETHREAD any place that it
// has single-threaded assumptions.
namespace PhysMainSingleThread {
struct __TA_CAPABILITY("PhysMainSingleThread") Type {};
constexpr Type kInstance;
}  // namespace PhysMainSingleThread
#define PHYS_SINGLETHREAD __TA_REQUIRES(PhysMainSingleThread::kInstance)

// This is the entry point from the assembly code kernel entry point.
// The stack and thread pointer ABIs are fully set up for normal C++ code.
// The first argument is passed along from the boot loader and the second
// is the earliest possible time sample at entry.
extern "C" [[noreturn]] void CppPhysMain(void*, arch::EarlyTicks) PHYS_SINGLETHREAD;

// Performs initial set-up dependent solely on CPU state. This routine should
// be called as early as possible and before UART configuration and
// ArchSetUpZbi() (if called), and is responsible for setting gArchPhysInfo (if
// used).
void ArchSetUpCpu();

// Performs the rest of basic architecture-specific set-up based on ZBI
// information. In contexts where there is a ZBI, this routine should be called
// after ArchSetUpCpu() and UART configuration and as early as possible after
// the ZBI is parsed.
void ArchSetUpZbi(EarlyBootZbi zbi);

// Try to reboot or shut down the machine in a panic situation.
[[noreturn]] void ArchPanicReset();

// Returns the name of the current program, according to the currently
// registered Symbolize object. If no Symbolize has yet been registered, then
// it is assumed that we were in an early set-up context before we have had a
// chance to construct one; in that case, "early-init" is returned.
const char* ProgramName();

// Set up different subsystems for handling a panic. For example, all logs will go
// through serial only.
void PreparePanic();

#endif  // ZIRCON_KERNEL_PHYS_RUNTIME_INCLUDE_PHYS_RUNTIME_MAIN_H_
