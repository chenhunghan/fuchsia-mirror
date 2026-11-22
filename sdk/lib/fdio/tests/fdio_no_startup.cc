// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/compiler.h>
#include <zircon/types.h>

// This takes the place of the libc startup hook that fdio defines, so fdio is
// never told to set itself up in a process this is linked into. The retain
// attribute keeps the linker from discarding the definition, since nothing in
// the executable refers to it.
//
// NOLINTNEXTLINE(bugprone-reserved-identifier)
extern "C" [[gnu::retain]] __EXPORT void __libc_extensions_init(uint32_t handle_count,
                                                                zx_handle_t handle[],
                                                                uint32_t handle_info[],
                                                                uint32_t name_count, char** names) {
}
