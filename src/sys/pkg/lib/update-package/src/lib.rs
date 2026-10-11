// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![deny(missing_docs)]

//! Typesafe wrappers around an "update" package.

pub mod images;
pub mod manifest;
mod packages;
pub mod signed_manifest;
mod update_mode;
mod version;

pub use crate::images::{
    ImageMetadata, ImageMetadataError, ImagePackagesManifest, ImagePackagesManifestBuilder,
    ImagesMetadata, VerifyError, VersionedImagePackagesManifest, ZbiAndOptionalVbmetaMetadata,
    parse_image_packages_json,
};
pub use crate::packages::{SerializePackageError, parse_packages_json, serialize_packages_json};
pub use crate::signed_manifest::MANIFEST_DEV_KEY_PEM;
pub use crate::update_mode::UpdateMode;
pub use crate::version::SystemVersion;

#[cfg(not(target_os = "fuchsia"))]
pub use crate::images::ImagePackagesError;
#[cfg(not(target_os = "fuchsia"))]
pub use crate::packages::ParsePackageError;
#[cfg(not(target_os = "fuchsia"))]
pub use crate::update_mode::ParseUpdateModeError;

#[cfg(target_os = "fuchsia")]
mod target;
#[cfg(target_os = "fuchsia")]
pub use crate::target::*;
