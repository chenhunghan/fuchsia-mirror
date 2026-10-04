// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/intrin.h>
#include <lib/crypto/entropy/hw_rng_collector.h>
#include <lib/lazy_init/lazy_init.h>
#include <zircon/errors.h>

#include <dev/hw_rng.h>
#include <ktl/atomic.h>

namespace {
lazy_init::LazyInit<crypto::entropy::HwRngCollector, lazy_init::CheckType::None,
                    lazy_init::Destructor::Disabled>
    g_collector;
}  // namespace

namespace crypto {

namespace entropy {

zx_status_t HwRngCollector::GetInstance(Collector** ptr) {
  if (ptr == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  static HwRngCollector* instance = nullptr;
  static ktl::atomic<uint32_t> state{0};  // 0: uninitialized, 1: initializing, 2: initialized

  if (state.load(ktl::memory_order_acquire) != 2) {
    uint32_t expected = 0;
    if (state.compare_exchange_strong(expected, 1, ktl::memory_order_acq_rel)) {
      if (hw_rng_is_registered()) {
        g_collector.Initialize();
        instance = &g_collector.Get();
      }
      state.store(2, ktl::memory_order_release);
    } else {
      while (state.load(ktl::memory_order_acquire) != 2) {
        arch::Yield();
      }
    }
  }

  if (instance != nullptr) {
    *ptr = instance;
    return ZX_OK;
  } else {
    *ptr = nullptr;
    return ZX_ERR_NOT_SUPPORTED;
  }
}

HwRngCollector::HwRngCollector() : Collector("hw_rng", /* entropy_per_1000_bytes */ 8000) {}

size_t HwRngCollector::DrawEntropy(uint8_t* buf, size_t len) {
  // Especially on systems that have RdRand but not RdSeed, avoid parallel
  // accesses. Per the Intel documentation, properly using RdRand to seed a
  // CPRNG requires careful access patterns, to avoid multiple RNG draws from
  // the same physical seed (see https://fxbug.dev/42105846).
  Guard<Mutex> guard(&lock_);

  return hw_rng_get_entropy(buf, len);
}

}  // namespace entropy

}  // namespace crypto
