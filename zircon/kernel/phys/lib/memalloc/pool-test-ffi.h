// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_LIB_MEMALLOC_POOL_TEST_FFI_H_
#define ZIRCON_KERNEL_PHYS_LIB_MEMALLOC_POOL_TEST_FFI_H_

#include <lib/memalloc/pool.h>

namespace memalloc {

extern "C" {

void cpp_new_test_pool(Pool* pool);

}  // extern "C"

}  // namespace memalloc

#endif  // ZIRCON_KERNEL_PHYS_LIB_MEMALLOC_POOL_TEST_FFI_H_
