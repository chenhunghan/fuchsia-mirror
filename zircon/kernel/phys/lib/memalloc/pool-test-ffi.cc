// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "pool-test-ffi.h"

#include <cstddef>
#include <cstdint>
#include <memory>
#include <new>
#include <vector>

namespace memalloc {

extern "C" {

// This sets `pool` to be a pool with baked-in bookkeeping map logically equivalent to that found
// in the `PoolContext` object used for tests.
void cpp_new_test_pool(Pool* pool) {
  using Bookkeeping = std::vector<std::unique_ptr<std::byte[]>>;
  new (pool) Pool([bookkeeping = std::make_unique<Bookkeeping>()](uint64_t addr, uint64_t size) {
    bookkeeping->push_back(std::make_unique<std::byte[]>(size));
    return bookkeeping->back().get();
  });
}

}  // extern "C"

}  // namespace memalloc
