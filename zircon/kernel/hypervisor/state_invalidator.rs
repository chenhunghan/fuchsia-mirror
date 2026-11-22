// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Hypervisor state invalidation.

/// Allows hypervisor state to be invalidated.
pub trait Invalidate {
    /// Invalidates the cached hypervisor state.
    fn invalidate(&mut self);
}
