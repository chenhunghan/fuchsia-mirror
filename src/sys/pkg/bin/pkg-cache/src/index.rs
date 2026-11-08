// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Types to track and manage indices of packages.
//!
//! [`PackageIndex`] owns the only lock on these indices. See its documentation for how changes to
//! them are serialized with garbage collection.

mod package;
mod retained;
mod writing;

pub use package::{AddBlobsError, PackageIndex, UpgradablePackagesInitialized};
pub use writing::StopError;
