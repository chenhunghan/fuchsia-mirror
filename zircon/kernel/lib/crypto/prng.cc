// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/crypto/entropy_pool.h>
#include <lib/crypto/prng.h>
#include <lib/crypto/prng_ffi.h>
#include <lib/jitterentropy/jitterentropy.h>
#include <lib/object-constants.h>

#include <openssl/sha.h>

#include <ktl/enforce.h>

// The Rust implementation mirrors these layouts; keep them in sync with
// //zircon/kernel/lib/object-constants/include/lib/object-constants.h.
static_assert(sizeof(crypto::Prng) == kPrngSize);
static_assert(alignof(crypto::Prng) == kPrngAlign);
static_assert(sizeof(crypto::EntropyPool) == kEntropyPoolSize);
static_assert(alignof(crypto::EntropyPool) == kEntropyPoolAlign);
static_assert(sizeof(SHA256_CTX) == kSha256CtxSize);
static_assert(alignof(SHA256_CTX) == kSha256CtxAlign);
static_assert(sizeof(struct rand_data) == kJitterentropyRandDataSize);
static_assert(alignof(struct rand_data) == kJitterentropyRandDataAlign);

namespace crypto {

Prng::Prng(const void* data, size_t size) {
  rust_crypto_prng_init(&storage_, static_cast<const uint8_t*>(data), size,
                        /*thread_safe=*/true);
}

Prng::Prng(const void* data, size_t size, NonThreadSafeTag) {
  rust_crypto_prng_init(&storage_, static_cast<const uint8_t*>(data), size,
                        /*thread_safe=*/false);
}

Prng::~Prng() { rust_crypto_prng_destroy(this); }

void Prng::AddEntropy(const void* data, size_t size) {
  rust_crypto_prng_add_entropy(this, static_cast<const uint8_t*>(data), size);
}

void Prng::SelfReseed() { rust_crypto_prng_self_reseed(this); }

void Prng::Draw(void* out, size_t size) {
  rust_crypto_prng_draw(this, static_cast<uint8_t*>(out), size);
}

uint64_t Prng::RandInt(uint64_t exclusive_upper_bound) {
  return rust_crypto_prng_rand_int(this, exclusive_upper_bound);
}

void Prng::BecomeThreadSafe() { rust_crypto_prng_become_thread_safe(this); }

bool Prng::is_thread_safe() const { return rust_crypto_prng_is_thread_safe(this); }

}  // namespace crypto
