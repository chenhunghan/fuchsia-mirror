// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::subsystems::prelude::*;
use anyhow::ensure;
use assembly_config_capabilities::{Config, ConfigNestedValueType, ConfigValueType};
use assembly_config_schema::platform_settings::ui_config::{
    ImageImportConfig, ImageImportKinds, ImageImportNeverShown, ImageImportOffScreenOrder,
    PlatformUiConfig, UnsignedIntegerRangeInclusive,
};
use assembly_constants::{BoardFeature, FileEntry, PackageDestination, PackageSetDestination};

// Maximum CML string size for `fuchsia.scenic.ImageImportPolicy`; the grammar bounds valid strings
// to 139 bytes (< 256).
// LINT.IfChange(max_len)
const IMAGE_IMPORT_POLICY_MAX_LEN: u32 = 256;
// LINT.ThenChange(/src/ui/scenic/bin/meta/config.shard.cml:image_import_policy)

// LINT.IfChange
// Formats the set fields of `ImageImportConfig` into the comma-separated `key=value`
// `fuchsia.scenic.ImageImportPolicy` string consumed by Scenic's `ParseImageImportPolicy()`.
// Unset (`None`) fields are omitted so Scenic's `ImageImporterImpl::Config` defaults apply.
fn format_image_import_policy(config: &ImageImportConfig) -> String {
    let mut parts = Vec::with_capacity(6);
    if let Some(kinds) = config.kinds {
        let v = match kinds {
            ImageImportKinds::PredictedKindFirst => "predicted_kind_first",
            ImageImportKinds::Concurrent => "concurrent",
        };
        parts.push(format!("kinds={v}"));
    }
    if let Some(off_screen) = config.off_screen {
        let v = match off_screen {
            ImageImportOffScreenOrder::MostRecentFirst => "most_recent_first",
            ImageImportOffScreenOrder::LeastRecentFirst => "least_recent_first",
        };
        parts.push(format!("off_screen={v}"));
    }
    if let Some(never_shown) = config.never_shown {
        let v = match never_shown {
            ImageImportNeverShown::WhenIdle => "when_idle",
            ImageImportNeverShown::WhenFree => "when_free",
            ImageImportNeverShown::Never => "never",
        };
        parts.push(format!("never_shown={v}"));
    }
    if let Some(idle_ms) = config.idle_ms {
        parts.push(format!("idle_ms={idle_ms}"));
    }
    if let Some(display_cap) = config.display_cap {
        parts.push(format!("display_cap={display_cap}"));
    }
    if let Some(reply_thread) = config.reply_thread {
        parts.push(format!("reply_thread={reply_thread}"));
    }
    parts.join(",")
}
// LINT.ThenChange(/src/ui/scenic/lib/flatland/image_import/image_importer_impl.cc, /src/lib/assembly/config_schema/src/platform_settings/ui_config.rs, /src/ui/scenic/bin/meta/config.shard.cml:image_import_policy, /src/ui/scenic/lib/flatland/image_import/tests/image_importer_impl_unittest.cc:policy_test_cases)

pub(crate) struct UiSubsystem;

impl DefineSubsystemConfiguration<PlatformUiConfig> for UiSubsystem {
    fn define_configuration(
        context: &ConfigurationContext<'_>,
        ui_config: &PlatformUiConfig,
        builder: &mut dyn ConfigurationBuilder,
    ) -> anyhow::Result<()> {
        if context.board_config.provides_feature(BoardFeature::Input) {
            builder.platform_bundle("input_drivers")?;
        }

        let visual_debugging_level: u8 = ui_config.visual_debugging_level.clone().into();
        builder.set_config_capability(
            "fuchsia.ui.VisualDebuggingLevel",
            Config::new(ConfigValueType::Uint8, visual_debugging_level.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.ui.Prefetch",
            Config::new(ConfigValueType::Bool, ui_config.prefetch.into()),
        )?;

        if !ui_config.enabled {
            return Ok(());
        }

        if ui_config.enable_dso {
            // Supports ui_*_dso bundles below
            builder.platform_bundle("dso_runner")?;
        }

        match context.build_type {
            BuildType::Eng => {
                builder.platform_bundle("ui")?;
                if ui_config.enable_dso {
                    builder.icu_platform_bundle("ui_eng_dso")?;
                } else {
                    builder.icu_platform_bundle("ui_eng")?;
                }
                match &ui_config.with_synthetic_device_support {
                    true => {
                        builder.platform_bundle(
                            "ui_package_eng_userdebug_with_synthetic_device_support",
                        )?;
                    }
                    false => {
                        builder.platform_bundle("ui_package_eng")?;
                    }
                }
            }
            BuildType::UserDebug => {
                builder.platform_bundle("ui")?;
                if ui_config.enable_dso {
                    builder.icu_platform_bundle("ui_userdebug_dso")?;
                } else {
                    builder.icu_platform_bundle("ui_user_and_userdebug")?;
                }
                match &ui_config.with_synthetic_device_support {
                    true => {
                        builder.platform_bundle(
                            "ui_package_eng_userdebug_with_synthetic_device_support",
                        )?;
                    }
                    false => {
                        builder.platform_bundle("ui_package_user_and_userdebug")?;
                    }
                }
            }
            BuildType::User => {
                ensure!(
                    !ui_config.enable_dso,
                    "experimental dso must not be enabled on user builds"
                );
                builder.platform_bundle("ui")?;
                builder.icu_platform_bundle("ui_user_and_userdebug")?;
                builder.platform_bundle("ui_package_user_and_userdebug")?;
            }
        }

        ensure!(
            *context.feature_set_level == FeatureSetLevel::Standard,
            "UI is only supported in the default feature set level"
        );

        fn get_px_range(new: &UnsignedIntegerRangeInclusive) -> (i32, i32) {
            let start = new.start.map(|i| i as i32).unwrap_or(-1);
            let end = new.end.map(|i| i as i32).unwrap_or(-1);
            (start, end)
        }

        let (horizontal_res_min, horizontal_res_max) =
            get_px_range(&ui_config.display_mode.horizontal_resolution_px_range);
        let (vertical_res_min, vertical_res_max) =
            get_px_range(&ui_config.display_mode.vertical_resolution_px_range);
        let (refresh_rate_min, refresh_rate_max) =
            get_px_range(&ui_config.display_mode.refresh_rate_millihertz_range);

        // We should only configure scenic here when it has been added to assembly.
        builder.set_config_capability(
            "fuchsia.scenic.Renderer",
            Config::new(
                ConfigValueType::String { max_size: 16 },
                serde_json::to_value(ui_config.renderer.clone())?,
            ),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.FrameSchedulerMinPredictedFrameDurationInUs",
            Config::new(
                ConfigValueType::Uint64,
                ui_config.frame_scheduler_min_predicted_frame_duration_in_us.into(),
            ),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.FramePredictionMarginInUs",
            Config::new(ConfigValueType::Uint64, ui_config.frame_prediction_margin_in_us.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.PointerAutoFocus",
            Config::new(ConfigValueType::Bool, ui_config.pointer_auto_focus.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.UseSeparateInputThread",
            Config::new(ConfigValueType::Bool, ui_config.use_separate_input_thread.into()),
        )?;
        if ui_config.display_composition_enable_heuristics {
            ensure!(
                ui_config.display_composition,
                "display_composition_enable_heuristics can only be enabled if display_composition is enabled"
            );
        }
        builder.set_config_capability(
            "fuchsia.scenic.DisplayComposition",
            Config::new(ConfigValueType::Bool, ui_config.display_composition.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.DisplayCompositionEnableHeuristics",
            Config::new(
                ConfigValueType::Bool,
                ui_config.display_composition_enable_heuristics.into(),
            ),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.ImageImportWorkers",
            Config::new(ConfigValueType::Uint8, ui_config.image_import.workers.into()),
        )?;
        let image_import_policy = format_image_import_policy(&ui_config.image_import);
        ensure!(
            image_import_policy.len() <= IMAGE_IMPORT_POLICY_MAX_LEN as usize,
            "Formatted image_import_policy ({} bytes) exceeds max_size ({IMAGE_IMPORT_POLICY_MAX_LEN})",
            image_import_policy.len(),
        );
        builder.set_config_capability(
            "fuchsia.scenic.ImageImportPolicy",
            Config::new(
                ConfigValueType::String { max_size: IMAGE_IMPORT_POLICY_MAX_LEN },
                image_import_policy.into(),
            ),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.ICanHazDisplayId",
            Config::new(ConfigValueType::Int64, (-1i64).into()),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.ICanHazDisplayMode",
            Config::new(ConfigValueType::Int64, (-1i64).into()),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.DisplayRotation",
            Config::new(ConfigValueType::Uint64, ui_config.display_rotation.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.MinDisplayHorizontalResolutionPx",
            Config::new(ConfigValueType::Int32, horizontal_res_min.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.MaxDisplayHorizontalResolutionPx",
            Config::new(ConfigValueType::Int32, horizontal_res_max.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.MinDisplayVerticalResolutionPx",
            Config::new(ConfigValueType::Int32, vertical_res_min.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.MaxDisplayVerticalResolutionPx",
            Config::new(ConfigValueType::Int32, vertical_res_max.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.MinDisplayRefreshRateMillihertz",
            Config::new(ConfigValueType::Int32, refresh_rate_min.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.MaxDisplayRefreshRateMillihertz",
            Config::new(ConfigValueType::Int32, refresh_rate_max.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.scenic.FrameCounterOverlay",
            Config::new(ConfigValueType::Bool, ui_config.scenic_frame_counter_overlay.into()),
        )?;

        builder.set_config_capability(
            "fuchsia.ui.SupportedInputDevices",
            Config::new(
                ConfigValueType::Vector {
                    nested_type: ConfigNestedValueType::String { max_size: 12 },
                    max_count: 6,
                },
                ui_config
                    .supported_input_devices
                    .iter()
                    .filter_map(|d| serde_json::to_value(d).ok())
                    .collect::<serde_json::Value>(),
            ),
        )?;

        builder.set_config_capability(
            "fuchsia.ui.DisplayPixelDensity",
            Config::new(
                ConfigValueType::String { max_size: 8 },
                ui_config.display_pixel_density.clone().into(),
            ),
        )?;

        builder.set_config_capability(
            "fuchsia.ui.ViewingDistance",
            Config::new(
                ConfigValueType::String { max_size: 8 },
                ui_config.viewing_distance.as_ref().to_string().into(),
            ),
        )?;

        builder.set_config_capability(
            "fuchsia.ui.AttachA11yView",
            Config::new(ConfigValueType::Bool, ui_config.attach_a11y_view.into()),
        )?;

        builder.set_config_capability(
            "fuchsia.ui.IdleThresholdMs",
            Config::new(ConfigValueType::Uint64, {
                match context.build_type {
                    BuildType::Eng => 5000,
                    BuildType::UserDebug | BuildType::User => 5000,
                }
                .into()
            }),
        )?;

        let config_dir = builder
            .add_domain_config(PackageSetDestination::Blob(PackageDestination::SensorConfig))
            .directory("sensor-config");
        if let Some(sensor_config_path) = &ui_config.sensor_config {
            config_dir.entry(FileEntry {
                source: sensor_config_path.clone(),
                destination: "config.json".into(),
            })?;
        }

        let mut manage_display_power = false;
        let mut power_on_display_millis = 0u16;
        let mut power_off_display_millis = 0u16;
        if let Some(brightness_manager) = &ui_config.brightness_manager {
            builder.platform_bundle("brightness_manager")?;
            if brightness_manager.with_display_power {
                manage_display_power = true;
                power_on_display_millis = 35;
                power_off_display_millis = 85;
            }
        }
        builder.set_config_capability(
            "fuchsia.ui.ManageDisplayPower",
            Config::new(ConfigValueType::Bool, manage_display_power.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.ui.PowerOnDelayMillis",
            Config::new(ConfigValueType::Uint16, power_on_display_millis.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.ui.PowerOffDelayMillis",
            Config::new(ConfigValueType::Uint16, power_off_display_millis.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.ui.EnableMergeTouchEvents",
            Config::new(ConfigValueType::Bool, ui_config.enable_merge_touch_events.into()),
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::ConfigurationBuilderImpl;
    use std::num::NonZeroU32;

    fn run_ui_subsystem(
        image_import: ImageImportConfig,
    ) -> anyhow::Result<crate::common::CompletedConfiguration> {
        let context = ConfigurationContext {
            feature_set_level: &FeatureSetLevel::Standard,
            build_type: &BuildType::Eng,
            ..ConfigurationContext::default_for_tests()
        };
        let ui_config = PlatformUiConfig { enabled: true, image_import, ..Default::default() };
        let mut builder = ConfigurationBuilderImpl::default();
        UiSubsystem::define_configuration(&context, &ui_config, &mut builder)?;
        Ok(builder.build())
    }

    // Each policy string here must also be a `policy` in Scenic's `kPolicyTestCases`
    // (`image_importer_impl_unittest.cc`), which checks that Scenic parses it to the intended values.
    // LINT.IfChange(policy_test_cases)
    fn image_import_policy_cases() -> Vec<(ImageImportConfig, &'static str)> {
        vec![
            (ImageImportConfig::default(), ""),
            (
                ImageImportConfig {
                    kinds: Some(ImageImportKinds::Concurrent),
                    off_screen: Some(ImageImportOffScreenOrder::LeastRecentFirst),
                    never_shown: Some(ImageImportNeverShown::WhenFree),
                    idle_ms: Some(250),
                    display_cap: NonZeroU32::new(3),
                    reply_thread: Some(true),
                    ..Default::default()
                },
                "kinds=concurrent,off_screen=least_recent_first,never_shown=when_free,idle_ms=250,\
                 display_cap=3,reply_thread=true",
            ),
            (
                ImageImportConfig {
                    kinds: Some(ImageImportKinds::PredictedKindFirst),
                    off_screen: Some(ImageImportOffScreenOrder::MostRecentFirst),
                    never_shown: Some(ImageImportNeverShown::Never),
                    reply_thread: Some(false),
                    ..Default::default()
                },
                "kinds=predicted_kind_first,off_screen=most_recent_first,never_shown=never,\
                 reply_thread=false",
            ),
            (
                ImageImportConfig {
                    never_shown: Some(ImageImportNeverShown::WhenIdle),
                    ..Default::default()
                },
                "never_shown=when_idle",
            ),
            (ImageImportConfig { idle_ms: Some(0), ..Default::default() }, "idle_ms=0"),
            (
                ImageImportConfig {
                    idle_ms: Some(u32::MAX),
                    display_cap: NonZeroU32::new(1),
                    ..Default::default()
                },
                "idle_ms=4294967295,display_cap=1",
            ),
            (
                ImageImportConfig {
                    kinds: Some(ImageImportKinds::PredictedKindFirst),
                    off_screen: Some(ImageImportOffScreenOrder::LeastRecentFirst),
                    never_shown: Some(ImageImportNeverShown::WhenIdle),
                    idle_ms: Some(u32::MAX),
                    display_cap: NonZeroU32::new(u32::MAX),
                    reply_thread: Some(false),
                    ..Default::default()
                },
                "kinds=predicted_kind_first,off_screen=least_recent_first,never_shown=when_idle,\
                 idle_ms=4294967295,display_cap=4294967295,reply_thread=false",
            ),
        ]
    }
    // LINT.ThenChange(/src/ui/scenic/lib/flatland/image_import/tests/image_importer_impl_unittest.cc:policy_test_cases)

    #[test]
    fn image_import_policy_cases_format_and_assemble() {
        for (input, expected_policy) in image_import_policy_cases() {
            let config = run_ui_subsystem(ImageImportConfig { workers: 3, ..input })
                .unwrap_or_else(|e| panic!("expected '{expected_policy}' to succeed, got '{e}'"));
            assert_eq!(
                config.configuration_capabilities["fuchsia.scenic.ImageImportWorkers"].value(),
                serde_json::json!(3)
            );
            assert_eq!(
                config.configuration_capabilities["fuchsia.scenic.ImageImportPolicy"].value(),
                serde_json::json!(expected_policy)
            );
        }
    }

    #[test]
    fn image_import_workers_default_and_zero_assemble() {
        let default_config = run_ui_subsystem(ImageImportConfig::default()).unwrap();
        assert_eq!(
            default_config.configuration_capabilities["fuchsia.scenic.ImageImportWorkers"].value(),
            serde_json::json!(1)
        );
        assert_eq!(
            default_config.configuration_capabilities["fuchsia.scenic.ImageImportPolicy"].value(),
            serde_json::json!("")
        );

        let zero_config =
            run_ui_subsystem(ImageImportConfig { workers: 0, ..Default::default() }).unwrap();
        assert_eq!(
            zero_config.configuration_capabilities["fuchsia.scenic.ImageImportWorkers"].value(),
            serde_json::json!(0)
        );
    }
}
