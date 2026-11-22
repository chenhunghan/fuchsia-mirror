// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Code shared between the `kcounter` tool and its test: command line parsing
//! and read-only views of the kernel counter VMOs.

pub mod cmdline;
#[cfg(target_os = "fuchsia")]
pub mod vmo;

pub use cmdline::{KcounterCmdline, kcounter_parse_cmdline, kcounter_usage};
