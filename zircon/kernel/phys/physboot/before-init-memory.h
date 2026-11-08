// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_PHYSBOOT_BEFORE_INIT_MEMORY_H_
#define ZIRCON_KERNEL_PHYS_PHYSBOOT_BEFORE_INIT_MEMORY_H_

// Arch-specific preparation in physboot for heap and address space set-up
// (i.e., before calling InitMemory()).
void ArchPhysbootBeforeInitMemory();

#endif  // ZIRCON_KERNEL_PHYS_PHYSBOOT_BEFORE_INIT_MEMORY_H_
