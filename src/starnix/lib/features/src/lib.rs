// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use strum_macros::EnumString;
use thiserror::Error;

use std::str::FromStr;

/// Features are a way to enable or disable specific starnix behaviors.
///
/// These are specified in the component manifest of the starnix container,
/// or explicitly provided to the kernel constructor in tests.
#[derive(Debug, Clone, Copy, PartialEq, EnumString, strum_macros::Display)]
#[strum(serialize_all = "snake_case")]
pub enum Feature {
    // Legacy module feature flags retained for backwards compatibility with `ExtraFeatures` and
    // Product Assembly. Prefer configuring modules via `program.modules` in container manifests.
    Framebuffer,
    NetworkManager,
    WakeupTest,

    // Container and kernel behavior features.
    AdditionalMounts,
    AndroidBootreason,
    AndroidSerialno,
    Bpf,
    Container,
    CustomArtifacts,
    Devtmpfs,
    EnableSuid,
    ErrorOnFailedReboot,
    // TODO(https://fxbug.dev/485370648) remove when unnecessary
    FakeIon,
    IoUring,
    LogSysfsLookupMisses,
    PingGroupRange,
    RemotevolInProcess,
    RootfsRw,
    Selinux,
    SelinuxTestSuite,
    TestData,
    UnifiedTracing,
    Wifi,
}

/// Parsed [`Feature`] together with its optional raw argument string (`"<feature>:<raw_args>"`).
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureAndArgs {
    pub feature: Feature,
    pub raw_args: Option<String>,
}

/// Error returned when a feature is not recognized.
#[derive(Debug, Error)]
#[error("unsupported feature: {0}")]
pub struct UnsupportedFeatureError(String);

impl Feature {
    /// Parses the name of a feature from a string.
    pub fn try_parse(s: &str) -> Result<Feature, UnsupportedFeatureError> {
        Feature::from_str(s).map_err(|_| UnsupportedFeatureError(s.to_string()))
    }
}

impl FeatureAndArgs {
    /// Parses a feature and optional `:`-delimited argument from a string.
    pub fn try_parse(s: &str) -> Result<Self, UnsupportedFeatureError> {
        let (raw_flag, raw_args) =
            s.split_once(':').map(|(f, a)| (f, Some(a.to_string()))).unwrap_or((s, None));
        let feature = Feature::try_parse(raw_flag)?;
        Ok(Self { feature, raw_args })
    }
}

impl std::fmt::Display for FeatureAndArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.raw_args {
            Some(args) => write!(f, "{}:{}", self.feature, args),
            None => write!(f, "{}", self.feature),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn feature_serde() {
        for (feature, expected_str) in [
            (Feature::AndroidSerialno, "android_serialno"),
            (Feature::AndroidBootreason, "android_bootreason"),
            (Feature::Container, "container"),
            (Feature::CustomArtifacts, "custom_artifacts"),
            (Feature::Devtmpfs, "devtmpfs"),
            (Feature::Framebuffer, "framebuffer"),
            (Feature::NetworkManager, "network_manager"),
            (Feature::Bpf, "bpf"),
            (Feature::EnableSuid, "enable_suid"),
            (Feature::IoUring, "io_uring"),
            (Feature::ErrorOnFailedReboot, "error_on_failed_reboot"),
            (Feature::PingGroupRange, "ping_group_range"),
            (Feature::RootfsRw, "rootfs_rw"),
            (Feature::Selinux, "selinux"),
            (Feature::SelinuxTestSuite, "selinux_test_suite"),
            (Feature::TestData, "test_data"),
            (Feature::Wifi, "wifi"),
            (Feature::AdditionalMounts, "additional_mounts"),
            (Feature::WakeupTest, "wakeup_test"),
            // TODO(https://fxbug.dev/485370648) remove when unnecessary
            (Feature::FakeIon, "fake_ion"),
            (Feature::UnifiedTracing, "unified_tracing"),
            (Feature::RemotevolInProcess, "remotevol_in_process"),
            (Feature::LogSysfsLookupMisses, "log_sysfs_lookup_misses"),
        ] {
            let string = feature.to_string();
            assert_eq!(string.as_str(), expected_str);
            assert_eq!(Feature::try_parse(&string).expect("should parse"), feature);
        }
    }

    #[test]
    fn deserialize_feature_and_args() {
        let FeatureAndArgs { feature, raw_args } =
            FeatureAndArgs::try_parse("bpf:v2").expect("should parse successfully");
        assert_eq!(feature, Feature::Bpf);
        assert_eq!(raw_args.as_ref().expect("should be populated"), "v2");
    }
}
