// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "512"]

pub mod direct_input;
mod input_device;
mod input_event_relay;
mod input_file;

pub mod test_injection;
pub mod uinput;

pub const RELAY_MINOR_RANGE: std::ops::Range<u32> = 0..64;
pub const DIRECT_INPUT_MINOR_RANGE: std::ops::Range<u32> = 64..128;

pub use direct_input::{ButtonShortcutProcessor, DirectInputManager, query_ownership_event};
pub use input_device::*;
pub use input_event_relay::*;
pub use input_file::*;
pub use test_injection::{InputDeviceRegistry, serve_input_device_registry};

use starnix_core::task::Kernel;
use std::sync::Arc;

/// Helper to obtain or initialize the shared `DirectInputManager` for a `Kernel`.
pub fn get_or_init_direct_input(kernel: &Kernel) -> Arc<DirectInputManager> {
    (*kernel
        .expando
        .get_or_init(|| DirectInputManager::connect_and_start(kernel.weak_self.clone(), None)))
    .clone()
}
