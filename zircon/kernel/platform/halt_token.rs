// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

unsafe extern "C" {
    fn cpp_halt_token_take() -> bool;
}

/// This object is used to coordinate concurrent halt/reboot operations.
///
/// The idea is there's a single resource, the "halt token" and only the holder of the token may
/// initiate a halt/reboot (except for panics).
#[derive(Debug)]
pub struct HaltToken(());

impl HaltToken {
    /// Attempts to acquire the global halt token.
    ///
    /// Returns `Some(HaltToken)` if acquired, signaling an irrevocable intention
    /// to halt (or reboot) the system.
    ///
    /// Returns `None` if the token was already acquired by another caller.
    pub fn take() -> Option<Self> {
        // SAFETY: Safe to call from any context when coordinating halt/reboot.
        if unsafe { cpp_halt_token_take() } { Some(Self(())) } else { None }
    }
}
