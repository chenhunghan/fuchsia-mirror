// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Entropy collectors feeding the kernel's global CPRNG.

pub mod collector;
pub mod hw_rng_collector;
pub mod jitterentropy_collector;
pub mod quality_test;

pub use collector::{Collector, CollectorBase};
pub use hw_rng_collector::HwRngCollector;
pub use jitterentropy_collector::JitterentropyCollector;
pub use quality_test::early_boot_test;
#[cfg(enable_entropy_collector_test)]
pub use quality_test::{ENTROPY_VMO, ENTROPY_VMO_STREAM_SIZE, ENTROPY_WAS_LOST};
