// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Typesafe wrappers around parsing the update-mode file.

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use thiserror::Error;

/// An error encountered while parsing an update mode string.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[allow(missing_docs)]
pub enum ParseUpdateModeError {
    #[error("update mode not supported: '{0}'")]
    UpdateModeNotSupported(String),
}

/// Enum to describe the supported update modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateMode {
    /// Follow the normal system update flow.
    #[default]
    Normal,
    /// Instead of the normal flow, write a recovery image and reboot into it.
    ForceRecovery,
}

impl FromStr for UpdateMode {
    type Err = ParseUpdateModeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "normal" => Ok(UpdateMode::Normal),
            "force-recovery" => Ok(UpdateMode::ForceRecovery),
            other => Err(ParseUpdateModeError::UpdateModeNotSupported(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_str() {
        assert_eq!("normal".parse::<UpdateMode>().unwrap(), UpdateMode::Normal);
        assert_eq!("force-recovery".parse::<UpdateMode>().unwrap(), UpdateMode::ForceRecovery);
        assert_eq!(
            "potato".parse::<UpdateMode>().unwrap_err(),
            ParseUpdateModeError::UpdateModeNotSupported("potato".to_string())
        );
    }
}
