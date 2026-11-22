// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::ksync_kernel::TrackedSpinlock;
use core::convert::Infallible;
use pin_init::{PinInit, pin_data, pin_init};
use zx_types::ZX_MAX_NAME_LEN;

/// State shared by every entropy collector: its debugging name and entropy estimate.
#[pin_data]
pub struct CollectorBase {
    #[pin]
    name: fbl::Name<ZX_MAX_NAME_LEN, TrackedSpinlock>,

    entropy_per_1000_bytes: usize,
}

impl CollectorBase {
    /// Initialize this entropy collector. `name` is used for debugging and
    /// testing, and it may be truncated if it is too long.
    /// `entropy_per_1000_bytes` is the (approximate) amount of min-entropy in
    /// each 1000 bytes of data returned by the entropy source. The amount of
    /// entropy in a byte from the entropy source is generally not an integer.
    /// Quoting the entropy per 1000 bytes supports non-integer values, without
    /// requiring floating-point or fixed-point arithmetic. It is an error if
    /// `entropy_per_1000_bytes` is 0 or is greater than 8000.
    ///
    /// TODO(andrewkrieger): document entropy source quality tests, and reference
    /// that document here, to explain how to find a good value for
    /// entropy_per_1000_bytes.
    pub fn init(
        name: &'static [u8],
        entropy_per_1000_bytes: usize,
    ) -> impl PinInit<Self, Infallible> {
        debug_assert!(entropy_per_1000_bytes > 0);
        debug_assert!(entropy_per_1000_bytes <= 8000);
        pin_init!(Self {
            name <- fbl::Name::init(),
            entropy_per_1000_bytes,
        })
        .pin_chain(move |this| {
            this.name.set(name);
            Ok(())
        })
    }
}

/// A source of entropy.
pub trait Collector: Sync {
    /// Returns the collector's shared state.
    fn base(&self) -> &CollectorBase;

    /// Returns a null-terminated name, in the buffer `name`.
    fn get_name(&self, name: &mut [u8]) {
        self.base().name.get(name);
    }

    /// Fills `buf` with random data from this entropy collector's entropy
    /// source. The bytes that are returned may not be perfectly random, i.e.
    /// they may be statistically dependent or biased. Use the `bytes_needed`
    /// method to determine how may random bytes are needed to collect a certain
    /// amount of entropy.
    fn draw_entropy(&self, buf: &mut [u8]) -> usize;

    /// Returns the number of bytes of random data that should be drawn via
    /// `draw_entropy` to get approximately `bits` bits of entropy. Note: `bits`
    /// must be no larger than 2^20 (= 1048576).
    fn bytes_needed(&self, bits: usize) -> usize {
        // This avoids overflow, and (probably more likely) programming errors.
        debug_assert!(bits <= (1024 * 1024));
        // Round up, to ensure that the result always contains at least the
        // requested amount of entropy.
        (1000 * bits).div_ceil(self.base().entropy_per_1000_bytes)
    }
}

/// Test entropy collector implementation.
#[cfg(ktest)]
#[unittest::suite(name = "entropy_collector")]
mod tests {
    use super::{Collector, CollectorBase};
    use crate::crypto::entropy::hw_rng_collector::HwRngCollector;
    use crate::hw_rng::hw_rng_is_registered;
    use pin_init::{pin_data, pin_init, stack_pin_init};
    use unittest::{assert_true, expect_eq, expect_true};
    use zx_status::Status;

    #[pin_data]
    struct MockCollector {
        #[pin]
        base: CollectorBase,
    }

    impl MockCollector {
        fn init(
            entropy_per_1000_bytes: usize,
        ) -> impl pin_init::PinInit<Self, core::convert::Infallible> {
            pin_init!(Self { base <- CollectorBase::init(b"mock", entropy_per_1000_bytes) })
        }
    }

    impl Collector for MockCollector {
        fn base(&self) -> &CollectorBase {
            &self.base
        }

        fn draw_entropy(&self, _buf: &mut [u8]) -> usize {
            0
        }
    }

    #[doc = "entropy_estimate"]
    #[test]
    fn entropy_estimate_test() {
        stack_pin_init!(let ec_1 = MockCollector::init(1));
        expect_eq!(
            0,
            ec_1.bytes_needed(0),
            "bad entropy estimate (entropy per 1000 bytes = 1, bits = 0"
        );
        expect_eq!(
            1000,
            ec_1.bytes_needed(1),
            "bad entropy estimate (entropy per 1000 bytes = 1, bits = 1"
        );
        expect_eq!(
            2000,
            ec_1.bytes_needed(2),
            "bad entropy estimate (entropy per 1000 bytes = 1, bits = 2"
        );
        expect_eq!(
            1048575000,
            ec_1.bytes_needed(1024 * 1024 - 1),
            "bad entropy estimate (entropy per 1000 bytes = 1, bits = (1024*1024-1)"
        );
        expect_eq!(
            1048576000,
            ec_1.bytes_needed(1024 * 1024),
            "bad entropy estimate (entropy per 1000 bytes = 1, bits = (1024*1024)"
        );

        stack_pin_init!(let ec_2 = MockCollector::init(2));
        expect_eq!(
            0,
            ec_2.bytes_needed(0),
            "bad entropy estimate (entropy per 1000 bytes = 2, bits = 0"
        );
        expect_eq!(
            500,
            ec_2.bytes_needed(1),
            "bad entropy estimate (entropy per 1000 bytes = 2, bits = 1"
        );
        expect_eq!(
            1000,
            ec_2.bytes_needed(2),
            "bad entropy estimate (entropy per 1000 bytes = 2, bits = 2"
        );
        expect_eq!(
            1500,
            ec_2.bytes_needed(3),
            "bad entropy estimate (entropy per 1000 bytes = 2, bits = 3"
        );
        expect_eq!(
            524287500,
            ec_2.bytes_needed(1024 * 1024 - 1),
            "bad entropy estimate (entropy per 1000 bytes = 2, bits = (1024*1024-1)"
        );
        expect_eq!(
            524288000,
            ec_2.bytes_needed(1024 * 1024),
            "bad entropy estimate (entropy per 1000 bytes = 2, bits = (1024*1024)"
        );

        stack_pin_init!(let ec_3999 = MockCollector::init(3999));
        expect_eq!(
            0,
            ec_3999.bytes_needed(0),
            "bad entropy estimate (entropy per 1000 bytes = 3999, bits = 0"
        );
        expect_eq!(
            1,
            ec_3999.bytes_needed(1),
            "bad entropy estimate (entropy per 1000 bytes = 3999, bits = 1"
        );
        expect_eq!(
            1000,
            ec_3999.bytes_needed(3998),
            "bad entropy estimate (entropy per 1000 bytes = 3999, bits = 3998"
        );
        expect_eq!(
            1000,
            ec_3999.bytes_needed(3999),
            "bad entropy estimate (entropy per 1000 bytes = 3999, bits = 3999"
        );
        expect_eq!(
            1001,
            ec_3999.bytes_needed(4000),
            "bad entropy estimate (entropy per 1000 bytes = 3999, bits = 4000"
        );
        expect_eq!(
            262210,
            ec_3999.bytes_needed(1024 * 1024 - 1),
            "bad entropy estimate (entropy per 1000 bytes = 3999, bits = (1024*1024-1)"
        );
        expect_eq!(
            262210,
            ec_3999.bytes_needed(1024 * 1024),
            "bad entropy estimate (entropy per 1000 bytes = 3999, bits = (1024*1024)"
        );

        stack_pin_init!(let ec_4000 = MockCollector::init(4000));
        expect_eq!(
            0,
            ec_4000.bytes_needed(0),
            "bad entropy estimate (entropy per 1000 bytes = 4000, bits = 0"
        );
        expect_eq!(
            1,
            ec_4000.bytes_needed(1),
            "bad entropy estimate (entropy per 1000 bytes = 4000, bits = 1"
        );
        expect_eq!(
            1000,
            ec_4000.bytes_needed(3999),
            "bad entropy estimate (entropy per 1000 bytes = 4000, bits = 3999"
        );
        expect_eq!(
            1000,
            ec_4000.bytes_needed(4000),
            "bad entropy estimate (entropy per 1000 bytes = 4000, bits = 4000"
        );
        expect_eq!(
            1001,
            ec_4000.bytes_needed(4001),
            "bad entropy estimate (entropy per 1000 bytes = 4000, bits = 4001"
        );
        expect_eq!(
            262144,
            ec_4000.bytes_needed(1024 * 1024 - 1),
            "bad entropy estimate (entropy per 1000 bytes = 4000, bits = (1024*1024-1)"
        );
        expect_eq!(
            262144,
            ec_4000.bytes_needed(1024 * 1024),
            "bad entropy estimate (entropy per 1000 bytes = 4000, bits = (1024*1024)"
        );

        stack_pin_init!(let ec_4001 = MockCollector::init(4001));
        expect_eq!(
            0,
            ec_4001.bytes_needed(0),
            "bad entropy estimate (entropy per 1000 bytes = 4001, bits = 0"
        );
        expect_eq!(
            1,
            ec_4001.bytes_needed(1),
            "bad entropy estimate (entropy per 1000 bytes = 4001, bits = 1"
        );
        expect_eq!(
            1000,
            ec_4001.bytes_needed(4000),
            "bad entropy estimate (entropy per 1000 bytes = 4001, bits = 4000"
        );
        expect_eq!(
            1000,
            ec_4001.bytes_needed(4001),
            "bad entropy estimate (entropy per 1000 bytes = 4001, bits = 4001"
        );
        expect_eq!(
            1001,
            ec_4001.bytes_needed(4002),
            "bad entropy estimate (entropy per 1000 bytes = 4001, bits = 4002"
        );
        expect_eq!(
            262079,
            ec_4001.bytes_needed(1024 * 1024 - 1),
            "bad entropy estimate (entropy per 1000 bytes = 4001, bits = (1024*1024-1)"
        );
        expect_eq!(
            262079,
            ec_4001.bytes_needed(1024 * 1024),
            "bad entropy estimate (entropy per 1000 bytes = 4001, bits = (1024*1024)"
        );

        stack_pin_init!(let ec_7999 = MockCollector::init(7999));
        expect_eq!(
            0,
            ec_7999.bytes_needed(0),
            "bad entropy estimate (entropy per 1000 bytes = 7999, bits = 0"
        );
        expect_eq!(
            1,
            ec_7999.bytes_needed(1),
            "bad entropy estimate (entropy per 1000 bytes = 7999, bits = 1"
        );
        expect_eq!(
            1000,
            ec_7999.bytes_needed(7998),
            "bad entropy estimate (entropy per 1000 bytes = 7999, bits = 7998"
        );
        expect_eq!(
            1000,
            ec_7999.bytes_needed(7999),
            "bad entropy estimate (entropy per 1000 bytes = 7999, bits = 7999"
        );
        expect_eq!(
            1001,
            ec_7999.bytes_needed(8000),
            "bad entropy estimate (entropy per 1000 bytes = 7999, bits = 8000"
        );
        expect_eq!(
            131089,
            ec_7999.bytes_needed(1024 * 1024 - 1),
            "bad entropy estimate (entropy per 1000 bytes = 7999, bits = (1024*1024-1)"
        );
        expect_eq!(
            131089,
            ec_7999.bytes_needed(1024 * 1024),
            "bad entropy estimate (entropy per 1000 bytes = 7999, bits = (1024*1024)"
        );

        stack_pin_init!(let ec_8000 = MockCollector::init(8000));
        expect_eq!(
            0,
            ec_8000.bytes_needed(0),
            "bad entropy estimate (entropy per 1000 bytes = 8000, bits = 0"
        );
        expect_eq!(
            1,
            ec_8000.bytes_needed(1),
            "bad entropy estimate (entropy per 1000 bytes = 8000, bits = 1"
        );
        expect_eq!(
            1000,
            ec_8000.bytes_needed(7999),
            "bad entropy estimate (entropy per 1000 bytes = 8000, bits = 7999"
        );
        expect_eq!(
            1000,
            ec_8000.bytes_needed(8000),
            "bad entropy estimate (entropy per 1000 bytes = 8000, bits = 8000"
        );
        expect_eq!(
            1001,
            ec_8000.bytes_needed(8001),
            "bad entropy estimate (entropy per 1000 bytes = 8000, bits = 8001"
        );
        expect_eq!(
            131072,
            ec_8000.bytes_needed(1024 * 1024 - 1),
            "bad entropy estimate (entropy per 1000 bytes = 8000, bits = (1024*1024-1)"
        );
        expect_eq!(
            131072,
            ec_8000.bytes_needed(1024 * 1024),
            "bad entropy estimate (entropy per 1000 bytes = 8000, bits = (1024*1024)"
        );
    }

    // Verifies that the hardware RNG collector is available and functional whenever
    // a hardware RNG is registered. Platform drivers may register after the first
    // call to `HwRngCollector::get_instance` (e.g. during `EarlyBootSeed`), so an early
    // negative result must not prevent later callers from using the collector.
    #[doc = "hw rng collector availability"]
    #[test]
    fn hw_rng_collector_availability_test() {
        let result = HwRngCollector::get_instance();

        if hw_rng_is_registered() {
            assert_true!(result.is_ok(), "HW RNG registered but collector unavailable");
            let collector = result.expect("checked above");

            let mut buf = [0u8; 32];
            expect_eq!(buf.len(), collector.draw_entropy(&mut buf));

            // Repeated calls must keep returning the same instance.
            let again = HwRngCollector::get_instance();
            assert_true!(again.is_ok(), "HW RNG collector became unavailable");
            expect_true!(core::ptr::eq(collector, again.expect("checked above")));
        } else {
            expect_true!(matches!(result, Err(Status::NOT_SUPPORTED)));
        }
    }
}
