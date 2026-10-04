// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// This mod implements a refresh rate counter that calculates the exponential moving average of the
// frame rate. See https://en.wikipedia.org/wiki/Moving_average#Exponential_moving_average.

const ALPHA: f32 = 0.6;

/// Adds `sample` to the exponential moving average `avg`, using `ALPHA` as the weight.
fn add_to_average(avg: &mut f32, sample: f32) {
    // This is arithmetically equivalent to:
    // ALPHA * sample + (1 - ALPHA) * avg
    *avg += ALPHA * (sample - *avg);
}

pub(crate) struct Counter {
    // Most recent frame timestamp
    last_sample_timestamp: zx::MonotonicInstant,

    // Stores the exponential moving average of the time between two frames, using the above
    // `ALPHA` as the weight.
    avg_time_delta_ns: f32,

    // Total number of frames
    num_frames: i64,
}

pub(crate) struct Counts {
    pub sample_rate_hz: f32,
    pub sample_time_delta_ms: f32,
    pub num_frames: i64,
}

impl Counter {
    pub fn new() -> Counter {
        Counter {
            last_sample_timestamp: zx::MonotonicInstant::get(),
            avg_time_delta_ns: 0.0,
            num_frames: 0,
        }
    }

    pub fn add(&mut self, timestamp: zx::MonotonicInstant) {
        let delta = timestamp - self.last_sample_timestamp;
        self.last_sample_timestamp = timestamp;

        add_to_average(&mut self.avg_time_delta_ns, delta.into_nanos() as f32);

        self.num_frames += 1;
    }

    pub fn stats(&self) -> Counts {
        Counts {
            sample_rate_hz: 1000000000f32 / self.avg_time_delta_ns,
            sample_time_delta_ms: self.avg_time_delta_ns / 1000000f32,
            num_frames: self.num_frames,
        }
    }
}

/// Exponential moving averages of the CPU time spent drawing each frame.
///
/// Uses the same weight as [`Counter`], so the averages cover the same frames as the frame rate.
pub(crate) struct RenderTimes {
    // Time spent rendering a frame, excluding cache cleaning.
    avg_render_ns: f32,

    // Time spent cleaning CPU caches so a frame's pixels reach memory.
    avg_clean_ns: f32,
}

impl RenderTimes {
    pub fn new() -> RenderTimes {
        RenderTimes { avg_render_ns: 0.0, avg_clean_ns: 0.0 }
    }

    /// Records one frame that took `render_and_clean` to render, `clean` of which was spent
    /// cleaning caches.
    pub fn add(&mut self, render_and_clean: zx::MonotonicDuration, clean: zx::MonotonicDuration) {
        let render_ns = (render_and_clean.into_nanos() - clean.into_nanos()).max(0);
        add_to_average(&mut self.avg_render_ns, render_ns as f32);
        add_to_average(&mut self.avg_clean_ns, clean.into_nanos() as f32);
    }

    /// Average time spent rendering a frame, excluding cache cleaning, in milliseconds.
    pub fn render_ms(&self) -> f32 {
        self.avg_render_ns / 1000000f32
    }

    /// Average time spent cleaning caches for a frame, in milliseconds.
    pub fn clean_ms(&self) -> f32 {
        self.avg_clean_ns / 1000000f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::{expect_eq, expect_that, gtest, matchers};

    fn millis(ms: i64) -> zx::MonotonicDuration {
        zx::MonotonicDuration::from_millis(ms)
    }

    #[gtest]
    #[fuchsia::test]
    fn render_times_start_at_zero() {
        let times = RenderTimes::new();
        expect_eq!(times.render_ms(), 0.0);
        expect_eq!(times.clean_ms(), 0.0);
    }

    #[gtest]
    #[fuchsia::test]
    fn render_times_exclude_clean_time_from_render_time() {
        let mut times = RenderTimes::new();
        times.add(millis(10), millis(4));
        expect_that!(times.render_ms(), matchers::approx_eq(ALPHA * 6.0));
        expect_that!(times.clean_ms(), matchers::approx_eq(ALPHA * 4.0));
    }

    #[gtest]
    #[fuchsia::test]
    fn render_times_converge_to_steady_state() {
        let mut times = RenderTimes::new();
        for _ in 0..50 {
            times.add(millis(25), millis(5));
        }
        expect_that!(times.render_ms(), matchers::approx_eq(20.0));
        expect_that!(times.clean_ms(), matchers::approx_eq(5.0));
    }

    #[gtest]
    #[fuchsia::test]
    fn render_times_weigh_recent_frames_like_counter() {
        let mut times = RenderTimes::new();
        times.add(millis(10), millis(0));
        times.add(millis(20), millis(0));
        expect_that!(
            times.render_ms(),
            matchers::approx_eq(ALPHA * 20.0 + (1.0 - ALPHA) * ALPHA * 10.0)
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn render_times_clamp_render_time_at_zero() {
        let mut times = RenderTimes::new();
        times.add(millis(1), millis(2));
        expect_eq!(times.render_ms(), 0.0);
        expect_that!(times.clean_ms(), matchers::approx_eq(ALPHA * 2.0));
    }
}
