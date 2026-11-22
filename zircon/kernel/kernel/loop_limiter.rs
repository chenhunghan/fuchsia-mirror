// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use affine::Round;

use crate::platform_rs::timer::{
    DurationMono, DurationMonoTicks, InstantMonoTicks, timer_current_mono_ticks,
    timer_get_ticks_to_time_ratio,
};

/// `LoopLimiter` is used to detect when a thread is looping for "too long".
///
/// Example usage:
///
/// ```ignore
/// // Make sure we spend no more than 30,000 nanoseconds in the loop.
/// let mut limiter = LoopLimiter::<1>::with_duration(DurationMono::from_nanos(30000));
/// while !limiter.exceeded() {
///     // ...
/// }
/// ```
///
/// Because getting the current ticks may be expensive in some virtualized
/// environments, the const parameter `ITERS_PER_GET_TICKS` controls how often
/// `timer_current_mono_ticks` is called.  For example:
///
/// ```ignore
/// // Make sure we spend no more than 30,000 nanoseconds in the loop, but
/// // don't call timer_current_mono_ticks() more than once every 1,000 loop iterations.
/// let mut limiter = LoopLimiter::<1000>::with_duration(DurationMono::from_nanos(30000));
/// while !limiter.exceeded() {
///     // ...
/// }
/// ```
///
/// An `ITERS_PER_GET_TICKS` value of 1 means call `timer_current_mono_ticks` for each
/// invocation of `exceeded`.
#[derive(Debug)]
pub struct LoopLimiter<const ITERS_PER_GET_TICKS: u64> {
    /// Absolute deadline measured in monotonic clock ticks.
    deadline_ticks: InstantMonoTicks,
    /// Number of iterations since the last call to `timer_current_mono_ticks`.
    iter_since: u64,
}

impl<const ITERS_PER_GET_TICKS: u64> LoopLimiter<ITERS_PER_GET_TICKS> {
    /// Construct a limiter with a relative timeout.
    ///
    /// If `duration` is <= 0 the timeout has already been exceeded.
    #[inline]
    pub fn with_duration(duration: DurationMono) -> Self {
        let relative_ticks =
            timer_get_ticks_to_time_ratio().inverse().scale::<{ Round::DOWN }>(duration.0);
        Self::new(DurationMonoTicks(relative_ticks))
    }

    /// Returns true if the timeout has been exceeded.
    ///
    /// Call once per loop iteration.
    #[inline]
    pub fn exceeded(&mut self) -> bool {
        if ITERS_PER_GET_TICKS <= 1 {
            let now = timer_current_mono_ticks();
            return now >= self.deadline_ticks;
        }

        self.iter_since += 1;
        if self.iter_since >= ITERS_PER_GET_TICKS {
            self.iter_since = 0;
            let now = timer_current_mono_ticks();
            return now >= self.deadline_ticks;
        }
        false
    }

    #[inline]
    fn new(relative_ticks: DurationMonoTicks) -> Self {
        const { assert!(ITERS_PER_GET_TICKS > 0) };

        let mut deadline_ticks = InstantMonoTicks::default();
        if relative_ticks.0 > 0 {
            let now = timer_current_mono_ticks();
            deadline_ticks = InstantMonoTicks(now.0.saturating_add(relative_ticks.0));
        }
        Self { deadline_ticks, iter_since: 0 }
    }
}

/// Unit tests for [`LoopLimiter`].
#[cfg(ktest)]
#[unittest::suite(name = "loop_limiter")]
mod tests {
    use unittest::{expect_false, expect_true};

    use super::LoopLimiter;
    use crate::platform_rs::timer::DurationMono;

    /// Tests timeouts that have occurred, timeouts way out in the future, and the happy-cases.
    #[test]
    fn loop_limiter_test() {
        // Timeout has occurred.
        expect_true!(LoopLimiter::<1>::with_duration(DurationMono::INFINITE_PAST).exceeded());
        expect_true!(LoopLimiter::<1>::with_duration(DurationMono::from_nanos(-1)).exceeded());
        expect_true!(LoopLimiter::<1>::with_duration(DurationMono::ZERO).exceeded());

        // Way out in the future.
        expect_false!(LoopLimiter::<1>::with_duration(DurationMono::from_seconds(1000)).exceeded());
        expect_false!(LoopLimiter::<1>::with_duration(DurationMono::INFINITE).exceeded());

        // Happy-cases.
        {
            let mut limiter = LoopLimiter::<1>::with_duration(DurationMono::from_nanos(1));
            while !limiter.exceeded() {
                libarch::r#yield();
            }
        }
        {
            let mut limiter = LoopLimiter::<1>::with_duration(DurationMono::from_nanos(100));
            while !limiter.exceeded() {
                libarch::r#yield();
            }
        }
        {
            let mut limiter = LoopLimiter::<100>::with_duration(DurationMono::from_nanos(1));
            while !limiter.exceeded() {
                libarch::r#yield();
            }
        }
        {
            let mut limiter = LoopLimiter::<100>::with_duration(DurationMono::from_nanos(100));
            while !limiter.exceeded() {
                libarch::r#yield();
            }
        }
    }

    /// Tests when `exceeded` checks the clock, and what it reports, if `ITERS_PER_GET_TICKS > 1`.
    #[test]
    fn iters_per_get_ticks_test() {
        // The timeout has already occurred, so every clock check reports it as exceeded.
        let mut limiter = LoopLimiter::<3>::with_duration(DurationMono::ZERO);
        for _ in 0..2 {
            expect_false!(limiter.exceeded());
            expect_false!(limiter.exceeded());
            expect_true!(limiter.exceeded());
        }

        // A timeout way out in the future is not exceeded, even when the clock is checked.
        let mut limiter = LoopLimiter::<3>::with_duration(DurationMono::INFINITE);
        for _ in 0..6 {
            expect_false!(limiter.exceeded());
        }
    }
}
