// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_DEV_INIT_H_
#define ZIRCON_KERNEL_INCLUDE_DEV_INIT_H_

#include <zircon/compiler.h>

// Forward-declared; fully declared in <phys/handoff/arch-handoff.h>.
struct ArchPhysHandoff;

__BEGIN_CDECLS

// Platform-specific driver handoff initialization stages.
//
// Defined in //zircon/kernel/platform/generic-arm/dev-init.cc.
void PlatformDriverHandoffEarly(const ArchPhysHandoff& arch_handoff);
void PlatformDriverHandoffPostVm(const ArchPhysHandoff& arch_handoff);
void PlatformDriverHandoffLate(const ArchPhysHandoff& arch_handoff);

__END_CDECLS

#endif  // ZIRCON_KERNEL_INCLUDE_DEV_INIT_H_
