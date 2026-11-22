// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(any(all(debug_assertions, target_os = "fuchsia", not(doc)), test))]
use crate::log::warn;
#[cfg(any(all(debug_assertions, target_os = "fuchsia", not(doc)), test))]
use fuchsia_sync::Mutex;
#[cfg(any(all(debug_assertions, target_os = "fuchsia", not(doc)), test))]
use std::panic::Location;
#[cfg(any(debug_assertions, test))]
use std::time::Duration;
#[cfg(any(all(debug_assertions, target_os = "fuchsia", not(doc)), test))]
use std::time::Instant;

#[cfg(any(debug_assertions, test))]
pub(crate) const ASSERT_NOT_TOO_LONG_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(all(debug_assertions, target_os = "fuchsia", not(doc)))]
static BACKTRACE_RATE_LIMITER: BacktraceRateLimiter =
    BacktraceRateLimiter::new(ASSERT_NOT_TOO_LONG_TIMEOUT);

#[cfg(all(debug_assertions, target_os = "fuchsia", not(doc)))]
#[track_caller]
pub(crate) fn request_backtrace() {
    let location = Location::caller();
    BACKTRACE_RATE_LIMITER.request_backtrace_with(
        location,
        Instant::now,
        ::debug::backtrace_request_all_threads,
    );
}

#[cfg(any(all(debug_assertions, target_os = "fuchsia", not(doc)), test))]
#[derive(Debug, PartialEq, Eq)]
enum RateLimitState {
    Idle { cooldown_until: Option<Instant>, suppressed_count: u64 },
    InProgress { suppressed_count: u64 },
}

/// Coalesces concurrent backtrace requests and enforces a post-completion cooldown window.
///
/// `::debug::backtrace_request_all_threads()` triggers a software breakpoint exception handled by
/// `crashsvc`, which suspends all threads in the process while reading thread stacks and VMOs. If
/// multiple `debug_assert_not_too_long!` timers expire while the process is stalled or suspended in
/// `crashsvc`, firing backtrace requests for each expired timer back-to-back would repeatedly
/// re-suspend the process. Measuring the cooldown from when the dump completes ensures the process
/// gets at least `cooldown` of uninterrupted execution time before another dump can be triggered.
#[cfg(any(all(debug_assertions, target_os = "fuchsia", not(doc)), test))]
pub(crate) struct BacktraceRateLimiter {
    cooldown: Duration,
    state: Mutex<RateLimitState>,
}

#[cfg(any(all(debug_assertions, target_os = "fuchsia", not(doc)), test))]
impl BacktraceRateLimiter {
    pub(crate) const fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            state: Mutex::new(RateLimitState::Idle { cooldown_until: None, suppressed_count: 0 }),
        }
    }

    pub(crate) fn request_backtrace_with(
        &self,
        location: &Location<'_>,
        now_fn: impl Fn() -> Instant,
        dump_fn: impl FnOnce(),
    ) -> bool {
        let prior_suppressed = {
            let mut state = self.state.lock();
            let now = now_fn();
            match *state {
                RateLimitState::InProgress { ref mut suppressed_count } => {
                    *suppressed_count += 1;
                    let total_suppressed = *suppressed_count;
                    drop(state);
                    warn!(
                        "debug_assert_not_too_long! timed out at {}:{}; \
                         suppressing backtrace request (dump already in progress, suppressed={})",
                        location.file(),
                        location.line(),
                        total_suppressed,
                    );
                    return false;
                }
                RateLimitState::Idle { cooldown_until: Some(until), ref mut suppressed_count }
                    if now < until =>
                {
                    *suppressed_count += 1;
                    let total_suppressed = *suppressed_count;
                    let remaining = until - now;
                    drop(state);
                    warn!(
                        "debug_assert_not_too_long! timed out at {}:{}; \
                         suppressing backtrace request (cooldown remaining {:?}, suppressed={})",
                        location.file(),
                        location.line(),
                        remaining,
                        total_suppressed,
                    );
                    return false;
                }
                RateLimitState::Idle { suppressed_count, .. } => {
                    *state = RateLimitState::InProgress { suppressed_count: 0 };
                    suppressed_count
                }
            }
        };

        let cooldown = self.cooldown;
        scopeguard::defer! {
            let mut state = self.state.lock();
            let completed_at = now_fn();
            let suppressed_count = match *state {
                RateLimitState::InProgress { suppressed_count } => suppressed_count,
                RateLimitState::Idle { suppressed_count, .. } => suppressed_count,
            };
            *state = RateLimitState::Idle {
                cooldown_until: Some(completed_at + cooldown),
                suppressed_count,
            };
        }

        if prior_suppressed > 0 {
            warn!(
                "debug_assert_not_too_long! timed out at {}:{}; \
                 requesting backtrace (suppressed {} prior requests)",
                location.file(),
                location.line(),
                prior_suppressed,
            );
        } else {
            warn!(
                "debug_assert_not_too_long! timed out at {}:{}; requesting backtrace",
                location.file(),
                location.line(),
            );
        }

        dump_fn();
        true
    }
}

macro_rules! debug_assert_not_too_long {
    ($future:expr $(, $arg:tt)*) => {{
        #[cfg(debug_assertions)]
        {
            use futures::future::FutureExt;
            let future = $future.fuse();
            futures::pin_mut!(future);
            loop {
                futures::select_biased! {
                    result = future => break result,
                    () = fuchsia_async::Timer::new(
                        $crate::debug_assert_not_too_long::ASSERT_NOT_TOO_LONG_TIMEOUT,
                    )
                    .fuse() => {
                        #[cfg(all(target_os = "fuchsia", not(doc)))]
                        $crate::debug_assert_not_too_long::request_backtrace();
                        #[cfg(not(target_os = "fuchsia"))]
                        panic!($($arg),*);
                    }
                }
            }
        }
        #[cfg(not(debug_assertions))]
        $future.await
    }};
}

#[cfg(test)]
mod tests {
    use super::{ASSERT_NOT_TOO_LONG_TIMEOUT, BacktraceRateLimiter, RateLimitState};
    use std::cell::Cell;
    use std::panic::Location;
    use std::time::{Duration, Instant};

    #[fuchsia::test]
    fn test_rate_limiter_coalesces_in_progress_and_enforces_post_completion_cooldown() {
        let limiter = BacktraceRateLimiter::new(ASSERT_NOT_TOO_LONG_TIMEOUT);
        let base = Instant::now();
        let offset_secs = Cell::new(0u64);
        let now_fn = || base + Duration::from_secs(offset_secs.get());
        let dump_count = Cell::new(0u32);
        let location = Location::caller();

        // First timeout at t=60s triggers a backtrace dump.
        offset_secs.set(60);
        let dumped = limiter.request_backtrace_with(location, &now_fn, || {
            dump_count.set(dump_count.get() + 1);

            // Simulate two other threads timing out while crashsvc is still dumping (t=65s, t=200s).
            offset_secs.set(65);
            assert!(!limiter.request_backtrace_with(location, &now_fn, || {
                panic!("in-progress backtrace must suppress concurrent request");
            }));
            offset_secs.set(200);
            assert!(!limiter.request_backtrace_with(location, &now_fn, || {
                panic!("in-progress backtrace must suppress concurrent request");
            }));

            // Simulate crashsvc taking 300s to read pager-backed VMOs before resuming the process
            // at t=360s.
            offset_secs.set(360);
        });
        assert!(dumped);
        assert_eq!(dump_count.get(), 1);

        // Cooldown must be measured from completion (t=360s -> until t=420s), not start (t=60s).
        // Expired timers polled immediately after process resume at t=360s or at t=419s must be
        // suppressed.
        for t in [360, 390, 419] {
            offset_secs.set(t);
            assert!(!limiter.request_backtrace_with(location, &now_fn, || {
                panic!("request within post-completion cooldown at t={t}s must be suppressed");
            }));
        }
        assert_eq!(
            *limiter.state.lock(),
            RateLimitState::Idle {
                cooldown_until: Some(base + Duration::from_secs(420)),
                suppressed_count: 5,
            }
        );

        // Once 60s have elapsed after completion (t=420s), a new backtrace dump is allowed and
        // resets the suppressed counter.
        offset_secs.set(420);
        assert!(limiter.request_backtrace_with(location, &now_fn, || {
            dump_count.set(dump_count.get() + 1);
        }));
        assert_eq!(dump_count.get(), 2);
        assert_eq!(
            *limiter.state.lock(),
            RateLimitState::Idle {
                cooldown_until: Some(base + Duration::from_secs(480)),
                suppressed_count: 0,
            }
        );
    }

    #[cfg(panic = "unwind")]
    #[fuchsia::test]
    fn test_rate_limiter_recovers_after_unwind() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let limiter = BacktraceRateLimiter::new(ASSERT_NOT_TOO_LONG_TIMEOUT);
        let base = Instant::now();
        let offset_secs = Cell::new(0u64);
        let now_fn = || base + Duration::from_secs(offset_secs.get());
        let location = Location::caller();

        let unwind_result = catch_unwind(AssertUnwindSafe(|| {
            limiter.request_backtrace_with(location, &now_fn, || {
                offset_secs.set(10);
                panic!("simulated unwind during dump");
            });
        }));
        assert!(unwind_result.is_err());

        // State must have transitioned back to Idle with cooldown starting at t=10s (until t=70s).
        offset_secs.set(69);
        assert!(!limiter.request_backtrace_with(location, &now_fn, || {
            panic!("should still be in cooldown at t=69s");
        }));

        offset_secs.set(70);
        let dumped = Cell::new(false);
        assert!(limiter.request_backtrace_with(location, &now_fn, || {
            dumped.set(true);
        }));
        assert!(dumped.get());
    }
}
