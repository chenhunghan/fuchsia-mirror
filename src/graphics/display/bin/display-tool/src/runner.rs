// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implements a double-buffer swapchain runner to display a [`Scene`].
//!
//! The swapchain is represented by two images that alternate in their assignment to a primary
//! layer. Writes to each buffer and the resulting swap are synchronized using each configuration's
//! retirement fence, which is aligned to the display's vsync events by the display driver.

use anyhow::{Context, Result, format_err};
use display_utils::{
    Coordinator, DisplayConfig, DisplayId, Image, ImageId, ImageParameters, Layer, LayerConfig,
    LayerId, PixelFormat, VsyncEvent,
};
use fidl_fuchsia_hardware_display_types as fidl_display_types;
use fuchsia_async::TimeoutExt;
use fuchsia_trace::duration;
use futures::channel::mpsc::UnboundedReceiver;
use futures::{FutureExt, StreamExt};
use std::borrow::Borrow;
use std::io::Write;
use std::num::NonZero;
use std::time::Duration;

use crate::draw::{ByteCounts, MappedImage};
use crate::fps::{Counter, RenderTimes};

/// ANSI X3.64 (ECMA-48) escape code for clearing the current terminal line.
const CLEAR: &str = "\x1B[2K\r";

/// Nanoseconds per millisecond for floating-point timestamp formatting.
const NANOS_PER_MILLI: f64 = 1_000_000.0;

/// Default number of frames rendered while the display is off in each power cycle.
const DEFAULT_POWER_CYCLE_OFF_FRAME_COUNT: NonZero<u64> = NonZero::new(60).unwrap();

/// Frame pacing period used while the display is off.
///
/// Display hardware may not generate vsync events while powered off. Waiting for a committed
/// configuration's vsync is bounded by this timer so the client continues submitting frames.
const POWER_OFF_FRAME_PERIOD: Duration = Duration::from_millis(16);

/// Default upper bound on waiting for a committed configuration's vsync while the display is on.
///
/// Surfaces displays that stall or fail to resume generating vsyncs instead of hanging.
const DEFAULT_VSYNC_TIMEOUT: Duration = Duration::from_millis(1000);

/// Converts a monotonic timestamp to milliseconds since boot.
fn instant_to_ms(instant: zx::MonotonicInstant) -> f64 {
    instant.into_nanos() as f64 / NANOS_PER_MILLI
}

/// Converts a monotonic duration to milliseconds.
fn duration_to_ms(duration: zx::MonotonicDuration) -> f64 {
    duration.into_nanos() as f64 / NANOS_PER_MILLI
}

/// Configures periodic display power cycling during an animation loop.
///
/// When enabled, each cycle spans `total_frame_count` frames: the display remains powered on for
/// `total_frame_count - off_frame_count` frames, then powers off for `off_frame_count` frames
/// while the animation continues rendering and committing configurations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerCycle {
    /// Total frames per cycle, or [`None`] when power cycling is disabled.
    total_frame_count: Option<NonZero<u64>>,
    off_frame_count: NonZero<u64>,
}

impl PowerCycle {
    /// Creates a configuration with power cycling disabled.
    pub const fn new() -> Self {
        Self { total_frame_count: None, off_frame_count: DEFAULT_POWER_CYCLE_OFF_FRAME_COUNT }
    }

    /// Creates a validated power-cycling configuration from optional frame counts.
    ///
    /// Passing [`None`] for both arguments disables power cycling. When `total_frame_count` is
    /// [`Some`] and `off_frame_count` is [`None`], `off_frame_count` defaults to
    /// [`DEFAULT_POWER_CYCLE_OFF_FRAME_COUNT`].
    ///
    /// # Outcome
    ///
    /// Returns `Err` if `off_frame_count` is [`Some`] when `total_frame_count` is [`None`], or if
    /// `off_frame_count >= total_frame_count`.
    pub fn try_new(
        total_frame_count: Option<NonZero<u64>>,
        off_frame_count: Option<NonZero<u64>>,
    ) -> Result<Self> {
        let off_frame_count = match (total_frame_count, off_frame_count) {
            (None, None) => return Ok(Self::new()),
            (None, Some(_)) => {
                return Err(format_err!(
                    "--power-cycle-off-frame-count requires --power-cycle-frame-count"
                ));
            }
            (Some(_), None) => DEFAULT_POWER_CYCLE_OFF_FRAME_COUNT,
            (Some(total), Some(off)) if off >= total => {
                return Err(format_err!(
                    "invalid power cycle: need off frames ({}) < total frames ({})",
                    off,
                    total
                ));
            }
            (Some(_), Some(off)) => off,
        };
        Ok(Self { total_frame_count, off_frame_count })
    }

    /// Returns the target power mode if `frame` triggers a power transition from `current_mode`.
    fn next_transition(
        &self,
        frame: u64,
        current_mode: fidl_display_types::PowerMode,
        off_since_frame: Option<u64>,
    ) -> Option<fidl_display_types::PowerMode> {
        let total_frame_count = self.total_frame_count?.get();
        let off_frame_count = self.off_frame_count.get();
        let on_frame_count = total_frame_count - off_frame_count;

        match (current_mode, off_since_frame) {
            (fidl_display_types::PowerMode::On, _)
                if frame % total_frame_count == on_frame_count =>
            {
                Some(fidl_display_types::PowerMode::Off)
            }
            (fidl_display_types::PowerMode::Off, Some(off_frame))
                if frame - off_frame >= off_frame_count =>
            {
                Some(fidl_display_types::PowerMode::On)
            }
            _ => None,
        }
    }
}

impl Default for PowerCycle {
    fn default() -> Self {
        Self::new()
    }
}

/// When a display power mode change happened during [`DoubleBufferedFenceLoop::run`].
#[derive(Clone, Copy, Debug)]
struct PowerTransition {
    /// Number of the frame that triggered the change.
    frame: u64,
    /// Monotonic timestamp at which the `SetDisplayPowerMode` call completed.
    timestamp: zx::MonotonicInstant,
}

/// Tracks the current display power phase during [`DoubleBufferedFenceLoop::run`].
#[derive(Clone, Copy, Debug)]
enum PowerPhase {
    /// The display is powered on.
    On {
        /// The most recent power-on, cleared after the first post-power-on vsync arrives.
        awaiting_first_vsync_since: Option<PowerTransition>,
    },
    /// The display is powered off.
    Off {
        /// The power-off.
        since: PowerTransition,
        /// Number of committed frame configurations that received a vsync while powered off.
        vsyncs_received: u64,
    },
}

/// A scene whose contents may change over time and can be rendered into images mapped to the
/// address space.
pub trait Scene {
    /// Updates the scene contents for the next frame.
    ///
    /// # Outcome
    ///
    /// Advances internal scene state by one frame.
    fn update(&mut self) -> Result<()>;

    /// Initializes `image` prior to the swapchain loop.
    ///
    /// # Outcome
    ///
    /// Invoked once for each swapchain framebuffer image before its first [`Scene::render`] call.
    fn init_image(&self, image: &mut MappedImage) -> Result<()>;

    /// Renders the current scene contents into `image`.
    ///
    /// # Outcome
    ///
    /// Writes pixel data into `image` and flushes CPU caches as needed.
    ///
    /// # Preconditions
    ///
    /// `image` must not be in active use by the display engine during [`Scene::render`].
    fn render(&mut self, image: &mut MappedImage) -> Result<()>;
}

struct Presentation {
    image: MappedImage,
}

impl Presentation {
    pub fn new(image: MappedImage) -> Self {
        Presentation { image }
    }
}

/// Drives a double-buffered swapchain loop for a [`Scene`].
pub struct DoubleBufferedFenceLoop<'a, S: Scene> {
    coordinator: &'a Coordinator,
    display_id: DisplayId,
    layer_id: LayerId,

    params: ImageParameters,

    scene: S,
    presentations: Vec<Presentation>,

    /// Periodic power-cycling configuration.
    power_cycle: PowerCycle,

    /// Maximum time to wait for a committed configuration's vsync while the display is on.
    vsync_timeout: Duration,
}

impl<'a, S: Scene> DoubleBufferedFenceLoop<'a, S> {
    /// Allocates swapchain images and a primary layer for `display_id`.
    ///
    /// # Outcome
    ///
    /// Returns an initialized [`DoubleBufferedFenceLoop`] ready to [`Self::run`].
    pub async fn new(
        coordinator: &'a Coordinator,
        display_id: DisplayId,
        width: u32,
        height: u32,
        pixel_format: PixelFormat,
        scene: S,
    ) -> Result<Self> {
        let params = ImageParameters {
            width,
            height,
            pixel_format,
            color_space: fidl_fuchsia_images2::ColorSpace::Srgb,
            name: Some("image layer".to_string()),
        };
        let mut next_image_id = ImageId(1);

        const NUM_SWAPCHAIN_IMAGES: usize = 2;
        let mut image_presentations = Vec::new();
        for _ in 0..NUM_SWAPCHAIN_IMAGES {
            next_image_id = ImageId(next_image_id.0 + 1);

            let mut image = MappedImage::create(
                Image::create(coordinator.clone(), next_image_id, &params).await?,
            )?;
            scene.init_image(&mut image)?;
            image_presentations.push(Presentation::new(image));
        }

        let layer_id = coordinator.create_layer().await?;

        Ok(DoubleBufferedFenceLoop {
            coordinator,
            display_id,
            layer_id,
            params,

            scene,
            presentations: image_presentations,

            power_cycle: PowerCycle::new(),
            vsync_timeout: DEFAULT_VSYNC_TIMEOUT,
        })
    }

    /// Configures periodic display power cycling in [`Self::run`].
    pub fn set_power_cycle(&mut self, power_cycle: PowerCycle) {
        self.power_cycle = power_cycle;
    }

    /// Sets the timeout for waiting on a committed configuration's vsync while the display is on.
    pub fn set_vsync_timeout(&mut self, vsync_timeout: Duration) {
        self.vsync_timeout = vsync_timeout;
    }

    /// Sets the display power mode and logs the transition.
    ///
    /// # Outcome
    ///
    /// Sends a `SetDisplayPowerMode` FIDL call to the coordinator and returns the transition,
    /// timestamped when the call completes. Returns an error if the FIDL transport fails or the
    /// coordinator rejects the request.
    async fn set_power_mode(
        &self,
        frame: u64,
        power_mode: fidl_display_types::PowerMode,
    ) -> Result<PowerTransition> {
        let start = zx::MonotonicInstant::get();
        let result = self
            .coordinator
            .proxy()
            .set_display_power_mode(&self.display_id.into(), power_mode)
            .await
            .map(|r| r.map_err(zx::Status::err_from_raw));
        let end = zx::MonotonicInstant::get();
        println!(
            "{}[power-cycle] t={:.3}ms frame={} SetDisplayPowerMode({:?}) -> {:?} (call took {:.3}ms)",
            CLEAR,
            instant_to_ms(start),
            frame,
            power_mode,
            result,
            duration_to_ms(end - start)
        );
        result.context("SetDisplayPowerMode FIDL call failed")?.map_err(|status| {
            format_err!("SetDisplayPowerMode({:?}) failed: {}", power_mode, status)
        })?;
        Ok(PowerTransition { frame, timestamp: end })
    }

    /// Checks whether `frame` triggers a power mode transition and updates `phase`.
    ///
    /// # Outcome
    ///
    /// Calls [`Self::set_power_mode`] and transitions `phase` between [`PowerPhase::On`] and
    /// [`PowerPhase::Off`] when a boundary is reached.
    async fn update_power_phase(&self, frame: u64, phase: &mut PowerPhase) -> Result<()> {
        match *phase {
            PowerPhase::On { .. } => {
                if self.power_cycle.next_transition(frame, fidl_display_types::PowerMode::On, None)
                    == Some(fidl_display_types::PowerMode::Off)
                {
                    let power_off =
                        self.set_power_mode(frame, fidl_display_types::PowerMode::Off).await?;
                    *phase = PowerPhase::Off { since: power_off, vsyncs_received: 0 };
                }
            }
            PowerPhase::Off { since, vsyncs_received } => {
                if self.power_cycle.next_transition(
                    frame,
                    fidl_display_types::PowerMode::Off,
                    Some(since.frame),
                ) == Some(fidl_display_types::PowerMode::On)
                {
                    let power_on =
                        self.set_power_mode(frame, fidl_display_types::PowerMode::On).await?;
                    println!(
                        "{}[power-cycle] t={:.3}ms frame={} display was off for {} frames \
                         ({:.3}ms); {} of them got a vsync for their config, the rest were \
                         paced by the {}ms timer",
                        CLEAR,
                        instant_to_ms(power_on.timestamp),
                        frame,
                        frame - since.frame,
                        duration_to_ms(power_on.timestamp - since.timestamp),
                        vsyncs_received,
                        POWER_OFF_FRAME_PERIOD.as_millis()
                    );
                    *phase = PowerPhase::On { awaiting_first_vsync_since: Some(power_on) };
                }
            }
        }
        Ok(())
    }

    /// Waits for `vsync_listener` to report a vsync event matching `applied_stamp`.
    ///
    /// # Outcome
    ///
    /// Returns the monotonic timestamp of the matching vsync event, or an error if the vsync
    /// listener stream closes.
    async fn wait_for_config_vsync(
        vsync_listener: &mut UnboundedReceiver<VsyncEvent>,
        applied_stamp: u64,
    ) -> Result<zx::MonotonicInstant> {
        while let Some(VsyncEvent { id: _, timestamp, config }) = vsync_listener.next().await {
            if config.value == applied_stamp {
                return Ok(timestamp);
            }
        }
        Err(format_err!("stopped receiving vsync events"))
    }

    /// Waits for the committed frame configuration `applied_stamp` to be presented, applying
    /// power-cycle pacing or vsync timeout rules for `power_phase`.
    ///
    /// # Outcome
    ///
    /// Waits for the matching vsync event bounded by [`POWER_OFF_FRAME_PERIOD`] while off or
    /// `self.vsync_timeout` while on, and updates `power_phase` state.
    async fn wait_for_frame_retirement(
        &self,
        vsync_listener: &mut UnboundedReceiver<VsyncEvent>,
        frame: u64,
        applied_stamp: u64,
        power_phase: &mut PowerPhase,
    ) -> Result<()> {
        let wait_fut =
            Self::wait_for_config_vsync(vsync_listener, applied_stamp).map(|res| res.map(Some));

        match power_phase {
            PowerPhase::Off { vsyncs_received, .. } => {
                // The display is off and may not generate vsyncs.
                if wait_fut.on_timeout(POWER_OFF_FRAME_PERIOD, || Ok(None)).await?.is_some() {
                    *vsyncs_received += 1;
                }
            }
            PowerPhase::On { awaiting_first_vsync_since } => {
                match wait_fut.on_timeout(self.vsync_timeout, || Ok(None)).await? {
                    Some(timestamp) => {
                        if let Some(power_on) = awaiting_first_vsync_since.take() {
                            println!(
                                "{}[power-cycle] t={:.3}ms frame={} first vsync after power \
                                 on (power on at frame={}), {:.3}ms after power on",
                                CLEAR,
                                instant_to_ms(timestamp),
                                frame,
                                power_on.frame,
                                duration_to_ms(timestamp - power_on.timestamp)
                            );
                        }
                    }
                    None => {
                        println!(
                            "{}t={:.3}ms frame={} Timed out while waiting for {}ms for config \
                             with stamp {:?}",
                            CLEAR,
                            instant_to_ms(zx::MonotonicInstant::get()),
                            frame,
                            self.vsync_timeout.as_millis(),
                            applied_stamp
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn build_display_configs(&self, presentation_index: usize) -> Vec<DisplayConfig> {
        let presentation = &self.presentations[presentation_index];
        vec![DisplayConfig {
            id: self.display_id,
            layers: vec![Layer {
                id: self.layer_id,
                config: LayerConfig::Primary {
                    image_id: presentation.image.id(),
                    image_metadata: self.params.borrow().into(),
                    unblock_event: None,
                    alpha: None,
                },
            }],
        }]
    }

    /// Runs the double-buffered swapchain loop until interrupted or an error occurs.
    ///
    /// # Outcome
    ///
    /// Continuously renders frames, commits configurations, and awaits vsync retirement.
    pub async fn run(&mut self) -> Result<()> {
        // Apply the first config.
        let mut current_config = 0;
        let _ = self.coordinator.commit_config(&self.build_display_configs(current_config)).await?;

        let mut vsync_listener = self.coordinator.add_vsync_listener(None)?;
        let mut counter = Counter::new();
        let mut render_times = RenderTimes::new();
        // Bytes written and cleaned while rendering the previous frame.
        let mut frame_bytes = ByteCounts::default();
        let mut frame: u64 = 0;
        let mut power_phase = PowerPhase::On { awaiting_first_vsync_since: None };

        loop {
            // Log the frame rate.
            counter.add(zx::MonotonicInstant::get());
            let stats = counter.stats();
            print!(
                "{}Display {:.2} fps ({:.5} ms) render {:.3} ms clean {:.3} ms \
                 written {:.2} MB cleaned {:.2} MB",
                CLEAR,
                stats.sample_rate_hz,
                stats.sample_time_delta_ms,
                render_times.render_ms(),
                render_times.clean_ms(),
                frame_bytes.written as f64 / 1e6,
                frame_bytes.cleaned as f64 / 1e6
            );
            std::io::stdout().flush()?;

            frame += 1;
            self.update_power_phase(frame, &mut power_phase).await?;

            // Prepare the next image.
            // `current_config` alternates between 0 and 1.
            current_config ^= 1;
            let current_presentation = &mut self.presentations[current_config];

            let applied_stamp; // Config stamp of the about-to-be-applied config.
            {
                duration!(c"gfx", c"frame", "id" => stats.num_frames);
                {
                    duration!(c"gfx", c"update scene");
                    self.scene.update()?;
                }

                // Render the scene into the current presentation.
                {
                    duration!(c"gfx", c"render frame", "image" => current_config as u32);
                    let render_start = zx::MonotonicInstant::get();
                    self.scene.render(&mut current_presentation.image)?;
                    render_times.add(
                        zx::MonotonicInstant::get() - render_start,
                        current_presentation.image.take_clean_time(),
                    );
                    frame_bytes = current_presentation.image.take_byte_counts();
                }

                // Request the swap.
                {
                    duration!(c"gfx", c"apply config");
                    applied_stamp = self
                        .coordinator
                        .commit_config(&self.build_display_configs(current_config))
                        .await?;
                }
            }

            // Wait for the previous frame image to retire before drawing on it.
            self.wait_for_frame_retirement(
                &mut vsync_listener,
                frame,
                applied_stamp,
                &mut power_phase,
            )
            .await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::{expect_eq, expect_that, gtest, matchers};

    #[gtest]
    #[fuchsia::test]
    fn power_cycle_disabled_when_both_args_none() {
        let power_cycle = PowerCycle::try_new(None, None).unwrap();
        expect_eq!(power_cycle, PowerCycle::new());
        expect_eq!(power_cycle.next_transition(60, fidl_display_types::PowerMode::On, None), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn power_cycle_rejects_off_frames_without_total_frames() {
        expect_that!(
            PowerCycle::try_new(None, Some(NonZero::new(30).unwrap())),
            matchers::err(matchers::anything())
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn power_cycle_rejects_off_frames_greater_or_equal_to_period() {
        let period = NonZero::new(60);
        expect_that!(
            PowerCycle::try_new(period, NonZero::new(60)),
            matchers::err(matchers::anything())
        );
        expect_that!(
            PowerCycle::try_new(period, NonZero::new(61)),
            matchers::err(matchers::anything())
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn power_cycle_transitions_have_uniform_on_and_off_durations() {
        let power_cycle = PowerCycle::try_new(NonZero::new(300), None).unwrap();

        let mut mode = fidl_display_types::PowerMode::On;
        let mut off_since: Option<u64> = None;
        let mut transitions = Vec::new();

        for frame in 1..=900 {
            if let Some(next_mode) = power_cycle.next_transition(frame, mode, off_since) {
                transitions.push((frame, next_mode));
                mode = next_mode;
                off_since = (mode == fidl_display_types::PowerMode::Off).then_some(frame);
            }
        }

        expect_eq!(
            transitions,
            vec![
                (240, fidl_display_types::PowerMode::Off),
                (300, fidl_display_types::PowerMode::On),
                (540, fidl_display_types::PowerMode::Off),
                (600, fidl_display_types::PowerMode::On),
                (840, fidl_display_types::PowerMode::Off),
                (900, fidl_display_types::PowerMode::On),
            ]
        );
    }
}
