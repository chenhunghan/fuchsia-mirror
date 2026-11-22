// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_CRYPTO_INCLUDE_LIB_CRYPTO_PRNG_H_
#define ZIRCON_KERNEL_LIB_CRYPTO_INCLUDE_LIB_CRYPTO_PRNG_H_

#include <lib/object-constants.h>
#include <stddef.h>
#include <stdint.h>

#include <ktl/byte.h>

namespace crypto {

// This exposes a (optionally-)thread-safe cryptographically secure PRNG.
// This PRNG must be seeded with at least 256 bits of "real" entropy before
// being used for cryptographic applications.
//
// The implementation lives in Rust (//zircon/kernel/lib/crypto/prng.rs); this
// class is a thin facade over its storage.
class Prng {
 public:
  // Tag object for constructing a non-thread-safe version.
  struct NonThreadSafeTag {};

  // Construct a thread-safe instance of the PRNG with the byte array at
  // |data| as the initial seed.  |size| is the length of |data| in bytes.
  Prng(const void* data, size_t size);

  // Construct a non-thread-safe instance of the PRNG with the byte array at
  // |data| as the initial seed.  |size| is the length of |data| in bytes.
  Prng(const void* data, size_t size, NonThreadSafeTag);

  ~Prng();

  // Re-seed the PRNG by mixing-in new entropy. |size| is in bytes.
  // |size| MUST NOT be greater than kMaxEntropy.
  // If size is 0, only hash of the current key is used to re-seed.
  void AddEntropy(const void* data, size_t size);

  // Re-seed the PRNG by hash of the current key. This does not mix in new
  // entropy.
  void SelfReseed();

  // Get pseudo-random output of |size| bytes.  Blocks until at least
  // kMinEntropy bytes of entropy have been added to this PRNG.  |size| MUST
  // NOT be greater than kMaxDrawLen.  Identical PRNGs are only guaranteed to
  // produce identical output when given identical inputs.
  void Draw(void* out, size_t size);

  // Return an integer in the range [0, exclusive_upper_bound) chosen
  // uniformly at random.  This is a wrapper for Draw(), and so has the same
  // caveats.
  uint64_t RandInt(uint64_t exclusive_upper_bound);

  // Transitions the PRNG to thread-safe mode.  This asserts that the
  // instance is not yet thread-safe.
  void BecomeThreadSafe();

  // Inspect if this PRNG is thread-safe.
  bool is_thread_safe() const;

  // The minimum amount of entropy (in bytes) the generator requires before
  // Draw will return data.
  static constexpr uint64_t kMinEntropy = 32;

  // The maximum amount of entropy (in bytes) that can be submitted to
  // AddEntropy.  Anything above this will panic.
  static constexpr uint64_t kMaxEntropy = 1ULL << 30;

  // The maximum amount of pseudorandom data (in bytes) that can be drawn in
  // one call to Draw. This the limit imposed by the maximum number of bytes
  // that can be generated with a single key/nonce pair. Each request to Draw
  // uses a different key/nonce pair.  Anything above this will panic.
  static constexpr uint64_t kMaxDrawLen = 1ULL << 38;

 private:
  Prng(const Prng&) = delete;
  Prng& operator=(const Prng&) = delete;

  // Storage for the Rust `crypto::Prng`, constructed in place by rust_crypto_prng_init().
  alignas(kPrngAlign) ktl::byte storage_[kPrngSize];
};

}  // namespace crypto

#endif  // ZIRCON_KERNEL_LIB_CRYPTO_INCLUDE_LIB_CRYPTO_PRNG_H_
