// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_component_runtime as fruntime;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UtilsError {
    #[error("FIDL error: {0}")]
    Fidl(#[from] fidl::Error),
    #[error("Unexpected capability type. Wanted: {0}, got: {1}")]
    UnexpectedCapabilityType(String, String),
    #[error("Unexpected routed type")]
    UnexpectedRoutedType,
    #[error("fuchsia.component.runtime.Capabilities error: {0:?}")]
    Capabilities(fruntime::CapabilitiesError),
    #[error("Router error: {0:?}")]
    Router(fruntime::RouterError),
    #[error("The handle does not reference a usable dictionary: {0:?}")]
    InvalidDictionary(fruntime::CapabilitiesError),
    #[error("Routing {0} did not produce a capability")]
    Unavailable(String),
    #[error("Failed to connect to fuchsia.component.runtime.Capabilities: {0}")]
    Connect(String),
    #[error("zx error: {0}")]
    ZxStatus(#[from] zx::Status),
}

impl From<fruntime::CapabilitiesError> for UtilsError {
    fn from(err: fruntime::CapabilitiesError) -> Self {
        UtilsError::Capabilities(err)
    }
}

impl From<fruntime::RouterError> for UtilsError {
    fn from(err: fruntime::RouterError) -> Self {
        UtilsError::Router(err)
    }
}

impl From<i32> for UtilsError {
    fn from(err: i32) -> Self {
        UtilsError::ZxStatus(zx::Status::err_from_raw(err))
    }
}
