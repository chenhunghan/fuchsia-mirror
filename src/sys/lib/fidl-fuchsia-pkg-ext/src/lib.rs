// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `fidl_fuchsia_pkg_ext` contains wrapper types around the auto-generated `fidl_fuchsia_pkg`
//! bindings.

#[cfg(target_os = "fuchsia")]
extern crate fidl_fuchsia_pkg;
#[cfg(not(target_os = "fuchsia"))]
extern crate fidl_fuchsia_pkg_common as fidl_fuchsia_pkg;

#[cfg(not(target_os = "fuchsia"))]
use {anyhow as _, fidl as _, futures as _, zx_types as _};

mod types;
pub use crate::types::{BlobId, BlobInfo, CupData, ResolutionContext};

mod repo;
pub use crate::repo::{
    MirrorConfig, MirrorConfigBuilder, RepositoryConfig, RepositoryConfigBuilder,
    RepositoryConfigs, RepositoryError, RepositoryKey, RepositoryRegistrationAliasConflictMode,
    RepositoryStorageType, RepositoryTarget, RepositoryUrl,
};

pub mod errors;
pub use crate::errors::{
    BlobIdParseError, CupMissingField, MirrorConfigError, RepositoryParseError,
    ResolutionContextError, ResolveError,
};

mod measure;
pub use crate::measure::Measurable;

#[cfg(target_os = "fuchsia")]
pub mod base_package_index;
#[cfg(target_os = "fuchsia")]
pub use crate::base_package_index::BasePackageIndex;

#[cfg(target_os = "fuchsia")]
pub mod cache;

#[cfg(target_os = "fuchsia")]
mod serve_fidl_iterator;
#[cfg(target_os = "fuchsia")]
pub use serve_fidl_iterator::{serve_fidl_iterator_from_slice, serve_fidl_iterator_from_stream};

#[cfg(target_os = "fuchsia")]
mod fidl_iterator_to_stream;
#[cfg(target_os = "fuchsia")]
pub use fidl_iterator_to_stream::fidl_iterator_to_stream;
