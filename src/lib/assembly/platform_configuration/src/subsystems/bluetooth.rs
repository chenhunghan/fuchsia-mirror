// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::format_err;
use assembly_constants::BoardFeature;

use crate::subsystems::prelude::*;
use assembly_config_capabilities::{Config, ConfigValueType};
use assembly_config_schema::platform_settings::bluetooth_config::{
    A2dpConfig, A2dpSinkAndSource, A2dpSinkAndSourceConfig, A2dpSinkAndSourceDefaultEnabled,
    A2dpSourceOnly, AudioGatewayConfig, BluetoothConfig, BluetoothProfilesConfig, HandsFreeConfig,
    HfpCodecId, Snoop,
};
use assembly_config_schema::platform_settings::media_config::{AudioConfig, PlatformMediaConfig};

fn get_source_type_str(sink_and_source: &A2dpSinkAndSourceConfig) -> String {
    match sink_and_source {
        A2dpSinkAndSourceConfig::Enabled(A2dpSinkAndSourceDefaultEnabled { enabled: true }) => {
            "audio_out".to_owned()
        }
        A2dpSinkAndSourceConfig::Source(A2dpSourceOnly { source })
        | A2dpSinkAndSourceConfig::SinkAndSource(A2dpSinkAndSource { source, .. }) => {
            serde_json::Value::from(*source).as_str().unwrap().to_string()
        }
        _ => "none".to_owned(),
    }
}

// Common values from BT and media configs used by HFP AG and HF
struct HfpAudioConfig {
    hfp_supported_codecs: Vec<HfpCodecId>,
    controller_encodes: Vec<HfpCodecId>,
    offload_type: String,
}

fn get_hfp_audio_config(
    profiles: &BluetoothProfilesConfig,
    media_config: &PlatformMediaConfig,
) -> anyhow::Result<HfpAudioConfig> {
    // TODO(https://fxbug.dev/362573469): Bail if the features don't make sense
    // (VoiceRecognitionText without EnhancedVoiceRecognitionStatus, for example)
    let hfp_supported_codecs = if profiles.hfp.codecs_supported.is_empty() {
        vec![HfpCodecId::Cvsd, HfpCodecId::Msbc, HfpCodecId::Lc3Swb]
    } else {
        profiles.hfp.codecs_supported.clone()
    };

    let controller_encodes = profiles.hfp.controller_encodes.codecs();

    let offload_type = match media_config.audio {
        Some(AudioConfig::FullStack(_)) => String::from("dai"),
        Some(AudioConfig::DeviceRegistry(_)) => String::from("codec"),
        None => return Err(format_err!("Bluetooth HFP requires an audio stack")),
    };

    Ok(HfpAudioConfig { hfp_supported_codecs, controller_encodes, offload_type })
}

pub(crate) struct BluetoothSubsystemConfig;
impl DefineSubsystemConfiguration<(&BluetoothConfig, &PlatformMediaConfig)>
    for BluetoothSubsystemConfig
{
    fn define_configuration(
        context: &ConfigurationContext<'_>,
        config: &(&BluetoothConfig, &PlatformMediaConfig),
        builder: &mut dyn ConfigurationBuilder,
    ) -> anyhow::Result<()> {
        let (config, media_config) = config;
        // Snoop is only useful when Inspect filtering is turned on. In practice, this is in Eng &
        // UserDebug builds.
        match (context.build_type, config.snoop()) {
            (_, Snoop::None) => {}
            (BuildType::User, _) => return Err(format_err!("Snoop forbidden on user builds")),
            (_, Snoop::Eager) => {
                builder.platform_bundle("bluetooth_snoop_eager")?;
            }
            (_, Snoop::Lazy) => {
                builder.platform_bundle("bluetooth_snoop_lazy")?;
            }
        }

        // Include bt-transport-uart driver through a platform AIB.
        if context.board_config.provides_feature(BoardFeature::BtTransportUart)
            && (*context.feature_set_level == FeatureSetLevel::Standard
                || *context.feature_set_level == FeatureSetLevel::Utility)
        {
            builder.platform_bundle("bt_transport_uart_driver")?;
        }

        let BluetoothConfig::Standard { profiles, core, snoop } = config else {
            return Ok(());
        };

        // Bluetooth Core & Profile packages can only be added to the Standard platform
        // service level.
        if *context.feature_set_level != FeatureSetLevel::Standard {
            return Err(format_err!(
                "Bluetooth core & profiles are forbidden on non-Standard service levels"
            ));
        }
        builder.platform_bundle("bluetooth_core")?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LegacyPairing",
            Config::new(ConfigValueType::Bool, core.legacy_pairing_enabled.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.ScoOffloadPathIndex",
            Config::new(ConfigValueType::Uint8, core.sco_offload_path_index.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.OverrideVendorCapabilitiesVersion",
            Config::new(ConfigValueType::Uint16, core.override_vendor_capabilities_version.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeSlowAdvIntervalMin",
            Config::new(ConfigValueType::Uint16, core.slow_advertising.interval_min.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeSlowAdvIntervalMax",
            Config::new(ConfigValueType::Uint16, core.slow_advertising.interval_max.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeSlowAdvMaxTxPower",
            Config::new(ConfigValueType::Int8, core.slow_advertising.max_tx_power.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeFastAdvIntervalMin",
            Config::new(ConfigValueType::Uint16, core.fast_advertising.interval_min.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeFastAdvIntervalMax",
            Config::new(ConfigValueType::Uint16, core.fast_advertising.interval_max.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeFastAdvMaxTxPower",
            Config::new(ConfigValueType::Int8, core.fast_advertising.max_tx_power.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeVeryFastAdvIntervalMin",
            Config::new(ConfigValueType::Uint16, core.very_fast_advertising.interval_min.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeVeryFastAdvIntervalMax",
            Config::new(ConfigValueType::Uint16, core.very_fast_advertising.interval_max.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeVeryFastAdvMaxTxPower",
            Config::new(ConfigValueType::Int8, core.very_fast_advertising.max_tx_power.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeActiveScanInterval",
            Config::new(ConfigValueType::Uint16, core.scan.active_interval.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeActiveScanWindow",
            Config::new(ConfigValueType::Uint16, core.scan.active_window.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeBatchedScanningEnabled",
            Config::new(ConfigValueType::Bool, core.scan.batched.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeScanBatchMaxReadDelaySeconds",
            Config::new(ConfigValueType::Uint8, core.scan.batch_max_read_delay_seconds.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeScanOffloadFiltersEnabled",
            Config::new(ConfigValueType::Bool, core.scan.offload_filters_enabled.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.HciCommandTimeout",
            Config::new(ConfigValueType::Uint16, core.hci_command_timeout.into()),
        )?;
        // Fast Pair Provider is currently disabled by default.
        // TODO(https://fxbug.dev/253626392): Add a Fast Pair config to the schema and use it here.
        builder.set_config_capability(
            "fuchsia.bluetooth.FastPairProvider",
            Config::new(ConfigValueType::Bool, serde_json::Value::Bool(false)),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.Rfcomm",
            Config::new(ConfigValueType::Bool, profiles.rfcomm.enabled().into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.AutostartSnoop",
            Config::new(ConfigValueType::Bool, (!matches!(snoop, Snoop::None)).into()),
        )?;

        // `bt-gap` config capabilities
        builder.set_config_capability(
            "fuchsia.bluetooth.LePrivacy",
            Config::new(ConfigValueType::Bool, true.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeBackgroundScanning",
            Config::new(ConfigValueType::Bool, false.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.LeSecurityMode",
            Config::new(ConfigValueType::String { max_size: 21 }, "Mode1".into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.BredrConnectable",
            Config::new(ConfigValueType::Bool, core.start_connectable.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.bluetooth.BredrSecurityMode",
            Config::new(ConfigValueType::String { max_size: 21 }, "Mode4".into()),
        )?;

        if profiles.rfcomm.enabled() {
            builder.platform_bundle("bluetooth_rfcomm")?;
        }
        // Bail if RFCOMM is required by any enabled profiles but is not enabled in the schema.
        if profiles.requires_rfcomm() && !profiles.rfcomm.enabled() {
            return Err(format_err!("RFCOMM must be enabled when HFP or MAP are enabled"));
        }

        if let A2dpConfig::Enabled(a2dp) = profiles.a2dp {
            builder.platform_bundle("bluetooth_a2dp")?;

            // Domain, EnableAvrcpTarget, EnableAac, InitiatorDelay, and ChannelMode
            // are not configurable in assembly and use the default values defined in
            // bt-a2dp.cml.
            builder.set_config_capability(
                "fuchsia.bluetooth.EnableSink",
                Config::new(ConfigValueType::Bool, a2dp.sink_enabled().into()),
            )?;
            builder.set_config_capability(
                "fuchsia.bluetooth.SourceType",
                Config::new(
                    ConfigValueType::String { max_size: 10 },
                    get_source_type_str(&a2dp.sink_and_source).into(),
                ),
            )?;
        }
        if profiles.avrcp.enabled {
            builder.platform_bundle("bluetooth_avrcp")?;
        }
        if profiles.did.enabled {
            builder.platform_bundle("bluetooth_device_id")?;
            builder.set_config_capability(
                "fuchsia.bluetooth.VendorId",
                Config::new(ConfigValueType::Uint16, profiles.did.vendor_id.into()),
            )?;
            builder.set_config_capability(
                "fuchsia.bluetooth.ProductId",
                Config::new(ConfigValueType::Uint16, profiles.did.product_id.into()),
            )?;
            builder.set_config_capability(
                "fuchsia.bluetooth.Version",
                Config::new(ConfigValueType::Uint16, profiles.did.version.into()),
            )?;
            builder.set_config_capability(
                "fuchsia.bluetooth.Primary",
                Config::new(ConfigValueType::Bool, profiles.did.primary.into()),
            )?;
            builder.set_config_capability(
                "fuchsia.bluetooth.ServiceDescription",
                Config::new(
                    ConfigValueType::String { max_size: 200 },
                    profiles.did.service_description.clone().unwrap_or(String::new()).into(),
                ),
            )?;
        }

        if let AudioGatewayConfig::Enabled(_) = &profiles.hfp.audio_gateway {
            builder.platform_bundle("bluetooth_hfp_ag")?;
        }
        if let HandsFreeConfig::Enabled(_) = &profiles.hfp.hands_free {
            builder.platform_bundle("bluetooth_hfp_hf")?;
        }
        if matches!(profiles.hfp.audio_gateway, AudioGatewayConfig::Enabled(_))
            || matches!(profiles.hfp.hands_free, HandsFreeConfig::Enabled(_))
        {
            let audio_config = get_hfp_audio_config(profiles, media_config)?;

            // ThreeWayCalling, RejectIncomingVoiceCall, EchoCancelingAndNoiseReduction,
            // AttachPhoneNumberToVoiceTag, EnhancedCallControls, CallWaitingOrThreeWayCalling,
            // CliPresentationCapability, VoiceRecognitionActivation, RemoteVolumeControl,
            // EnhancedVoiceRecognition, and EnhancedVoiceRecognitionWithText are not configurable
            // in assembly and use the default values defined in bt-hfp-audio-gateway.cml and
            // bt-hfp-hands-free.cml.
            builder.set_config_capability(
                "fuchsia.bluetooth.ControllerEncodingCvsd",
                Config::new(
                    ConfigValueType::Bool,
                    audio_config.controller_encodes.contains(&HfpCodecId::Cvsd).into(),
                ),
            )?;
            builder.set_config_capability(
                "fuchsia.bluetooth.ControllerEncodingMsbc",
                Config::new(
                    ConfigValueType::Bool,
                    audio_config.controller_encodes.contains(&HfpCodecId::Msbc).into(),
                ),
            )?;
            builder.set_config_capability(
                "fuchsia.bluetooth.WideBandSpeech",
                Config::new(
                    ConfigValueType::Bool,
                    audio_config.hfp_supported_codecs.contains(&HfpCodecId::Msbc).into(),
                ),
            )?;
            builder.set_config_capability(
                "fuchsia.bluetooth.OffloadType",
                Config::new(
                    ConfigValueType::String { max_size: 8 },
                    audio_config.offload_type.into(),
                ),
            )?;
        }
        if profiles.map.mce_enabled {
            builder.platform_bundle("bluetooth_map_mce")?;
        }

        if *context.feature_set_level == FeatureSetLevel::Standard
            && *context.build_type == BuildType::Eng
        {
            builder.platform_bundle("bluetooth_affordances")?;
            builder.platform_bundle("bluetooth_pandora")?;

            if !profiles.a2dp.enabled()
                && matches!(media_config.audio, Some(AudioConfig::FullStack(_)))
            {
                builder.platform_bundle("bluetooth_a2dp_with_consumer")?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use assembly_config_schema::platform_settings::bluetooth_config::{
        A2dpSinkAndSource, A2dpSinkAndSourceConfig, A2dpSinkAndSourceDefaultEnabled, A2dpSinkOnly,
        A2dpSinkType, A2dpSourceOnly, A2dpSourceType,
    };

    #[test]
    fn test_a2dp_source_type_str() {
        let config =
            A2dpSinkAndSourceConfig::Enabled(A2dpSinkAndSourceDefaultEnabled { enabled: true });
        assert_eq!(get_source_type_str(&config), "audio_out");
        let config =
            A2dpSinkAndSourceConfig::Source(A2dpSourceOnly { source: A2dpSourceType::AudioOut });
        assert_eq!(get_source_type_str(&config), "audio_out");
        let config =
            A2dpSinkAndSourceConfig::Source(A2dpSourceOnly { source: A2dpSourceType::BigBen });
        assert_eq!(get_source_type_str(&config), "big_ben");
        let config = A2dpSinkAndSourceConfig::SinkAndSource(A2dpSinkAndSource {
            source: A2dpSourceType::Offload,
            sink: A2dpSinkType::MediaPlayer,
        });
        assert_eq!(get_source_type_str(&config), "offload");
        let config =
            A2dpSinkAndSourceConfig::Sink(A2dpSinkOnly { sink: A2dpSinkType::MediaPlayer });
        assert_eq!(get_source_type_str(&config), "none");
    }
}
