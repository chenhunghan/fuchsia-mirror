// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_DEFINES_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_DEFINES_H_

#define SHIFT_4K (12)

// TODO(https://fxbug.dev/42146863): Use constants/#defines from libpage
// instead of PAGE_SIZE_SHIFT.
#define PAGE_SIZE_SHIFT (SHIFT_4K)
#define USER_PAGE_SIZE_SHIFT SHIFT_4K

// The maximum cache line seen on any known ARM hardware.
#define MAX_CACHE_LINE 64

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_DEFINES_H_
