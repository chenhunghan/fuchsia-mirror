// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_CRYPTO_INCLUDE_LIB_CRYPTO_PRNG_FFI_H_
#define ZIRCON_KERNEL_LIB_CRYPTO_INCLUDE_LIB_CRYPTO_PRNG_FFI_H_

#include <lib/crypto/prng.h>
#include <stddef.h>
#include <stdint.h>

extern "C" {

// Rust trampolines (//zircon/kernel/lib/crypto/prng_ffi.rs) backing the crypto::Prng facade.
void rust_crypto_prng_init(void* storage, const uint8_t* data, size_t size, bool thread_safe);
void rust_crypto_prng_destroy(crypto::Prng* prng);
void rust_crypto_prng_add_entropy(crypto::Prng* prng, const uint8_t* data, size_t size);
void rust_crypto_prng_self_reseed(crypto::Prng* prng);
void rust_crypto_prng_draw(crypto::Prng* prng, uint8_t* out, size_t size);
uint64_t rust_crypto_prng_rand_int(crypto::Prng* prng, uint64_t exclusive_upper_bound);
void rust_crypto_prng_become_thread_safe(crypto::Prng* prng);
bool rust_crypto_prng_is_thread_safe(const crypto::Prng* prng);

// Returns the global PRNG singleton (//zircon/kernel/lib/crypto/global_prng.rs).
crypto::Prng* rust_crypto_global_prng_get_instance();

}  // extern "C"

#endif  // ZIRCON_KERNEL_LIB_CRYPTO_INCLUDE_LIB_CRYPTO_PRNG_FFI_H_
