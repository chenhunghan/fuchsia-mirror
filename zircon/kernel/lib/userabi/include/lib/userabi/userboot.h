// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_USERABI_INCLUDE_LIB_USERABI_USERBOOT_H_
#define ZIRCON_KERNEL_LIB_USERABI_INCLUDE_LIB_USERABI_USERBOOT_H_

#ifdef _KERNEL

#include <stddef.h>
#include <stdio.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>

class VmObject;

__BEGIN_CDECLS

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_boot_options_show(bool defaults, FILE* out);

__END_CDECLS

#endif  // _KERNEL

#endif  // ZIRCON_KERNEL_LIB_USERABI_INCLUDE_LIB_USERABI_USERBOOT_H_
