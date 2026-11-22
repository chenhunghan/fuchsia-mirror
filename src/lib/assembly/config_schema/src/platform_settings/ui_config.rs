// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assembly_container::WalkPaths;
use camino::Utf8PathBuf;
use input_device_constants::InputDeviceType;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;

fn default_frame_prediction_margin_in_us() -> u64 {
    3000
}

fn is_default_frame_prediction_margin(val: &u64) -> bool {
    *val == default_frame_prediction_margin_in_us()
}

fn default_image_import_workers() -> u8 {
    1
}

fn is_default_image_import_workers(val: &u8) -> bool {
    *val == default_image_import_workers()
}

/// Platform configuration options for the UI area.
#[derive(Debug, Deserialize, Serialize, PartialEq, JsonSchema, WalkPaths)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformUiConfig {
    /// Whether UI should be enabled on the product.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub enabled: bool,

    /// Whether to prefetch binary VMOs.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub prefetch: bool,

    /// The sensor config to provide to the input pipeline.
    #[schemars(schema_with = "crate::option_path_schema")]
    #[walk_paths]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensor_config: Option<Utf8PathBuf>,

    /// The minimum frame duration for frame scheduler.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub frame_scheduler_min_predicted_frame_duration_in_us: u64,

    /// Safety margin added to prediction time to reduce impact of noise and misprediction.
    /// Unfortunately this means minimum possible latency is increased by the same amount.
    #[serde(skip_serializing_if = "is_default_frame_prediction_margin")]
    pub frame_prediction_margin_in_us: u64,

    /// Scenic shifts focus from view to view as the user interacts with the UI.
    /// Set to false for Smart displays, as they use a different programmatic focus change scheme.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub pointer_auto_focus: bool,

    /// Only use a dedicated input loop/dispatcher/thread if configured to do so.
    /// Otherwise, use the same dispatcher for rendering and input.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub use_separate_input_thread: bool,

    /// Scenic attempts to delegate composition of client images to the display controller, with
    /// GPU/Vulkan composition as the fallback. If false, GPU/Vulkan composition is always used.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub display_composition: bool,

    /// Enables direct-to-display check config heuristics in Scenic.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub display_composition_enable_heuristics: bool,

    /// Configuration for Scenic client image import worker pool and scheduling policy.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub image_import: ImageImportConfig,

    /// Scenic *always* uses the Flatland2 uberstruct schema.
    /// This field is ignored. Delete ASAP.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub use_flatland2_uberstruct_schema: bool,

    /// The relevant input device bindings from which to install appropriate
    /// input handlers. Default to an empty set.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub supported_input_devices: Vec<InputDeviceType>,

    /// The rotation of the display, counter-clockwise, in 90-degree increments.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub display_rotation: u64,

    // TODO(https://fxbug.dev/324217535): change to float when supported in structured config.
    /// The density of the display, in pixels per mm.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub display_pixel_density: String,

    /// The expected viewing distance for the display.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub viewing_distance: ViewingDistance,

    /// Whether to include brightness manager, and the relevant configs.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub brightness_manager: Option<BrightnessManager>,

    /// Set with_synthetic_device_support true to include input-helper to ui.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub with_synthetic_device_support: bool,

    /// The renderer Scenic should use.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub renderer: RendererType,

    /// The constraints on the display mode
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub display_mode: DisplayModeConfig,

    /// Enables a frame counter overlay in Scenic.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub scenic_frame_counter_overlay: bool,

    /// Set visual_debugging_level to enable visual debugging features.
    /// See "Config for Fuchsia Visual Debugging": go/config-fuchsia-visual-debugging
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub visual_debugging_level: VisualDebuggingLevel,

    /// Attaches a11y view in SceneManager
    pub attach_a11y_view: bool,

    /// Enables merging move only touch events in the input pipeline.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub enable_merge_touch_events: bool,

    /// Enables cohosting a subset of ui components in dso_runner, currently scene_manager and
    /// scenic
    /// (https://fxbug.dev/403545512)
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub enable_dso: bool,
}

impl Default for PlatformUiConfig {
    fn default() -> Self {
        Self {
            enabled: Default::default(),
            prefetch: false,
            sensor_config: Default::default(),
            frame_scheduler_min_predicted_frame_duration_in_us: Default::default(),
            frame_prediction_margin_in_us: default_frame_prediction_margin_in_us(),
            pointer_auto_focus: true,
            use_separate_input_thread: true,
            display_composition: false,
            display_composition_enable_heuristics: false,
            image_import: Default::default(),
            use_flatland2_uberstruct_schema: false,
            supported_input_devices: Default::default(),
            display_rotation: Default::default(),
            display_pixel_density: Default::default(),
            viewing_distance: Default::default(),
            brightness_manager: Default::default(),
            with_synthetic_device_support: Default::default(),
            renderer: Default::default(),
            display_mode: Default::default(),
            scenic_frame_counter_overlay: Default::default(),
            visual_debugging_level: Default::default(),
            attach_a11y_view: true,
            enable_merge_touch_events: false,
            enable_dso: false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum ViewingDistance {
    Handheld,
    Close,
    Near,
    Midrange,
    Far,
    #[default]
    Unknown,
}

impl AsRef<str> for ViewingDistance {
    fn as_ref(&self) -> &str {
        match &self {
            Self::Handheld => "handheld",
            Self::Close => "close",
            Self::Near => "near",
            Self::Midrange => "midrange",
            Self::Far => "far",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayModeConfig {
    /// The constraints on the display mode horizontal resolution, in pixels.
    pub horizontal_resolution_px_range: UnsignedIntegerRangeInclusive,

    /// The constraints on the display mode vertical resolution, in pixels.
    pub vertical_resolution_px_range: UnsignedIntegerRangeInclusive,

    /// The constraints on the display mode refresh rate, in millihertz (10^-3 Hz).
    pub refresh_rate_millihertz_range: UnsignedIntegerRangeInclusive,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub struct BrightnessManager {
    pub with_display_power: bool,
}

// LINT.IfChange
/// Options for Scenic renderers that may be supported.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum RendererType {
    Null,
    #[default]
    Vulkan,
}
// LINT.ThenChange(/src/ui/scenic/bin/app.h)

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
pub struct UnsignedIntegerRangeInclusive {
    /// The inclusive lower bound of the range. If None, the range is unbounded.
    pub start: Option<u32>,

    /// The inclusive upper bound of the range. If None, the range is unbounded.
    pub end: Option<u32>,
}

/// VisualDebuggingLevel used to enable visualized debug features.
/// It has 3 level for now:
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum VisualDebuggingLevel {
    /// None (0): disable all visual debugging features.
    #[default]
    None,
    /// InfoProduct (1): enable colorful blackscreen.
    InfoProduct,
    /// InfoPlatform (2): enable platform related debug: scenic tint.
    InfoPlatform,
}

impl std::convert::From<VisualDebuggingLevel> for u8 {
    fn from(val: VisualDebuggingLevel) -> Self {
        match val {
            VisualDebuggingLevel::None => 0,
            VisualDebuggingLevel::InfoProduct => 1,
            VisualDebuggingLevel::InfoPlatform => 2,
        }
    }
}

// LINT.IfChange
/// Configuration for Scenic client image import worker pool and scheduling policy.
///
/// Every policy field other than `workers` is optional. An unset (`None`) field is omitted from
/// `fuchsia.scenic.ImageImportPolicy`, so Scenic's built-in default applies (and may change
/// between releases). Setting a field pins that policy value for the product, even if Scenic's
/// default later changes.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ImageImportConfig {
    /// Number of background GPU worker threads for Scenic client image import (default 1; 0 runs
    /// inline on the main thread). Each worker spawns one dedicated thread under the image-import
    /// scheduler role; values above the hardware core count provide no latency benefit.
    #[serde(skip_serializing_if = "is_default_image_import_workers")]
    pub workers: u8,

    /// Scheduling policy across display and GPU import kinds. If unset, Scenic chooses the
    /// default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kinds: Option<ImageImportKinds>,

    /// Priority order among off-screen client images. If unset, Scenic chooses the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub off_screen: Option<ImageImportOffScreenOrder>,

    /// Background import policy for client images that have not yet been presented. If unset,
    /// Scenic chooses the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub never_shown: Option<ImageImportNeverShown>,

    /// Idle delay in milliseconds after the last frame before `when_idle` imports may start. If
    /// unset, Scenic chooses the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_ms: Option<u32>,

    /// Maximum in-flight idle-window display imports. If unset, Scenic chooses the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_cap: Option<NonZeroU32>,

    /// Whether to run display-coordinator replies on a dedicated thread instead of pumping the
    /// reply loop on Scenic's main thread. If unset, Scenic chooses the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_thread: Option<bool>,
}

impl Default for ImageImportConfig {
    fn default() -> Self {
        Self {
            workers: default_image_import_workers(),
            kinds: None,
            off_screen: None,
            never_shown: None,
            idle_ms: None,
            display_cap: None,
            reply_thread: None,
        }
    }
}

/// Scheduling policy across display and GPU import kinds.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageImportKinds {
    /// Favor the import kind (display or GPU) predicted for the next frame, and avoid running
    /// display and GPU imports concurrently.
    PredictedKindFirst,
    /// Allow display and GPU imports to run concurrently.
    Concurrent,
}

/// Priority order among off-screen client images.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageImportOffScreenOrder {
    /// Prioritize off-screen images that were shown most recently.
    MostRecentFirst,
    /// Prioritize off-screen images that were shown least recently.
    LeastRecentFirst,
}

/// Background import policy for client images that have not yet been presented.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageImportNeverShown {
    /// Import never-presented images in the background once Scenic has been idle for `idle_ms`.
    WhenIdle,
    /// Import never-presented images whenever import capacity is free, without waiting for the
    /// idle delay.
    WhenFree,
    /// Do not run background imports for images until a `Present` names them.
    Never,
}
// LINT.ThenChange(/src/lib/assembly/platform_configuration/src/subsystems/ui.rs, /src/ui/scenic/lib/flatland/image_import/image_importer_impl.cc, /src/ui/scenic/bin/meta/config.shard.cml:image_import_policy)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_serialization() {
        crate::common::tests::default_serialization_helper::<PlatformUiConfig>();
        // `PlatformUiConfig` skips `image_import` when it is at its default, so round-trip
        // `ImageImportConfig` directly as well.
        crate::common::tests::default_serialization_helper::<ImageImportConfig>();
    }

    #[test]
    fn test_zero_image_import_workers_serialization() {
        // `workers: 0` (inline mode, no worker threads) is `u8::default()`, but
        // `ImageImportConfig::default().workers` is 1. If `workers` used
        // `crate::common::is_default` instead of `is_default_image_import_workers` in
        // `skip_serializing_if`, then 0 would be omitted when serializing and would silently
        // become 1 when deserialized.
        crate::common::tests::value_serialization_helper(PlatformUiConfig {
            image_import: ImageImportConfig { workers: 0, ..Default::default() },
            ..Default::default()
        });
        assert_eq!(
            serde_json::to_value(ImageImportConfig { workers: 0, ..Default::default() }).unwrap(),
            serde_json::json!({ "workers": 0 }),
        );
    }

    #[test]
    fn test_non_default_image_import_serialization() {
        // `reply_thread: Some(false)` also checks that an explicit `false` is kept, not collapsed
        // to `None`.
        let image_import = ImageImportConfig {
            workers: 2,
            kinds: Some(ImageImportKinds::Concurrent),
            off_screen: Some(ImageImportOffScreenOrder::LeastRecentFirst),
            never_shown: Some(ImageImportNeverShown::WhenFree),
            idle_ms: Some(250),
            display_cap: Some(NonZeroU32::new(3).unwrap()),
            reply_thread: Some(false),
        };
        assert_eq!(
            serde_json::to_value(&image_import).unwrap(),
            serde_json::json!({
                "workers": 2,
                "kinds": "concurrent",
                "off_screen": "least_recent_first",
                "never_shown": "when_free",
                "idle_ms": 250,
                "display_cap": 3,
                "reply_thread": false,
            }),
        );
        crate::common::tests::value_serialization_helper(PlatformUiConfig {
            image_import,
            ..Default::default()
        });
    }

    #[test]
    fn test_invalid_image_import_config_rejected() {
        assert!(
            serde_json::from_value::<PlatformUiConfig>(
                serde_json::json!({ "image_import": { "display_cap": 0 } })
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<PlatformUiConfig>(
                serde_json::json!({ "image_import": { "kinds": "bogus" } })
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<PlatformUiConfig>(
                serde_json::json!({ "image_import": { "unknown_key": 1 } })
            )
            .is_err()
        );
    }
}
