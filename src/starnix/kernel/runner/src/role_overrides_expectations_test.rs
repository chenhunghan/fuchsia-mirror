// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared role-override expectations test entrypoint packaged via
//! `//src/starnix/build/starnix_container_role_overrides_test.gni`.

use anyhow::Error;

#[fuchsia::test]
fn container_role_overrides_match_expectations() -> Result<(), Error> {
    starnix_role_overrides_expectations::run_from_pkg()
}
