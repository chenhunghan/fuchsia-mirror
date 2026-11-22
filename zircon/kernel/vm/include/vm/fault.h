// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_FAULT_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_FAULT_H_

#include <sys/types.h>

// page fault flags
const uint VMM_PF_FLAG_WRITE = (1u << 0);
// Indicates the fault occurred while the CPU was executing in user mode.
const uint VMM_PF_FLAG_USER = (1u << 1);
const uint VMM_PF_FLAG_GUEST = (1u << 2);
const uint VMM_PF_FLAG_INSTRUCTION = (1u << 3);
const uint VMM_PF_FLAG_NOT_PRESENT = (1u << 4);
const uint VMM_PF_FLAG_HW_FAULT = (1u << 5);  // hardware is requesting a fault
const uint VMM_PF_FLAG_SW_FAULT = (1u << 6);  // software fault
const uint VMM_PF_FLAG_ACCESS = (1u << 7);
const uint VMM_PF_FLAG_FAULT_MASK = (VMM_PF_FLAG_HW_FAULT | VMM_PF_FLAG_SW_FAULT);

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_FAULT_H_
