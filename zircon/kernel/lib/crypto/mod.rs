// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel cryptography: the global ChaCha20-based CPRNG, the SHA-256 entropy pool that keys it
//! and the entropy collectors that seed it.

mod boringssl;
pub mod entropy;
pub mod entropy_pool;
pub mod global_prng;
pub mod prng;
mod prng_ffi;

pub use entropy_pool::{EntropyPool, MAX_ENTROPY_SIZE, MIN_ENTROPY_BYTES};
pub use prng::Prng;
